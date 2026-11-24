// The native half of the call mode: on-device speech recognition
// (SpeechAnalyzer), speech (the natural voice, PocketTTS, where it speaks the
// language and is ready, else AVSpeechSynthesizer; either played through the same
// voice-processing engine, so its echo is cancelled and the person can talk
// over it) and condensing a long reply with the on-device Foundation Models.
// Audio never leaves the device. Nothing here logs what is heard or said.

import AVFoundation
import CoreML
import FluidAudio
import Speech

#if canImport(FoundationModels)
  import FoundationModels
#endif
#if os(iOS)
  import Flutter
  import UIKit
#else
  import FlutterMacOS
#endif

public final class VoiceCallPlugin: NSObject, FlutterPlugin, FlutterStreamHandler {
  private var sink: FlutterEventSink?
  /// A `VoiceCall` while a call runs (typed loosely: it needs OS 26).
  private var active: AnyObject?
  /// A `LiveAudio` while a Gemini Live call runs (ADR 0034).
  private var live: AnyObject?
  private let liveEvents = LiveEvents()

  public static func register(with registrar: FlutterPluginRegistrar) {
    #if os(iOS)
      let messenger = registrar.messenger()
    #else
      let messenger = registrar.messenger
    #endif
    let instance = VoiceCallPlugin()
    registrar.addMethodCallDelegate(
      instance, channel: FlutterMethodChannel(name: "connecto/voice", binaryMessenger: messenger))
    FlutterEventChannel(name: "connecto/voice/events", binaryMessenger: messenger)
      .setStreamHandler(instance)
    FlutterEventChannel(name: "connecto/voice/live", binaryMessenger: messenger)
      .setStreamHandler(instance.liveEvents)
  }

  public func onListen(withArguments _: Any?, eventSink events: @escaping FlutterEventSink)
    -> FlutterError?
  {
    sink = events
    return nil
  }

  public func onCancel(withArguments _: Any?) -> FlutterError? {
    sink = nil
    return nil
  }

  private func emit(_ event: [String: Any]) {
    DispatchQueue.main.async { [weak self] in self?.sink?(event) }
  }

  public func handle(_ call: FlutterMethodCall, result: @escaping FlutterResult) {
    let args = call.arguments as? [String: Any] ?? [:]
    // The Gemini Live settings work on any OS version the app runs on.
    switch call.method {
    case "liveSettings":
      result(["hasKey": LiveKey.read() != nil, "enabled": LiveKey.enabled])
      return
    case "liveKey":
      result(LiveKey.read())
      return
    case "setLiveKey":
      do {
        try LiveKey.write(args["key"] as? String)
        result(nil)
      } catch {
        result(FlutterError(code: "call", message: (error as? CallError)?.message, details: nil))
      }
      return
    case "setLiveEnabled":
      LiveKey.enabled = args["on"] as? Bool ?? false
      result(nil)
      return
    default:
      break
    }
    guard #available(iOS 26.0, macOS 26.0, *) else {
      if call.method == "support" {
        result(["available": false, "reason": "Calls need iOS 26 or macOS 26 or later."])
      } else {
        result(FlutterError(code: "unsupported", message: "Calls need iOS 26 or macOS 26 or later.", details: nil))
      }
      return
    }
    let emit: ([String: Any]) -> Void = { [weak self] in self?.emit($0) }
    // Calls run in order on the main actor; read the session there, so a stop
    // sent right after a start finds it.
    Task { @MainActor in
      let running = self.active as? VoiceCall
      do {
        switch call.method {
        case "support":
          result(await VoiceCall.support())
        case "start":
          (self.active as? VoiceCall)?.stop()
          let session = VoiceCall(emit: emit)
          self.active = session
          do {
            try await session.start(language: args["language"] as? String)
          } catch {
            session.stop()
            if self.active === session { self.active = nil }
            throw error
          }
          result(nil)
        case "stop":
          running?.stop()
          if self.active === running { self.active = nil }
          result(nil)
        case "mute":
          running?.muted = args["muted"] as? Bool ?? false
          result(nil)
        case "speak":
          guard let running else { throw CallError("The call has ended.") }
          let spoken =
            await running.speak(
              args["text"] as? String ?? "", natural: args["natural"] as? Bool ?? true,
              plan: (args["plan"] as? [[Any]] ?? []).compactMap { part -> (String, Double)? in
                guard part.count == 2, let text = part[0] as? String,
                  let hold = (part[1] as? NSNumber)?.doubleValue
                else { return nil }
                return (text, hold)
              })
          result(["completed": spoken.completed, "heard": spoken.heard])
        case "stopSpeaking":
          running?.stopSpeaking()
          result(nil)
        case "pauseSpeaking":
          running?.pauseSpeaking()
          result(nil)
        case "resumeSpeaking":
          running?.resumeSpeaking()
          result(nil)
        case "endTurn":
          running?.endTurn()
          result(nil)
        case "languages":
          result(await VoiceCall.languages())
        case "setLanguage":
          VoiceCall.chosenLanguage = args["language"] as? String
          result(nil)
        case "setVoice":
          if let code = VoiceCall.wantedLocale.language.languageCode?.identifier {
            NaturalVoice.setChoice(args["choice"] as? String ?? NaturalVoice.system, for: code)
          }
          result(nil)
        case "setShowTimings":
          VoiceCall.showTimings = args["on"] as? Bool ?? false
          result(nil)
        case "naturalReady":
          result(await running?.naturalReady() ?? false)
        case "cue":
          running?.cue()
          result(nil)
        case "hold":
          running?.hold(args["on"] as? Bool ?? false)
          result(nil)
        case "clarify":
          let turns = (args["turns"] as? [[Any]] ?? []).compactMap { t -> (Bool, String)? in
            guard t.count == 2, let mine = t[0] as? Bool, let text = t[1] as? String else { return nil }
            return (mine, text)
          }
          let language = args["language"] as? String ?? "en"
          let memory = args["memory"] as? String ?? ""
          // Kept by its number, so a newer turn can call it off.
          let work = Task { @MainActor in await Intake.clarify(turns, language: language, memory: memory) }
          let request = (args["request"] as? NSNumber)?.intValue
          if let request { Intake.running[request] = work }
          let answer = await work.value
          if let request { Intake.running[request] = nil }
          result(Task.isCancelled || work.isCancelled ? nil : answer)
        case "cancelClarify":
          if let request = (args["request"] as? NSNumber)?.intValue { Intake.running[request]?.cancel() }
          result(nil)
        case "phoneModel":
          result(PhoneModel.shared.status())
        case "setPhoneModel":
          PhoneModel.shared.set(on: args["on"] as? Bool ?? false)
          result(PhoneModel.shared.status())
        case "phoneModelPrewarm":
          PhoneModel.shared.prewarm()
          result(nil)
        case "localComplete":
          let messages = (args["messages"] as? [[String]] ?? []).compactMap { m -> (String, String)? in
            m.count == 2 ? (m[0], m[1]) : nil
          }
          let ms = (args["timeoutMs"] as? NSNumber)?.doubleValue ?? 10_000
          result(
            await PhoneModel.shared.complete(
              messages, maxTokens: (args["maxTokens"] as? NSNumber)?.intValue ?? 120,
              temperature: (args["temperature"] as? NSNumber)?.doubleValue ?? 0.3,
              deadline: Date().addingTimeInterval(ms / 1000),
              request: (args["request"] as? NSNumber)?.intValue))
        case "cancelLocalComplete":
          if let request = (args["request"] as? NSNumber)?.intValue { PhoneModel.shared.cancel(request) }
          result(nil)
        case "liveStart":
          (self.live as? LiveAudio)?.stop()
          let audio = LiveAudio(emit: self.liveEvents.emit)
          self.live = audio
          do {
            try await audio.start()
          } catch {
            audio.stop()
            if self.live === audio { self.live = nil }
            throw error
          }
          result(nil)
        case "liveStop":
          (self.live as? LiveAudio)?.stop()
          self.live = nil
          result(nil)
        case "livePlay":
          if let data = args["data"] as? FlutterStandardTypedData { (self.live as? LiveAudio)?.play(data.data) }
          result(nil)
        case "liveFlush":
          (self.live as? LiveAudio)?.flush()
          result(nil)
        case "liveSay":
          guard let audio = self.live as? LiveAudio else { throw CallError("The call has ended.") }
          result(await audio.say(args["text"] as? String ?? "", language: args["language"] as? String))
        case "liveListen":
          guard let audio = self.live as? LiveAudio else { throw CallError("The call has ended.") }
          try await audio.listenForAnswer(
            language: args["language"] as? String, session: (args["session"] as? NSNumber)?.intValue ?? 0)
          result(nil)
        case "liveStopListening":
          (self.live as? LiveAudio)?.stopListening()
          result(nil)
        case "liveMute":
          (self.live as? LiveAudio)?.muted = args["muted"] as? Bool ?? false
          result(nil)
        case "condense":
          result(
            await Condenser.condense(
              args["text"] as? String ?? "", language: args["language"] as? String ?? "en"))
        default:
          result(FlutterMethodNotImplemented)
        }
      } catch {
        result(
          FlutterError(code: "call", message: (error as? CallError)?.message ?? error.localizedDescription, details: nil))
      }
    }
  }
}

