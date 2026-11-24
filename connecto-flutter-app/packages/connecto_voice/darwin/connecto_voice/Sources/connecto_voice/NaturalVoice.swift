// The natural voice: Kyutai's PocketTTS (CC-BY-4.0), run on the device
// through FluidAudio's Core ML port. It streams 24 kHz audio a few frames
// at a time, so speech starts well before the whole reply is synthesized.
// Its model (~330 MB per language) is downloaded the first time a call
// needs it; until it is ready, calls use the system voice.
//
// Which engine runs it (GPU, fp16 or int8 FlowLM) is measured once per
// language on this device, at warm-up: the fastest that speaks faster than
// real time is kept. Where none does, calls use the system voice rather than
// stutter, and it is measured again a day later: one slow measurement (the
// device warm or busy) must not keep the natural voice off for good.

import AVFoundation
import FluidAudio

/// How busy the device is with speech: the on-phone model gives way while
/// speech is synthesized, so the two never fight over the GPU.
enum SpeechLoad {
  private static let count = Locked(0)

  static var busy: Bool { count.value > 0 }

  static func begin() { count.update { $0 += 1 } }
  static func end() { count.update { $0 = max(0, $0 - 1) } }
}

/// One way to run PocketTTS: model placement and FlowLM precision.
enum TtsEngine: String, CaseIterable, Sendable {
  case gpuFp16 = "gpu-fp16"
  case gpuInt8 = "gpu-int8"
  /// The rank-4 Neural Engine models (fp16 only: precision is ignored there).
  /// Not measured any more ([measured]): on an iPhone 14 it was the fastest
  /// but now and then held a vowel on and on, the FlowLM's end of speech
  /// lost to fp16 error on the Neural Engine, as the Mimi decoder's beeps
  /// were. Kept so a stored measurement still reads.
  case aneFp16 = "ane-fp16"

  /// The engines measured, in order.
  static let measured: [TtsEngine] = [.gpuFp16, .gpuInt8]

  var placement: PocketTtsModelPlacement { self == .aneFp16 ? .ane : .gpu }
  var precision: PocketTtsPrecision { self == .gpuInt8 ? .int8 : .fp16 }

  /// FluidAudio's measured routing for the rest, with the Mimi decoder kept
  /// on the CPU: on the Neural Engine its fp16 streaming state turns into
  /// audible beeps.
  static let units = PocketTtsComputeUnits(mimiDecoder: .cpuOnly)
}