struct CallError: Error {
  let message: String
  init(_ message: String) { self.message = message }
}

@available(iOS 26.0, macOS 26.0, *)
@MainActor
final class VoiceCall {
  private let emit: ([String: Any]) -> Void
  private let engine = AVAudioEngine()
  private let player = AVAudioPlayerNode()
  private let synthesizer = AVSpeechSynthesizer()
  private var analyzer: SpeechAnalyzer?
  private var input: AsyncStream<AnalyzerInput>.Continuation?
  private var resultsTask: Task<Void, Never>?
  private var playerFormat: AVAudioFormat?
  private var language = Locale.current.language.languageCode?.identifier ?? "en"
  /// Bumped by every stop, so callbacks of an interrupted utterance are ignored.
  private var utterance = 0
  private var finishSpeaking: ((Bool, String) -> Void)?
  /// The current utterance's text (trimmed), its parts as ranges of it with
  /// the audio made for each so far and whether each is all made, and the
  /// audio played back to the person: what tells how much of it was heard.
  private var spokenText = ""
  private var spokenParts: [SpokenPart] = []
  private var playedBack = 0.0
  /// Some of the system voice's audio could not be played: not said whole.
  private var lostAudio = false
  /// The first part some of whose audio could not be played: nothing from
  /// it on counts as heard, whatever played after it.
  private var firstLostPart: Int?
  private var stopped = false
  private var sessionActive = false
  private var analyzerFormat: AVAudioFormat?
  private var observers: [NSObjectProtocol] = []
  private let tap = TapState()
  /// Hears when the person starts and stops talking, and judges whether a
  /// pause ends their turn.
  private lazy var watch = TurnWatch(emit: emit)
  /// Speech is held for the person who began talking over it.
  private var held = false
  /// Plays the hold tone while the call is on hold.
  private var holdTask: Task<Void, Never>?
  /// Speech is being set up (its voice looked up) and has not begun.
  private var starting = false
  /// Synthesizes the natural voice's current utterance.
  private var naturalTask: Task<Void, Never>?
  /// The last utterance whose first audio was scheduled.
  private var sounded = 0
  /// Downloads and loads the natural voice in the background.
  private var naturalLoad: Task<Void, Never>?

  var muted: Bool {
    get { tap.muted }
    set { tap.muted = newValue }
  }

  init(emit: @escaping ([String: Any]) -> Void) { self.emit = emit }

  /// The language the person chose for calls, kept across launches; nil
  /// for the device's own.
  static var chosenLanguage: String? {
    get { UserDefaults.standard.string(forKey: "connecto.voice.language") }
    set {
      if let newValue, !newValue.isEmpty {
        UserDefaults.standard.set(newValue, forKey: "connecto.voice.language")
      } else {
        UserDefaults.standard.removeObject(forKey: "connecto.voice.language")
      }
    }
  }

  /// The call screen shows how fast speech is made (Settings).
  static var showTimings: Bool {
    get { UserDefaults.standard.bool(forKey: "connecto.voice.timings") }
    set { UserDefaults.standard.set(newValue, forKey: "connecto.voice.timings") }
  }

  static var wantedLocale: Locale { chosenLanguage.map(Locale.init(identifier:)) ?? Locale.current }

  static func name(of id: String) -> String {
    Locale.current.localizedString(forIdentifier: id) ?? id
  }

  /// Whether a call can run here, and what helps it.
  static func support() async -> [String: Any] {
    var summarizer = false
    #if canImport(FoundationModels)
      summarizer = SystemLanguageModel.default.isAvailable
    #endif
    if await transcriber(for: wantedLocale) == nil {
      // Another language may still work: the call page lets the person pick one.
      let any = !(await supportedLocales()).isEmpty
      return [
        "available": any, "summarizer": summarizer,
        "reason": "On-device speech recognition does not support \(name(of: wantedLocale.identifier)).",
      ]
    }
    return ["available": true, "summarizer": summarizer]
  }

  private static func supportedLocales() async -> [Locale] {
    var all = await DictationTranscriber.supportedLocales
    if SpeechTranscriber.isAvailable { all += await SpeechTranscriber.supportedLocales }
    return all
  }

  /// Every language either transcriber knows, by name, with whether its model
  /// is already on the device.
  static func languages() async -> [String: Any] {
    var installed = Set((await DictationTranscriber.installedLocales).map { $0.identifier(.bcp47) })
    if SpeechTranscriber.isAvailable {
      installed.formUnion((await SpeechTranscriber.installedLocales).map { $0.identifier(.bcp47) })
    }
    var seen = Set<String>()
    var list: [[String: Any]] = []
    for locale in await supportedLocales() {
      let id = locale.identifier(.bcp47)
      guard seen.insert(id).inserted else { continue }
      list.append(["id": id, "name": name(of: id), "installed": installed.contains(id)])
    }
    list.sort { ($0["name"] as! String).localizedCompare($1["name"] as! String) == .orderedAscending }
    var answer: [String: Any] = ["languages": list, "current": wantedLocale.identifier(.bcp47)]
    // The natural voices for the current language, the one chosen, and
    // how fast it ran here.
    if let code = wantedLocale.language.languageCode?.identifier, NaturalVoice.pack(for: code) != nil {
      answer["voices"] = NaturalVoice.voices(for: code)
      answer["voice"] = NaturalVoice.choice(for: code)
      if let (engine, speed) = NaturalVoice.measured(for: code) {
        answer["engine"] = engine.rawValue
        answer["speed"] = speed
        answer["tooSlow"] = speed < NaturalVoice.minimumSpeed
      }
    }
    answer["showTimings"] = showTimings
    if let chosen = chosenLanguage { answer["selected"] = chosen }
    return answer
  }