@available(iOS 26.0, macOS 26.0, *)
actor NaturalVoice {
  static let shared = NaturalVoice()

  /// Below this, measured at warm-up, the natural voice cannot keep up with
  /// playback here: calls use the system voice.
  static let minimumSpeed = 1.1

  /// The choice meaning "the system voice, always".
  static let system = "system"

  /// Sampling temperature for every language. Kyutai made 0.3 the default
  /// of every shipped model (kyutai-labs/pocket-tts#324); its Italian
  /// figures there are for italian_24l (word errors 20.7% at 0.7, 9.1% at
  /// 0.3), not the 6-layer .italian used here, which is not reported
  /// separately. FluidAudio still defaults to 0.7 for every pack but
  /// English. The temperature only scales the flow decoder's initial noise
  /// (PocketTtsConstants), so the work per frame should not change; speed
  /// at 0.3 has not been measured on a device.
  static let temperature: Float = 0.3

  private static let legacyKey = "connecto.voice.natural"
  private static func choiceKey(_ language: String) -> String { "connecto.voice.choice.\(language)" }
  /// Bumped when what is measured changes, so old measurements are redone
  /// (v2: the Neural Engine left out).
  private static func benchKey(_ pack: PocketTtsLanguage) -> String { "connecto.voice.bench.v2.\(pack.rawValue)" }

  /// How long a too-slow measurement stands before it is redone.
  static let remeasureAfter: TimeInterval = 24 * 60 * 60

  /// The language pack and native voice for [language] (a code such as
  /// "it"), or nil where PocketTTS has none. The native voice was recorded
  /// in the language and sounds most at home there.
  static func pack(for language: String) -> (language: PocketTtsLanguage, voice: String)? {
    switch language {
    case "en": return (.english, "alba")
    case "it": return (.italian, "giovanni")
    case "de": return (.german, "juergen")
    case "es": return (.spanish, "lola")
    case "pt": return (.portuguese, "rafael")
    case "fr": return (.french24L, "estelle")
    default: return nil
    }
  }

  /// The voices offered for [language]: its native one, then a few female
  /// voices every pack has.
  static func voices(for language: String) -> [String] {
    guard let pack = pack(for: language) else { return [] }
    var out = [pack.voice]
    for voice in ["vera", "anna", "eve", "alba"] where !out.contains(voice) { out.append(voice) }
    return out
  }

  /// What calls in [language] speak with: one of [voices], or [system].
  /// The native voice unless the person chose another in Settings.
  static func choice(for language: String) -> String {
    let voices = voices(for: language)
    guard let native = voices.first else { return system }
    if let stored = UserDefaults.standard.string(forKey: choiceKey(language)),
      stored == system || voices.contains(stored)
    {
      return stored
    }
    // Before the picker there was only an on/off switch.
    if UserDefaults.standard.object(forKey: legacyKey) as? Bool == false { return system }
    return native
  }

  static func setChoice(_ choice: String, for language: String) {
    guard choice == system || voices(for: language).contains(choice) else { return }
    UserDefaults.standard.set(choice, forKey: choiceKey(language))
  }

  /// What was measured for [language] here: the engine kept and its speed
  /// (audio seconds per second of work); nil until measured, and again once
  /// a too-slow measurement is [remeasureAfter] old, so it is redone.
  static func measured(for language: String, now: Date = Date()) -> (engine: TtsEngine, speed: Double)? {
    guard let pack = pack(for: language),
      let stored = UserDefaults.standard.dictionary(forKey: benchKey(pack.language)),
      let engine = (stored["engine"] as? String).flatMap(TtsEngine.init(rawValue:)),
      let speed = stored["speed"] as? Double
    else { return nil }
    if speed < minimumSpeed {
      let at = (stored["at"] as? Double).map(Date.init(timeIntervalSince1970:)) ?? .distantPast
      guard now.timeIntervalSince(at) < remeasureAfter else { return nil }
    }
    return (engine, speed)
  }

  /// Measured too slow here: calls use the system voice.
  static func tooSlow(for language: String) -> Bool {
    measured(for: language).map { $0.speed < minimumSpeed } ?? false
  }

  static let sampleRate = Double(PocketTtsConstants.audioSampleRate)

  /// Kept in Application Support, not Caches, so the system does not purge
  /// a model the person waited for; not backed up.
  private static func directory() throws -> URL {
    var url = try FileManager.default.url(
      for: .applicationSupportDirectory, in: .userDomainMask, appropriateFor: nil, create: true
    ).appendingPathComponent("ConnectoVoice", isDirectory: true)
    try FileManager.default.createDirectory(at: url, withIntermediateDirectories: true)
    var values = URLResourceValues()
    values.isExcludedFromBackup = true
    try? url.setResourceValues(values)
    return url
  }

  /// Where [engine]'s models live. Each engine has a folder of its own: the
  /// downloader deletes the other FlowLM precision in the folder it fills,
  /// so a shared one would lose a measured engine's files. GPU fp16 keeps
  /// the folder the voice always used, so nothing is downloaded again.
  private static func directory(for engine: TtsEngine) throws -> URL {
    let root = try directory()
    guard engine != .gpuFp16 else { return root }
    let url = root.appendingPathComponent("engines/\(engine.rawValue)", isDirectory: true)
    try FileManager.default.createDirectory(at: url, withIntermediateDirectories: true)
    return url
  }

  /// Deletes one language pack of one engine ([root], as the downloader
  /// returned it): measured and not kept. Other languages' packs, in the
  /// same engine folder, stay.
  private static func remove(_ root: URL?) {
    guard let root else { return }
    try? FileManager.default.removeItem(at: root)
  }

  /// FluidAudio logs the text it synthesizes at info level; nothing that
  /// is said may reach the logs.
  private static let quiet: Void = {
    AppLogger.minimumLevel = .warning
    AppLogger.mirrorsToConsole = false
  }()

  /// A short sentence in the language, timed at warm-up.
  private static func sample(for language: PocketTtsLanguage) -> String {
    switch language {
    case .italian, .italian24L: return "Ciao, ho controllato il progetto: i test passano tutti e la pagina di accesso funziona."
    case .german, .german24L: return "Hallo, ich habe das Projekt geprüft: alle Tests laufen und die Anmeldeseite funktioniert."
    case .spanish, .spanish24L: return "Hola, he revisado el proyecto: todas las pruebas pasan y la página de acceso funciona."
    case .portuguese, .portuguese24L: return "Olá, verifiquei o projeto: todos os testes passam e a página de entrada funciona."
    case .french24L: return "Bonjour, j'ai vérifié le projet : tous les tests passent et la page de connexion fonctionne."
    default: return "Hi, I checked the project: all the tests pass and the sign-in page works."
    }
  }

  private var manager: PocketTtsManager?
  private var engine: TtsEngine?
  /// The language the latest call asked for: only its load is kept.
  private var wanted: PocketTtsLanguage?
  /// Loads in flight, one per language, shared by everyone waiting for it.
  private var tasks: [PocketTtsLanguage: Task<(PocketTtsManager, TtsEngine)?, Error>] = [:]
  /// A session made ahead of time, on the manager and for the voice it
  /// names: its voice prefill is done, so an utterance pays only for its own
  /// text. At most one; one refill at a time.
  private var spare: (session: PocketTtsSession, manager: ObjectIdentifier, voice: String)?
  private var refilling: (manager: ObjectIdentifier, voice: String)?

  /// The loaded voice for [language] and the engine it runs on, or nil while
  /// it is not ready, is turned off, or is too slow here.
  func ready(for language: String) -> (PocketTtsManager, String, TtsEngine)? {
    let choice = Self.choice(for: language)
    guard choice != Self.system, !Self.tooSlow(for: language), let pack = Self.pack(for: language),
      let manager, let engine, manager.language == pack.language
    else { return nil }
    return (manager, choice, engine)
  }

  /// A session for [voice], its prefill done: the one made ahead of time
  /// when it fits, else a new one. The next is made once this one's work is
  /// over ([refill]), not now: it would compete with this utterance.
  func session(voice: String) async throws -> PocketTtsSession {
    guard let manager else { throw CallError("The natural voice is not loaded.") }
    if let spare, spare.manager == ObjectIdentifier(manager), spare.voice == voice {
      self.spare = nil
      return spare.session
    }
    await dropSpare()
    return try await manager.makeSession(voice: voice, temperature: Self.temperature)
  }

  /// Makes the next utterance's session ahead of time, when none is waiting
  /// for this manager and voice. A session made for a manager that was
  /// replaced meanwhile, or beaten by another, is cancelled.
  func refill(voice: String) async {
    guard let manager else { return }
    let owner = ObjectIdentifier(manager)
    if let spare, spare.manager == owner, spare.voice == voice { return }
    if let refilling, refilling.manager == owner, refilling.voice == voice { return }
    await dropSpare()
    refilling = (owner, voice)
    let made = try? await manager.makeSession(voice: voice, temperature: Self.temperature)
    if let refilling, refilling.manager == owner, refilling.voice == voice { self.refilling = nil }
    guard let made else { return }
    guard let current = self.manager, ObjectIdentifier(current) == owner, spare == nil else {
      await made.cancel()
      return
    }
    spare = (made, owner, voice)
  }

  private func dropSpare() async {
    let old = spare
    spare = nil
    // An abandoned session keeps its task waiting for text until cancelled.
    await old?.session.cancel()
  }

  /// Downloads (when needed) and loads the voice for [language], measuring
  /// the engines the first time, and reports download progress in [0, 1].
  /// Only one language is kept in memory: the one the latest call asked for.
  func prepare(language: String, progress: @escaping @Sendable (Double) -> Void) async throws {
    let voice = Self.choice(for: language)
    guard voice != Self.system, let pack = Self.pack(for: language) else { return }
    wanted = pack.language
    if let manager, manager.language == pack.language {
      await refill(voice: voice)
      return
    }
    let task: Task<(PocketTtsManager, TtsEngine)?, Error>
    if let running = tasks[pack.language] {
      task = running
    } else {
      _ = Self.quiet
      task = Task<(PocketTtsManager, TtsEngine)?, Error> {
        if let (engine, speed) = Self.measured(for: language) {
          // Measured too slow: nothing to load.
          guard speed >= Self.minimumSpeed else { return nil }
          return (try await Self.load(pack.language, engine: engine, progress: progress).manager, engine)
        }
        return try await Self.measure(pack.language, voice: voice, progress: progress)
      }
      tasks[pack.language] = task
    }
    let result = await task.result
    // Only the first waiter to resume owns the result; the others are done.
    guard tasks[pack.language] == task else {
      if case .failure(let error) = result { throw error }
      return
    }
    tasks[pack.language] = nil
    guard let (loaded, engine) = try result.get() else { return }
    // Checked and published in one actor turn, before any await.
    guard wanted == pack.language else {
      // A later call wants another language: this one is let go.
      await loaded.cleanup()
      return
    }
    let old = manager
    manager = loaded
    self.engine = engine
    await dropSpare()
    if let old { await old.cleanup() }
    await refill(voice: voice)
  }

  /// Downloads and loads [language] on [engine], compiled and warmed.
  /// Also where its language pack lies.
  private static func load(
    _ language: PocketTtsLanguage, engine: TtsEngine, progress: (@Sendable (Double) -> Void)?
  ) async throws -> (manager: PocketTtsManager, root: URL) {
    let directory = try directory(for: engine)
    var handler: ProgressHandler?
    if let progress { handler = { progress($0.fractionCompleted) } }
    let root = try await PocketTtsResourceDownloader.ensureModels(
      language: language, directory: directory, precision: engine.precision, placement: engine.placement,
      progressHandler: handler)
    let manager = PocketTtsManager(
      language: language, directory: directory, precision: engine.precision, placement: engine.placement,
      computeUnits: TtsEngine.units)
    // Compiling the models is heavy work: the on-phone model waits.
    SpeechLoad.begin()
    defer { SpeechLoad.end() }
    try await manager.initialize()
    return (manager, root)
  }

  /// Loads [language] on each engine in turn and times a short sentence on
  /// it (after one untimed run that compiles the graphs). Keeps the fastest,
  /// remembers it and its speed; nil when even that is too slow here.
  private static func measure(
    _ language: PocketTtsLanguage, voice: String, progress: @escaping @Sendable (Double) -> Void
  ) async throws -> (PocketTtsManager, TtsEngine)? {
    var best: (manager: PocketTtsManager, engine: TtsEngine, speed: Double, root: URL)?
    var firstError: Error?
    // What the Neural Engine left here when it was still measured.
    if let root = try? directory() {
      try? FileManager.default.removeItem(at: root.appendingPathComponent("engines/\(TtsEngine.aneFp16.rawValue)"))
    }
    for engine in TtsEngine.measured {
      var root: URL?
      do {
        // Only the first download is shown: it is the one a call waits for.
        let (manager, packRoot) = try await load(
          language, engine: engine, progress: best == nil ? progress : nil)
        root = packRoot
        // Only the synthesis holds the device: the downloads above do not.
        SpeechLoad.begin()
        let speed: Double
        do {
          defer { SpeechLoad.end() }
          _ = try await speak(sample: "Ciao.", on: manager, voice: voice)
          speed = try await speak(sample: sample(for: language), on: manager, voice: voice)
        }
        if let current = best, current.speed >= speed {
          await manager.cleanup()
          remove(packRoot)
        } else {
          if let current = best {
            await current.manager.cleanup()
            remove(current.root)
          }
          best = (manager, engine, speed, packRoot)
        }
      } catch {
        firstError = firstError ?? error
        // Loaded but failed: its pack goes. One that never finished
        // downloading is resumed by the downloader next time.
        remove(root)
      }
    }
    guard let best else { throw firstError ?? CallError("The natural voice could not be loaded.") }
    UserDefaults.standard.set(
      ["engine": best.engine.rawValue, "speed": best.speed, "at": Date().timeIntervalSince1970],
      forKey: benchKey(language))
    guard best.speed >= minimumSpeed else {
      await best.manager.cleanup()
      return nil
    }
    return (best.manager, best.engine)
  }

  /// Synthesizes [sample] on [manager] through a session: audio seconds per
  /// second of work.
  private static func speak(sample: String, on manager: PocketTtsManager, voice: String) async throws -> Double {
    let session = try await manager.makeSession(voice: voice, temperature: Self.temperature)
    let started = ContinuousClock.now
    session.enqueue(sample)
    session.finish()
    var samples = 0
    for try await frame in session.frames { samples += frame.samples.count }
    let seconds = (ContinuousClock.now - started) / .seconds(1)
    return seconds > 0 ? Double(samples) / sampleRate / seconds : 0
  }
}