  /// SpeechTranscriber (the newer, faster model) when this device and
  /// language have it, else DictationTranscriber. Support and start both
  /// decide here, so they never disagree.
  static func transcriber(for wanted: Locale) async -> (speech: Bool, locale: Locale)? {
    if SpeechTranscriber.isAvailable, let l = await SpeechTranscriber.supportedLocale(equivalentTo: wanted) {
      return (true, l)
    }
    if let l = await DictationTranscriber.supportedLocale(equivalentTo: wanted) {
      return (false, l)
    }
    return nil
  }

  static func module(speech: Bool, locale: Locale) -> any SpeechModule {
    speech
      ? SpeechTranscriber(
        locale: locale, transcriptionOptions: [],
        reportingOptions: [.volatileResults, .fastResults], attributeOptions: [])
      : DictationTranscriber(
        locale: locale, contentHints: [.shortForm], transcriptionOptions: [.punctuation],
        reportingOptions: [.volatileResults, .frequentFinalization], attributeOptions: [])
  }

  /// Downloads the model [module] needs, if any, reporting progress. An app
  /// may keep only a few languages reserved: the oldest other one is let go
  /// to make room.
  static func install(_ module: any SpeechModule, locale: Locale, emit: @escaping ([String: Any]) -> Void)
    async throws
  {
    let id = locale.identifier(.bcp47)
    let reserved = await AssetInventory.reservedLocales
    if !reserved.contains(where: { $0.identifier(.bcp47) == id }) {
      if reserved.count >= AssetInventory.maximumReservedLocales, let oldest = reserved.first {
        await AssetInventory.release(reservedLocale: oldest)
      }
      try await AssetInventory.reserve(locale: locale)
    }
    guard let request = try await AssetInventory.assetInstallationRequest(supporting: [module]) else { return }
    emit(["type": "download", "language": id, "fraction": 0.0])
    let watch = request.progress.observe(\.fractionCompleted) { progress, _ in
      emit(["type": "download", "language": id, "fraction": progress.fractionCompleted])
    }
    defer { watch.invalidate() }
    try await request.downloadAndInstall()
    emit(["type": "download", "language": id, "fraction": 1.0])
  }

  /// Throws once the call was stopped while start was waiting, after
  /// releasing what start had taken so far.
  private func ensureLive() throws {
    if stopped {
      teardown()
      throw CallError("The call has ended.")
    }
  }

  func start(language requested: String?) async throws {
    do {
      try await begin(language: requested)
    } catch {
      teardown()
      throw error
    }
  }

  private func begin(language requested: String?) async throws {
    let wanted = requested.map(Locale.init(identifier:)) ?? Self.wantedLocale
    try await Self.permissions()
    try ensureLive()
    guard let (speech, locale) = await Self.transcriber(for: wanted) else {
      throw CallError("On-device speech recognition does not support \(Self.name(of: wanted.identifier)).")
    }
    try ensureLive()
    language = locale.language.languageCode?.identifier ?? language

    let module = Self.module(speech: speech, locale: locale)
    if let transcriber = module as? SpeechTranscriber {
      listen(to: transcriber.results)
    } else if let transcriber = module as? DictationTranscriber {
      listen(to: transcriber.results)
    }
    try await Self.install(module, locale: locale, emit: emit)
    try ensureLive()
    guard let format = await SpeechAnalyzer.bestAvailableAudioFormat(compatibleWith: [module]) else {
      throw CallError("Speech recognition has no audio format for this device.")
    }
    try ensureLive()
    analyzerFormat = format

    try configureSession()
    // Voice processing (echo cancellation, noise suppression) for both I/O
    // nodes; it can only be switched on while the engine is stopped.
    try engine.inputNode.setVoiceProcessingEnabled(true)
    engine.attach(player)

    let (stream, continuation) = AsyncStream<AnalyzerInput>.makeStream(bufferingPolicy: .bufferingNewest(256))
    input = continuation
    let analyzer = SpeechAnalyzer(modules: [module], options: .init(priority: .userInitiated, modelRetention: .lingering))
    self.analyzer = analyzer
    try await analyzer.prepareToAnalyze(in: format)
    try ensureLive()
    try await analyzer.start(inputSequence: stream)
    try ensureLive()

    try connectMicrophone()
    observe()
    // Load the models now, not when the first turn or long reply is due.
    TurnWatch.prepare()
    prepareNaturalVoice()
    Intake.prewarm(language: language)
    Condenser.prewarm(language: language)
    emit(["type": "listening"])
  }

  /// Taps the microphone in its current format and (re)starts the engine.
  private func connectMicrophone() throws {
    guard let format = analyzerFormat, let input else { return }
    let micFormat = engine.inputNode.outputFormat(forBus: 0)
    guard micFormat.sampleRate > 0 else { throw CallError("No microphone is available.") }
    engine.inputNode.removeTap(onBus: 0)
    tap.configure(from: micFormat, to: format, continuation: input, watch: watch)
    let state = tap
    engine.inputNode.installTap(onBus: 0, bufferSize: 2048, format: micFormat) { buffer, _ in
      state.feed(buffer)
    }
    engine.prepare()
    try engine.start()
  }

  /// A new route (headset in or out) stops the engine and may change the
  /// microphone's format: tap again and go on. An interruption (a phone
  /// call, Siri) ends the call.
  private func observe() {
    let center = NotificationCenter.default
    observers.append(
      center.addObserver(forName: .AVAudioEngineConfigurationChange, object: engine, queue: .main) {
        [weak self] _ in
        MainActor.assumeIsolated {
          guard let self, !self.stopped else { return }
          self.stopSpeaking()
          do {
            try self.connectMicrophone()
          } catch {
            self.emit(["type": "error", "message": "The audio device changed and the call could not go on."])
          }
        }
      })
    #if os(iOS)
      observers.append(
        center.addObserver(forName: AVAudioSession.interruptionNotification, object: nil, queue: .main) {
          [weak self] note in
          let began =
            (note.userInfo?[AVAudioSessionInterruptionTypeKey] as? UInt)
            .flatMap(AVAudioSession.InterruptionType.init(rawValue:)) == .began
          MainActor.assumeIsolated {
            guard began, let self, !self.stopped else { return }
            self.emit(["type": "error", "message": "The call was interrupted."])
          }
        })
    #endif
  }

  private static func permissions() async throws {
    #if os(iOS)
      let mic = await AVAudioApplication.requestRecordPermission()
    #else
      let mic = await AVCaptureDevice.requestAccess(for: .audio)
    #endif
    // SpeechAnalyzer runs on the device; SFSpeechRecognizer's separate
    // authorization does not apply to it.
    guard mic else { throw CallError("Allow microphone access in Settings to make calls.") }
  }

  private func configureSession() throws {
    #if os(iOS)
      let session = AVAudioSession.sharedInstance()
      try session.setCategory(
        .playAndRecord, mode: .voiceChat,
        options: [.defaultToSpeaker, .allowBluetoothHFP, .allowBluetoothA2DP])
      try session.setActive(true)
      sessionActive = true
    #endif
  }

  private func listen<S: AsyncSequence & Sendable>(to results: S) where S.Element: SpeechModuleResult {
    resultsTask = Task { @MainActor [weak self] in
      do {
        for try await result in results {
          guard let self else { return }
          let text: String
          if let r = result as? SpeechTranscriber.Result {
            text = String(r.text.characters)
          } else if let r = result as? DictationTranscriber.Result {
            text = String(r.text.characters)
          } else {
            continue
          }
          // The audio range lets the app tell a late final for audio it has
          // already acted on from new speech.
          var event: [String: Any] = ["type": result.isFinal ? "final" : "partial", "text": text]
          let start = result.range.start.seconds, end = result.range.end.seconds
          if start.isFinite, end.isFinite {
            event["start"] = start
            event["end"] = end
          }
          self.emit(event)
        }
      } catch {
        if let self, !self.stopped {
          self.emit(["type": "error", "message": "Speech recognition stopped: \(error.localizedDescription)"])
        }
      }
    }
  }

  // MARK: Speaking

  /// Whether the natural voice could speak now. The app asks once, at a
  /// call's first words, and keeps that voice for the whole call.
  func naturalReady() async -> Bool {
    await NaturalVoice.shared.ready(for: language) != nil
  }

  /// Speaks [text] through the engine; true when it played to the end, false
  /// when it was stopped (barge-in, hang-up). The natural voice only when
  /// [natural] (the call's voice) and ready, said by the parts of [plan]:
  /// each `(text, hold)` plays once `hold` seconds of it are made, or once
  /// it is all made (`hold` < 0), so a slow device pauses only between
  /// sentences, never inside one.
  func speak(_ text: String, natural wantsNatural: Bool, plan: [(String, Double)]) async -> (
    completed: Bool, heard: String
  ) {
    stopSpeaking()
    let trimmed = text.trimmingCharacters(in: .whitespacesAndNewlines)
    guard !trimmed.isEmpty, !stopped else { return (true, trimmed) }
    utterance += 1
    let mine = utterance
    spokenText = trimmed
    spokenParts = [SpokenPart(range: trimmed.startIndex..<trimmed.endIndex)]
    playedBack = 0
    lostAudio = false
    firstLostPart = nil
    watch.playing = true
    // A pause that comes while the voice is looked up holds what follows.
    starting = true
    let natural = wantsNatural ? await NaturalVoice.shared.ready(for: language) : nil
    // Stopped or replaced while the voice was looked up.
    guard mine == utterance, !stopped else { return (false, "") }
    starting = false
    return await withCheckedContinuation { done in
      finishSpeaking = { done.resume(returning: ($0, $1)) }
      if let (_, voice, engine) = natural {
        let parts = plan.filter { !$0.0.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty }
        speakNaturally(
          trimmed, parts: parts.isEmpty ? [(trimmed, 0)] : parts, voice: voice, engine: engine, utterance: mine)
      } else {
        speakSystem(trimmed, utterance: mine)
      }
    }
  }

  /// The system voice: AVSpeechSynthesizer writes buffers that the engine plays.
  private func speakSystem(_ text: String, utterance mine: Int) {
    // Said whole, in one part (also where the natural voice failed).
    if mine == utterance {
      spokenParts = [SpokenPart(range: spokenText.startIndex..<spokenText.endIndex)]
      playedBack = 0
      lostAudio = false
      firstLostPart = nil
    }
    let speech = AVSpeechUtterance(string: text)
    speech.voice = Self.voice(for: language)
    speech.prefersAssistiveTechnologySettings = true
    synthesizer.write(speech) { [weak self] buffer in
      guard let pcm = buffer as? AVAudioPCMBuffer else { return }
      DispatchQueue.main.async {
        MainActor.assumeIsolated {
          guard let self else { return }
          if mine == self.utterance, !self.spokenParts.isEmpty {
            // The end marker: the text is all made.
            if pcm.frameLength == 0 {
              self.spokenParts[0].done = true
            } else {
              self.spokenParts[0].made += Double(pcm.frameLength) / pcm.format.sampleRate
            }
          }
          // Audio that could not be played: the utterance is not said whole.
          if !self.play(pcm, utterance: mine), pcm.frameLength > 0, mine == self.utterance {
            self.lostAudio = true
            self.firstLostPart = self.firstLostPart ?? 0
          }
        }
      }
    }
  }

  /// The natural voice: PocketTTS frames (80 ms each), from a session whose
  /// voice prefill was done ahead of time. Each part is enqueued in order;
  /// its frames are held back until its hold is made (or, with a negative
  /// hold, until the part is all made), then play as they come. Its speed
  /// (audio seconds per second of work) and time to first sound are
  /// reported when it ends. If it fails before saying anything, the system
  /// voice says it instead.
  private func speakNaturally(
    _ text: String, parts: [(String, Double)], voice: String, engine: TtsEngine, utterance mine: Int
  ) {
    naturalTask = Task { @MainActor [weak self] in
      var played = 0
      var dropped = false
      var held: [AVAudioPCMBuffer] = []
      var heldSeconds = 0.0
      var part = 0
      var releasedPart = false
      var samples = 0
      let started = ContinuousClock.now
      var lastFrame = started
      var firstSound: ContinuousClock.Instant?
      var session: PocketTtsSession?
      /// Plays what is held, then [buffer]; false once this utterance is over.
      @MainActor func release(_ buffer: AVAudioPCMBuffer?) -> Bool {
        guard let self else { return false }
        for pcm in held + (buffer.map { [$0] } ?? []) {
          if self.play(pcm, utterance: mine) {
            played += 1
            if firstSound == nil { firstSound = .now }
          } else {
            dropped = true
            if mine == self.utterance { self.firstLostPart = self.firstLostPart ?? part }
          }
        }
        held.removeAll()
        heldSeconds = 0
        return mine == self.utterance && !self.stopped
      }
      do {
        let made = try await NaturalVoice.shared.session(voice: voice)
        session = made
        guard let self, mine == self.utterance, !self.stopped else {
          await made.cancel()
          return
        }
        self.spokenParts = SpokenPart.parts(of: self.spokenText, as: parts.map(\.0))
        // Speech has the GPU: the on-phone model waits.
        SpeechLoad.begin()
        defer { SpeechLoad.end() }
        for (piece, _) in parts { made.enqueue(piece) }
        made.finish()
        for try await frame in made.frames {
          guard mine == self.utterance, !self.stopped else { break }
          // A frame of the next part: the last one is all made.
          let index = frame.utteranceIndex ?? part
          if index > part {
            for done in part..<min(index, self.spokenParts.count) { self.spokenParts[done].done = true }
            if !releasedPart, !release(nil) { break }
            part = index
            releasedPart = false
          }
          if frame.samples.isEmpty { continue }
          samples += frame.samples.count
          if part < self.spokenParts.count {
            self.spokenParts[part].made += Double(frame.samples.count) / NaturalVoice.sampleRate
          }
          lastFrame = .now
          guard let pcm = Self.pcm(frame.samples) else {
            dropped = true
            self.firstLostPart = self.firstLostPart ?? part
            continue
          }
          if !releasedPart {
            held.append(pcm)
            heldSeconds += Double(frame.samples.count) / NaturalVoice.sampleRate
            let hold = part < parts.count ? parts[part].1 : 0
            guard hold >= 0, heldSeconds >= hold else { continue }
            releasedPart = true
            if !release(nil) { break }
            continue
          }
          if !release(pcm) { break }
        }
        guard mine == self.utterance, !self.stopped else {
          await made.cancel()
          return
        }
        for done in self.spokenParts.indices { self.spokenParts[done].done = true }
        _ = release(nil)
        self.report(samples: samples, from: started, to: lastFrame, firstSound: firstSound, engine: engine, voice: voice)
        if played == 0 {
          // Nothing could be played: the system voice says it.
          self.speakSystem(text, utterance: mine)
        } else {
          self.finishAfterPlayback(utterance: mine, completed: !dropped)
        }
      } catch {
        await session?.cancel()
        guard let self, mine == self.utterance, !self.stopped else { return }
        _ = release(nil)
        if played == 0 {
          self.speakSystem(text, utterance: mine)
        } else {
          // Part of the reply was lost: it was not said to the end.
          self.finishAfterPlayback(utterance: mine, completed: false)
        }
      }
      // The next utterance's session, now that this one's work is done.
      Task { await NaturalVoice.shared.refill(voice: voice) }
    }
  }

  /// How the natural voice did on one utterance: its speed (audio seconds
  /// per second of synthesis), when it was first heard, and on what.
  private func report(
    samples: Int, from started: ContinuousClock.Instant, to last: ContinuousClock.Instant,
    firstSound: ContinuousClock.Instant?, engine: TtsEngine, voice: String
  ) {
    let audio = Double(samples) / NaturalVoice.sampleRate
    let work = (last - started) / .seconds(1)
    guard audio > 0, work > 0 else { return }
    var event: [String: Any] = [
      "type": "timing", "speed": audio / work, "audio": audio, "engine": engine.rawValue, "voice": voice,
    ]
    if let firstSound { event["firstAudioMs"] = Int((firstSound - started) / .milliseconds(1)) }
    emit(event)
  }

  private static func pcm(_ samples: [Float]) -> AVAudioPCMBuffer? {
    guard !samples.isEmpty,
      let format = AVAudioFormat(standardFormatWithSampleRate: NaturalVoice.sampleRate, channels: 1),
      let buffer = AVAudioPCMBuffer(pcmFormat: format, frameCapacity: AVAudioFrameCount(samples.count)),
      let channel = buffer.floatChannelData?[0]
    else { return nil }
    buffer.frameLength = AVAudioFrameCount(samples.count)
    samples.withUnsafeBufferPointer { channel.update(from: $0.baseAddress!, count: samples.count) }
    return buffer
  }

  /// Gets the natural voice ready for the call's language without holding up
  /// the call: until it is, the system voice speaks.
  private func prepareNaturalVoice() {
    guard NaturalVoice.choice(for: language) != NaturalVoice.system else { return }
    let language = language
    naturalLoad = Task { [weak self] in
      // Report the download (only when there is one) in steps of 5%.
      let shown = Locked(-1)
      do {
        try await NaturalVoice.shared.prepare(language: language) { [weak self] fraction in
          guard fraction < 1 else { return }
          let step = Int(fraction * 20)
          guard shown.swap(step) != step else { return }
          Task { @MainActor in self?.notice("Downloading the natural voice… \(step * 5)%") }
        }
        if shown.value >= 0 { await self?.notice("") }
      } catch {
        if shown.value >= 0 {
          await self?.notice("The natural voice could not be downloaded; using the system voice.")
        }
      }
    }
  }

  /// A notice on the call screen, while this call is on ("" clears it). The
  /// download goes on after a call ends; a later call must not hear of it.
  private func notice(_ text: String) {
    guard !stopped else { return }
    emit(["type": "status", "text": text])
  }

  /// Schedules [buffer]; true when it was queued to be heard.
  @discardableResult
  private func play(_ buffer: AVAudioPCMBuffer, utterance mine: Int) -> Bool {
    guard mine == utterance, !stopped else { return false }
    guard engine.isRunning else {
      // Ends the utterance for good: nothing may speak it after this.
      stopSpeaking()
      return false
    }
    // The system voice's end marker.
    if buffer.frameLength == 0 {
      finishAfterPlayback(utterance: mine, completed: !lostAudio)
      return false
    }
    if playerFormat == nil {
      engine.connect(player, to: engine.mainMixerNode, format: buffer.format)
      playerFormat = buffer.format
    }
    guard let format = playerFormat, let ready = Convert.buffer(buffer, to: format) else { return false }
    // Counted once heard: a buffer cut by a stop (the utterance is then
    // already over) or never reached does not count.
    let seconds = Double(ready.frameLength) / ready.format.sampleRate
    player.scheduleBuffer(ready, completionCallbackType: .dataPlayedBack) { [weak self] _ in
      DispatchQueue.main.async {
        MainActor.assumeIsolated {
          guard let self, mine == self.utterance else { return }
          self.playedBack += seconds
        }
      }
    }
    if !player.isPlaying && !held { player.play() }
    // The first audio of each utterance, for the timings on screen.
    if sounded != mine {
      sounded = mine
      emit(["type": "firstSound"])
    }
    return true
  }

  /// Finishes the utterance once everything scheduled has been heard.
  private func finishAfterPlayback(utterance mine: Int, completed: Bool) {
    guard mine == utterance, !stopped else { return }
    guard engine.isRunning else { return stopSpeaking() }
    guard let format = playerFormat, let silence = AVAudioPCMBuffer(pcmFormat: format, frameCapacity: 1)
    else { return finish(completed) }
    silence.frameLength = 1
    player.scheduleBuffer(silence, completionCallbackType: .dataPlayedBack) { [weak self] _ in
      DispatchQueue.main.async {
        MainActor.assumeIsolated {
          guard let self, mine == self.utterance else { return }
          self.finish(completed)
        }
      }
    }
    // A stopped player would never play the marker; a held one resumes it.
    if !player.isPlaying && !held { player.play() }
  }

  private func finish(_ completed: Bool) {
    let done = finishSpeaking
    finishSpeaking = nil
    watch.playing = false
    let whole = spokenParts.prefix(firstLostPart ?? spokenParts.count)
    done?(completed, completed ? spokenText : SpokenPart.heard(of: spokenText, parts: Array(whole), played: playedBack))
  }

  /// Holds the speech where it is: what is still coming is queued behind it.
  func pauseSpeaking() {
    guard finishSpeaking != nil || starting, !held else { return }
    held = true
    if player.engine != nil, player.isPlaying { player.pause() }
  }

  func resumeSpeaking() {
    guard held else { return }
    held = false
    if finishSpeaking != nil, engine.isRunning, player.engine != nil { player.play() }
  }

  /// The app took the person's turn: judge the next one on its own audio.
  func endTurn() { watch.reset() }

  /// Two short soft notes saying the turn was heard and sent. Played through
  /// the same player as speech, so echo cancellation removes it; it does not
  /// count as an utterance and the next speech cuts it off.
  func cue() {
    tone([(660, 0.07), (880, 0.09)], gain: 0.12)
  }

  /// While on, a light, low two-note tone every few seconds, skipped while
  /// something is being said: the call is on hold, not dropped.
  func hold(_ on: Bool) {
    holdTask?.cancel()
    holdTask = nil
    guard on, !stopped else { return }
    holdTask = Task { @MainActor [weak self] in
      // Leave room for the phrase that puts the call on hold.
      try? await Task.sleep(for: .seconds(1.5))
      while !Task.isCancelled {
        self?.holdTick()
        try? await Task.sleep(for: .seconds(3.5))
      }
    }
  }

  private func holdTick() {
    guard !stopped, finishSpeaking == nil, !starting, !synthesizer.isSpeaking else { return }
    guard !held else { return }
    tone([(523.25, 0.14), (392.0, 0.2)], gain: 0.05)
  }

  /// Plays [notes] (a sine each, with soft edges) through the speech player.
  private func tone(_ notes: [(hz: Double, seconds: Double)], gain: Double) {
    guard engine.isRunning, !stopped else { return }
    let rate = 24_000.0
    guard let tone = AVAudioFormat(standardFormatWithSampleRate: rate, channels: 1) else { return }
    if playerFormat == nil {
      engine.connect(player, to: engine.mainMixerNode, format: tone)
      playerFormat = tone
    }
    let frames = AVAudioFrameCount(notes.reduce(0) { $0 + $1.seconds } * rate)
    guard let buffer = AVAudioPCMBuffer(pcmFormat: tone, frameCapacity: frames),
      let samples = buffer.floatChannelData?[0]
    else { return }
    var at = 0
    for note in notes {
      let count = Int(note.seconds * rate)
      for i in 0..<count {
        // A sine with a short raised-cosine fade at both ends: no clicks.
        let t = Double(i) / rate
        let edge = min(1, Double(min(i, count - 1 - i)) / (0.012 * rate))
        let envelope = 0.5 - 0.5 * cos(Double.pi * edge)
        samples[at + i] = Float(gain * envelope * sin(2 * Double.pi * note.hz * t))
      }
      at += count
    }
    buffer.frameLength = AVAudioFrameCount(at)
    guard let format = playerFormat, let ready = Convert.buffer(buffer, to: format) else { return }
    player.scheduleBuffer(ready)
    if !player.isPlaying { player.play() }
  }

  func stopSpeaking() {
    held = false
    utterance += 1
    starting = false
    naturalTask?.cancel()
    naturalTask = nil
    if synthesizer.isSpeaking { synthesizer.stopSpeaking(at: .immediate) }
    if player.engine != nil { player.stop() }
    finish(false)
  }

  /// The person's Personal Voice when they allowed it, else the best installed
  /// voice for the language.
  private static func voice(for language: String) -> AVSpeechSynthesisVoice? {
    let voices = AVSpeechSynthesisVoice.speechVoices().filter {
      $0.language.hasPrefix(language)
    }
    if AVSpeechSynthesizer.personalVoiceAuthorizationStatus == .authorized,
      let personal = voices.first(where: { $0.voiceTraits.contains(.isPersonalVoice) })
    {
      return personal
    }
    let preferred = Locale.current.language.languageCode?.identifier == language
      ? AVSpeechSynthesisVoice(language: Locale.current.identifier.replacingOccurrences(of: "_", with: "-"))
      : nil
    let ordinary = voices.filter { !$0.voiceTraits.contains(.isNoveltyVoice) && !$0.voiceTraits.contains(.isPersonalVoice) }
    return ordinary.max {
      ($0.quality.rawValue, $0.language == preferred?.language ? 1 : 0)
        < ($1.quality.rawValue, $1.language == preferred?.language ? 1 : 0)
    } ?? AVSpeechSynthesisVoice(language: language)
  }

  // MARK: Ending

  func stop() {
    stopped = true
    teardown()
  }

  /// Releases everything start took; safe to call more than once.
  private func teardown() {
    holdTask?.cancel()
    holdTask = nil
    // The download goes on in NaturalVoice for the next call; only this
    // call's progress reports stop.
    naturalLoad?.cancel()
    naturalLoad = nil
    for observer in observers { NotificationCenter.default.removeObserver(observer) }
    observers = []
    stopSpeaking()
    tap.close()
    if engine.isRunning { engine.stop() }
    engine.inputNode.removeTap(onBus: 0)
    input?.finish()
    input = nil
    resultsTask?.cancel()
    if let analyzer {
      Task { await analyzer.cancelAndFinishNow() }
    }
    analyzer = nil
    #if os(iOS)
      if sessionActive {
        sessionActive = false
        try? AVAudioSession.sharedInstance().setActive(false, options: .notifyOthersOnDeactivation)
      }
    #endif
  }
}

/// What the audio thread's tap needs, behind a lock.
@available(iOS 26.0, macOS 26.0, *)
final class TapState: @unchecked Sendable {
  private let lock = NSLock()
  private var converter: AVAudioConverter?
  private var target: AVAudioFormat?
  private var continuation: AsyncStream<AnalyzerInput>.Continuation?
  private var watch: TurnWatch?
  private var isMuted = false

  var muted: Bool {
    get { lock.withLock { isMuted } }
    set { lock.withLock { isMuted = newValue } }
  }

  func configure(
    from source: AVAudioFormat, to target: AVAudioFormat, continuation: AsyncStream<AnalyzerInput>.Continuation,
    watch: TurnWatch
  ) {
    watch.configure(from: source)
    lock.withLock {
      self.converter = source == target ? nil : AVAudioConverter(from: source, to: target)
      self.target = target
      self.continuation = continuation
      self.watch = watch
    }
  }

  func feed(_ buffer: AVAudioPCMBuffer) {
    lock.withLock {
      guard !isMuted, let continuation, let target else { return }
      let ready: AVAudioPCMBuffer?
      if let converter {
        ready = Convert.buffer(buffer, with: converter, to: target)
      } else {
        ready = buffer
      }
      if let ready { continuation.yield(AnalyzerInput(buffer: ready)) }
      // The same audio, so its clock matches the recognizer's.
      watch?.feed(buffer)
    }
  }

  func close() {
    lock.withLock {
      continuation = nil
      converter = nil
      watch = nil
    }
  }
}


/// Listens to the echo-cancelled microphone on the audio thread: says when
/// the person starts and stops talking (`activity`), and when they pause,
/// how likely their turn is over (`turn`), from Smart Turn v3.2 run on the
/// last 8 seconds of the turn. It hears how something is said (intonation,
/// pace), not only the words, so "vorrei che il login..." with a pause to
/// think is not taken for the end. Nothing is kept beyond the turn.
@available(iOS 26.0, macOS 26.0, *)
final class TurnWatch: @unchecked Sendable {
  static let rate = 16_000.0
  static let window = 128_000  // 8 s, the model's input
  private static let frame = 320  // 20 ms

  private let emit: ([String: Any]) -> Void
  private let lock = NSLock()
  private let queue = DispatchQueue(label: "connecto.voice.turn", qos: .userInitiated)
  private var converter: AVAudioConverter?
  private let format = AVAudioFormat(
    commonFormat: .pcmFormatFloat32, sampleRate: TurnWatch.rate, channels: 1, interleaved: false)!

  /// The turn's audio, at most the model's window; [pending] is what has
  /// not yet made a whole frame.
  private var turn: [Float] = []
  private var pending: [Float] = []
  /// Seconds of audio heard, the recognizer's clock.
  private var heard = 0.0
  private var speaking = false
  private var above = 0.0, below = 0.0
  /// The room's level, in dBFS, followed slowly.
  private var floor: Float = -60
  private var isPlaying = false

  /// The call is speaking: its echo is mostly cancelled, but the person's
  /// voice must stand out more to count.
  var playing: Bool {
    get { lock.withLock { isPlaying } }
    set { lock.withLock { isPlaying = newValue } }
  }

  init(emit: @escaping ([String: Any]) -> Void) { self.emit = emit }

  func configure(from source: AVAudioFormat) {
    lock.withLock { converter = AVAudioConverter(from: source, to: format) }
  }

  func reset() {
    lock.withLock { turn.removeAll(keepingCapacity: true) }
  }

  /// Audio thread.
  func feed(_ buffer: AVAudioPCMBuffer) {
    lock.withLock {
      guard let converter, let mono = Convert.buffer(buffer, with: converter, to: format),
        let data = mono.floatChannelData?[0]
      else { return }
      pending.append(contentsOf: UnsafeBufferPointer(start: data, count: Int(mono.frameLength)))
      var at = 0
      while pending.count - at >= Self.frame {
        step(pending[at..<at + Self.frame])
        at += Self.frame
      }
      pending.removeFirst(at)
    }
  }

  /// One 20 ms frame: follows the level, with hysteresis so a breath or a
  /// click is not a voice and a short gap is not an end.
  private func step(_ frame: ArraySlice<Float>) {
    turn.append(contentsOf: frame)
    if turn.count > Self.window { turn.removeFirst(turn.count - Self.window) }
    let seconds = Double(Self.frame) / Self.rate
    heard += seconds
    var sum: Float = 0
    for x in frame { sum += x * x }
    let level = 10 * log10(sum / Float(frame.count) + 1e-12)
    // Down fast, up slowly (about 1 dB a second): speech never becomes the
    // floor, a noisier room does.
    floor = level < floor ? floor * 0.9 + level * 0.1 : floor + 0.02
    floor = min(max(floor, -80), -38)
    let margin: Float = isPlaying ? 20 : 14
    if !speaking {
      above = level > max(-44, floor + margin) ? above + seconds : max(0, above - seconds)
      if above >= (isPlaying ? 0.2 : 0.12) {
        speaking = true
        below = 0
        emit(["type": "activity", "speaking": true])
      }
    } else {
      below = level < max(-50, floor + margin - 6) ? below + seconds : 0
      if below >= 0.25 {
        speaking = false
        above = 0
        emit(["type": "activity", "speaking": false])
        judge(turn, pausedAt: heard - below)
      }
    }
  }

  private func judge(_ audio: [Float], pausedAt: Double) {
    let emit = self.emit
    queue.async {
      guard let complete = Self.complete(audio) else { return }
      emit(["type": "turn", "complete": complete, "at": pausedAt])
    }
  }

  // MARK: The model

  private static let modelLock = NSLock()
  nonisolated(unsafe) private static var model: MLModel?
  nonisolated(unsafe) private static var loading = false

  /// Loads the model off the main thread, once; until then no estimate is
  /// given and the app waits a fixed silence.
  static func prepare() {
    let start = modelLock.withLock { () -> Bool in
      if model != nil || loading { return false }
      loading = true
      return true
    }
    guard start else { return }
    DispatchQueue.global(qos: .userInitiated).async {
      let config = MLModelConfiguration()
      config.computeUnits = .cpuAndNeuralEngine
      let loaded = Bundle.module.url(forResource: "smart_turn", withExtension: "mlmodelc")
        .flatMap { try? MLModel(contentsOf: $0, configuration: config) }
      modelLock.withLock {
        model = loaded
        loading = false
      }
    }
  }

  /// The probability that [audio] (16 kHz, the turn so far) ends a turn.
  private static func complete(_ audio: [Float]) -> Double? {
    guard let model = modelLock.withLock({ model }),
      let input = try? MLMultiArray(shape: [1, NSNumber(value: window)], dataType: .float32)
    else { return nil }
    let tail = audio.suffix(window)
    let pointer = input.dataPointer.bindMemory(to: Float.self, capacity: window)
    pointer.initialize(repeating: 0, count: window)
    // Zeros in front, the latest audio last.
    tail.withUnsafeBufferPointer { samples in
      guard let first = samples.baseAddress else { return }
      (pointer + (window - samples.count)).update(from: first, count: samples.count)
    }
    guard let output = try? model.prediction(from: MLDictionaryFeatureProvider(dictionary: ["audio": input])),
      let probability = output.featureValue(for: "probability")?.multiArrayValue
    else { return nil }
    return probability[0].doubleValue
  }
}

enum Convert {
  static func buffer(_ buffer: AVAudioPCMBuffer, to format: AVAudioFormat) -> AVAudioPCMBuffer? {
    if buffer.format == format { return buffer }
    guard let converter = AVAudioConverter(from: buffer.format, to: format) else { return nil }
    return Self.buffer(buffer, with: converter, to: format)
  }

  static func buffer(_ buffer: AVAudioPCMBuffer, with converter: AVAudioConverter, to format: AVAudioFormat)
    -> AVAudioPCMBuffer?
  {
    let ratio = format.sampleRate / buffer.format.sampleRate
    let capacity = AVAudioFrameCount((Double(buffer.frameLength) * ratio).rounded(.up)) + 16
    guard let out = AVAudioPCMBuffer(pcmFormat: format, frameCapacity: capacity) else { return nil }
    var consumed = false
    var error: NSError?
    let status = converter.convert(to: out, error: &error) { _, state in
      if consumed {
        state.pointee = .noDataNow
        return nil
      }
      consumed = true
      state.pointee = .haveData
      return buffer
    }
    return status == .error || out.frameLength == 0 ? nil : out
  }
}

/// The English name of a language code ("it" is "Italian"), for prompts.
func languageName(_ code: String) -> String {
  Locale(identifier: "en").localizedString(forLanguageCode: code) ?? code
}

/// Shortens an agent's reply for speaking, on device. Nil when the model is
/// unavailable (unsupported device, Apple Intelligence off on OS 26, model
/// still downloading) or refuses; the app then speaks its own shortening.
@MainActor
enum Condenser {
  #if canImport(FoundationModels)
    /// A session made and prewarmed ahead of time for [readyLanguage] (typed
    /// loosely: it needs OS 26), so a reply's condensing does not wait for
    /// the model to load.
    private static var ready: AnyObject?
    private static var readyLanguage = ""

    @available(iOS 26.0, macOS 26.0, *)
    private static func session(_ language: String) -> LanguageModelSession {
      LanguageModelSession(
        model: .default,
        instructions: """
          You turn an AI coding agent's written reply into what a phone assistant \
          would say out loud. Use at most three short sentences in plain speech: \
          no markdown, no lists, no code, no file paths or URLs read out. Keep the \
          outcome, any question the agent asks, and anything the person must do. \
          Always answer in \(languageName(language)), whatever the reply's language.
          """)
    }
  #endif

  /// Prepares the next condensing session; cheap when the model is loaded.
  static func prewarm(language: String) {
    #if canImport(FoundationModels)
      guard #available(iOS 26.0, macOS 26.0, *), SystemLanguageModel.default.isAvailable,
        ready == nil || readyLanguage != language
      else { return }
      let next = session(language)
      next.prewarm()
      ready = next
      readyLanguage = language
    #endif
  }

  static func condense(_ text: String, language: String) async -> String? {
    #if canImport(FoundationModels)
      guard #available(iOS 26.0, macOS 26.0, *) else { return nil }
      let model = SystemLanguageModel.default
      guard model.isAvailable else { return nil }
      // A session answers once (its transcript would carry over): take the
      // prepared one and prepare the next.
      let prepared = (readyLanguage == language ? ready as? LanguageModelSession : nil) ?? session(language)
      ready = nil
      defer { prewarm(language: language) }
      // The window is shared by instructions, prompt and answer: keep the
      // reply to about half of it (about 3 characters per token is safe for
      // English; other scripts count more tokens, hence the margin).
      let budget = max(1024, model.contextSize / 2) * 3
      let reply = text.count > budget ? String(text.prefix(budget)) + "…" : text
      do {
        let response = try await prepared.respond(
          to: "Reply to read aloud:\n\n\(reply)",
          options: GenerationOptions(maximumResponseTokens: 120))
        let spoken = response.content.trimmingCharacters(in: .whitespacesAndNewlines)
        return spoken.isEmpty ? nil : spoken
      } catch {
        return nil
      }
    #else
      return nil
    #endif
  }
}

#if canImport(FoundationModels)
  @available(iOS 26.0, macOS 26.0, *)
  @Generable
  struct IntakeAnswer {
    // First, so the app can act on check before the rest is written.
    @Guide(
      description:
        "check once the request is nearly clear or needs knowledge of the project's code; talk otherwise",
      .anyOf(["talk", "check"]))
    var next: String
    @Guide(description: "When next is talk: what to say aloud now, one or two short sentences. Empty for check")
    var say: String
  }
#endif

/// The voice session's front desk: on the device, holds a short, natural
/// conversation about what the person wants, and says when the project's
/// agent should check the request. It never proposes a task itself: it
/// cannot see the code. Nil when the model is unavailable; the app then asks
/// the project straight away.
@MainActor
enum Intake {
  /// The answers being worked out, by request: a newer turn calls one off.
  static var running: [Int: Task<[String: Any]?, Never>] = [:]

  #if canImport(FoundationModels)
    private static var ready: AnyObject?
    private static var readyLanguage = ""

    @available(iOS 26.0, macOS 26.0, *)
    private static func session(_ language: String) -> LanguageModelSession {
      let name = languageName(language)
      return LanguageModelSession(
        model: .default,
        instructions: """
          You answer a phone call for an AI coding agent that works on the person's \
          software project. You cannot see the project's code. Hold a short, natural \
          conversation in \(name) to understand what the person wants done: the goal, \
          what to change, and how to tell it is done. Reply in one or two short \
          sentences, like a person on the phone. Ask at most one question at a time, \
          and only what matters. Answer greetings and small talk briefly and warmly. \
          Never propose a task, sum it up or ask for confirmation yourself. Choose \
          check as soon as the request is nearly clear, or when the next question \
          needs knowledge of the project's code: the project will check it. Otherwise \
          choose talk. Always write in \(name). No markdown, lists or code.
          """)
    }
  #endif

  /// Prepares the next intake session, so the first turn does not wait for
  /// the model to load.
  static func prewarm(language: String) {
    #if canImport(FoundationModels)
      guard #available(iOS 26.0, macOS 26.0, *), SystemLanguageModel.default.isAvailable,
        ready == nil || readyLanguage != language
      else { return }
      let next = session(language)
      next.prewarm()
      ready = next
      readyLanguage = language
    #endif
  }

  /// The most of the project's memory excerpt given to [model]: the window
  /// is shared by instructions, memory, conversation and answer, so the
  /// excerpt gets about a quarter of it (about 3 characters per token, as
  /// the condenser counts).
  #if canImport(FoundationModels)
    @available(iOS 26.0, macOS 26.0, *)
    static func memoryBudget(_ model: SystemLanguageModel) -> Int {
      max(0, model.contextSize / 4) * 3
    }
  #endif

  static func clarify(
    _ turns: [(mine: Bool, text: String)], language: String, memory: String = ""
  ) async -> [String: Any]? {
    #if canImport(FoundationModels)
      guard #available(iOS 26.0, macOS 26.0, *), SystemLanguageModel.default.isAvailable else { return nil }
      let session = (readyLanguage == language ? ready as? LanguageModelSession : nil) ?? session(language)
      ready = nil
      defer { prewarm(language: language) }
      let lines = turns.suffix(16).map { ($0.mine ? "Person: " : "You: ") + $0.text }
      // In the prompt, not the instructions: the prewarmed session stays usable.
      let budget = memoryBudget(SystemLanguageModel.default)
      let excerpt = memory.trimmingCharacters(in: .whitespacesAndNewlines)
      let known =
        excerpt.isEmpty || budget < 200
        ? ""
        : "What the project does, from its notes:\n\n"
          + (excerpt.count > budget ? String(excerpt.prefix(budget)) + "…" : excerpt) + "\n\n"
      var next = "talk"
      var say = ""
      do {
        let stream = session.streamResponse(
          to: "\(known)The conversation so far:\n\n\(lines.joined(separator: "\n"))\n\nAnswer the person.",
          generating: IntakeAnswer.self,
          options: GenerationOptions(maximumResponseTokens: 80))
        for try await snapshot in stream {
          let partial = snapshot.content
          if let n = partial.next { next = n }
          guard let s = partial.say else { continue }
          // next is complete once say begins: a check needs nothing more.
          if next == "check" { return ["check": true, "say": ""] }
          say = s
        }
        return ["check": next == "check", "say": say]
      } catch {
        return nil
      }
    #else
      return nil
    #endif
  }
}

/// One part of an utterance: its range of the text, the audio made for it
/// so far, and whether it is all made.
struct SpokenPart {
  var range: Range<String.Index>
  var made = 0.0
  var done = false

  /// [pieces] as parts of [text], found in order; the whole text as one part
  /// when one is not found there.
  static func parts(of text: String, as pieces: [String]) -> [SpokenPart] {
    var out: [SpokenPart] = []
    var from = text.startIndex
    for piece in pieces {
      let wanted = piece.trimmingCharacters(in: .whitespacesAndNewlines)
      guard !wanted.isEmpty, let found = text.range(of: wanted, range: from..<text.endIndex) else {
        return [SpokenPart(range: text.startIndex..<text.endIndex)]
      }
      // Whatever lies between two parts (spaces, a line break) goes with
      // the earlier one.
      if let last = out.indices.last { out[last].range = out[last].range.lowerBound..<found.lowerBound }
      out.append(SpokenPart(range: found))
      from = found.upperBound
    }
    return out.isEmpty ? [SpokenPart(range: text.startIndex..<text.endIndex)] : out
  }

  /// The start of [text] known to be heard once [played] seconds of the
  /// audio made for [parts] were played back, in order: the parts all made
  /// and all played, and nothing of the next. Audio length says nothing of
  /// where within a part it stopped (a slow number, an acronym), so only a
  /// part's end is a known point.
  static func heard(of text: String, parts: [SpokenPart], played: Double) -> String {
    var left = played
    var end = text.startIndex
    for part in parts {
      guard part.done, part.made > 0, left >= part.made - 0.01 else { break }
      end = part.range.upperBound
      left -= part.made
    }
    return String(text[..<end])
  }
}

/// A value shared with a callback on another thread.
final class Locked<Value>: @unchecked Sendable {
  private let lock = NSLock()
  private var current: Value
  init(_ value: Value) { current = value }
  var value: Value { lock.withLock { current } }
  /// Changes the value in place.
  func update(_ change: (inout Value) -> Void) {
    lock.withLock { change(&current) }
  }

  /// Sets [new] and returns what was there.
  func swap(_ new: Value) -> Value {
    lock.withLock {
      let old = current
      current = new
      return old
    }
  }
}
