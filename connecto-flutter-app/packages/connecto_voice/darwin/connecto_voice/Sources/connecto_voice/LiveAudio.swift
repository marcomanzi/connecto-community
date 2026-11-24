// The audio of a Gemini Live call (ADR 0034): the microphone, echo-cancelled
// by voice processing, as 16 kHz 16-bit PCM for the Live socket, and
// Gemini's 24 kHz 16-bit PCM played through the same engine, so the person
// can talk over it. Recognition, turn-taking and speech are Gemini's; this
// only moves audio. The API key lives in the Keychain (`LiveKey`).

import AVFoundation
import Foundation
import Security
import Speech

#if os(iOS)
  import Flutter
#else
  import FlutterMacOS
#endif

/// The live audio's own event channel (`connecto/voice/live`): microphone
/// chunks and playback state. Kept apart from the call's events, whose
/// native side has a single sink.
final class LiveEvents: NSObject, FlutterStreamHandler {
  private var sink: FlutterEventSink?

  func onListen(withArguments _: Any?, eventSink events: @escaping FlutterEventSink) -> FlutterError? {
    sink = events
    return nil
  }

  func onCancel(withArguments _: Any?) -> FlutterError? {
    sink = nil
    return nil
  }

  func emit(_ event: [String: Any]) {
    DispatchQueue.main.async { [weak self] in self?.sink?(event) }
  }
}

@available(iOS 26.0, macOS 26.0, *)
@MainActor
final class LiveAudio {
  private let emit: ([String: Any]) -> Void
  private let engine = AVAudioEngine()
  private let player = AVAudioPlayerNode()
  private let tap = LiveTap()
  private var observers: [NSObjectProtocol] = []
  private var sessionActive = false
  private var stopped = false
  /// Gemini's audio: 24 kHz mono, played as float.
  private let playFormat = AVAudioFormat(
    commonFormat: .pcmFormatFloat32, sampleRate: 24_000, channels: 1, interleaved: false)!
  /// Buffers scheduled and not yet heard, and a generation bumped by every
  /// flush, so a flushed buffer's callback changes nothing.
  private var pending = 0
  private var generation = 0
  /// The on-device recognizer that hears the person's answer to a proposal
  /// (ADR 0034): Gemini's transcripts come in no set order, this one in the
  /// microphone's.
  private var analyzer: SpeechAnalyzer?
  private var resultsTask: Task<Void, Never>?
  private var answers = 0

  init(emit: @escaping ([String: Any]) -> Void) {
    self.emit = emit
    tap.emit = emit
  }

  var muted: Bool {
    get { tap.muted }
    set { tap.muted = newValue }
  }

  func start() async throws {
    #if os(iOS)
      let mic = await AVAudioApplication.requestRecordPermission()
    #else
      let mic = await AVCaptureDevice.requestAccess(for: .audio)
    #endif
    guard mic else { throw CallError("Allow microphone access in Settings to make calls.") }
    guard !stopped else { throw CallError("The call has ended.") }
    #if os(iOS)
      let session = AVAudioSession.sharedInstance()
      try session.setCategory(
        .playAndRecord, mode: .voiceChat,
        options: [.defaultToSpeaker, .allowBluetoothHFP, .allowBluetoothA2DP])
      try session.setActive(true)
      sessionActive = true
    #endif
    // Echo cancellation and noise suppression; only while the engine is stopped.
    try engine.inputNode.setVoiceProcessingEnabled(true)
    engine.attach(player)
    engine.connect(player, to: engine.mainMixerNode, format: playFormat)
    try connectMicrophone()
    observe()
  }

  private func connectMicrophone() throws {
    let micFormat = engine.inputNode.outputFormat(forBus: 0)
    guard micFormat.sampleRate > 0 else { throw CallError("No microphone is available.") }
    engine.inputNode.removeTap(onBus: 0)
    tap.configure(from: micFormat)
    let state = tap
    engine.inputNode.installTap(onBus: 0, bufferSize: 2048, format: micFormat) { buffer, _ in
      state.feed(buffer)
    }
    engine.prepare()
    try engine.start()
  }

  /// A new route taps the microphone again; an interruption ends the call.
  private func observe() {
    let center = NotificationCenter.default
    observers.append(
      center.addObserver(forName: .AVAudioEngineConfigurationChange, object: engine, queue: .main) {
        [weak self] _ in
        MainActor.assumeIsolated {
          guard let self, !self.stopped else { return }
          self.flush()
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

  /// Plays [data], 24 kHz 16-bit little-endian mono PCM, after what is queued.
  func play(_ data: Data) {
    guard !stopped, engine.isRunning else { return }
    let frames = data.count / 2
    guard frames > 0, let buffer = AVAudioPCMBuffer(pcmFormat: playFormat, frameCapacity: AVAudioFrameCount(frames))
    else { return }
    buffer.frameLength = AVAudioFrameCount(frames)
    let out = buffer.floatChannelData![0]
    data.withUnsafeBytes { raw in
      for i in 0..<frames {
        let sample = Int16(littleEndian: raw.loadUnaligned(fromByteOffset: i * 2, as: Int16.self))
        out[i] = Float(sample) / 32768
      }
    }
    let mine = generation
    if pending == 0 { emit(["type": "playback", "playing": true]) }
    pending += 1
    player.scheduleBuffer(buffer, completionCallbackType: .dataPlayedBack) { [weak self] _ in
      DispatchQueue.main.async {
        MainActor.assumeIsolated {
          guard let self, mine == self.generation else { return }
          self.pending -= 1
          if self.pending == 0 { self.emit(["type": "playback", "playing": false]) }
        }
      }
    }
    if !player.isPlaying { player.play() }
  }

  /// The device's own voice, for reading back what a yes would send (ADR
  /// 0034, "answers by voice"): the app's words, never Gemini's.
  private let synthesizer = AVSpeechSynthesizer()
  /// Which read-back counts, and the synthesis being waited for.
  private let readBacks = ReadBackTracker()
  private var synthesizing: SpokenBuffers<AVAudioPCMBuffer>?
  /// The read-back being played, by its number: resumed exactly once, with
  /// true only when its last buffer played out and nothing invalidated it.
  private var saying: (Int, CheckedContinuation<Bool, Never>)?
  /// The longest a synthesis may take before the read-back is given up.
  private let synthesisLimit: Duration = .seconds(30)

  private func endSaying(_ number: Int?, whole: Bool) {
    guard let (current, done) = saying, number == nil || number == current else { return }
    saying = nil
    done.resume(returning: whole)
  }

  /// Says [text] with the device's voice in [language], through the same
  /// player as Gemini (so echo cancellation knows it), after what is queued.
  /// True only once all of it played out; false when any of it could not be
  /// converted, it was flushed (talked over) while synthesized or played,
  /// replaced by a newer [say], its synthesis stalled, or the call stopped.
  func say(_ text: String, language: String?) async -> Bool {
    guard !stopped, engine.isRunning, !text.isEmpty else { return false }
    // A newer read-back replaces the one before, wherever it was.
    endSaying(nil, whole: false)
    synthesizing?.cancel()
    let mine = readBacks.begin()
    let utterance = AVSpeechUtterance(string: text)
    utterance.voice = AVSpeechSynthesisVoice(language: language)
    let collected = SpokenBuffers<AVAudioPCMBuffer>()
    synthesizing = collected
    let limit = synthesisLimit
    let timeout = Task { @MainActor [weak self] in
      try? await Task.sleep(for: limit)
      if collected.cancel() {
        self?.readBacks.invalidate()
        self?.synthesizer.stopSpeaking(at: .immediate)
      }
    }
    await collected.wait {
      synthesizer.write(utterance) { buffer in
        guard let pcm = buffer as? AVAudioPCMBuffer else { return }
        if pcm.frameLength == 0 {
          collected.finish()
        } else {
          collected.add(pcm)
        }
      }
    }
    timeout.cancel()
    if synthesizing === collected { synthesizing = nil }
    guard readBacks.isValid(mine), !collected.cancelled, !stopped, engine.isRunning else {
      return false
    }
    // All of it, or none: a piece that failed to convert is not skipped.
    guard
      let buffers = ReadBackTracker.allConverted(
        collected.all.map { Convert.buffer($0, to: playFormat) })
    else {
      readBacks.invalidate()
      return false
    }
    let gen = generation
    return await withCheckedContinuation { (done: CheckedContinuation<Bool, Never>) in
      saying = (mine, done)
      if pending == 0 { emit(["type": "playback", "playing": true]) }
      for (i, buffer) in buffers.enumerated() {
        pending += 1
        let last = i == buffers.count - 1
        player.scheduleBuffer(buffer, completionCallbackType: .dataPlayedBack) { [weak self] _ in
          DispatchQueue.main.async {
            MainActor.assumeIsolated {
              guard let self, gen == self.generation else { return }
              self.pending -= 1
              if self.pending == 0 { self.emit(["type": "playback", "playing": false]) }
              if last { self.endSaying(mine, whole: self.readBacks.isValid(mine) && !self.stopped) }
            }
          }
        }
      }
      if !player.isPlaying { player.play() }
    }
  }

  /// Drops everything queued to play: the person talked over it.
  func flush() {
    // A read-back cut short, while synthesized or played, never counts.
    readBacks.invalidate()
    if synthesizing?.cancel() == true { synthesizer.stopSpeaking(at: .immediate) }
    synthesizing = nil
    endSaying(nil, whole: false)
    generation += 1
    let was = pending
    pending = 0
    if player.engine != nil { player.stop() }
    if was > 0 { emit(["type": "playback", "playing": false]) }
  }

  /// Starts recognizing what the person says, on the device, in
  /// [language] (the call language when nil): `heard` events with each
  /// result, tagged with the app's [session], and `heardEnded` if the
  /// recognizer stops on its own. Throws when no recognizer can run.
  func listenForAnswer(language: String?, session: Int) async throws {
    stopListening()
    answers += 1
    let mine = answers
    let wanted = language.map(Locale.init(identifier:)) ?? VoiceCall.wantedLocale
    guard let (speech, locale) = await VoiceCall.transcriber(for: wanted) else {
      throw CallError("On-device speech recognition does not support this language.")
    }
    let module = VoiceCall.module(speech: speech, locale: locale)
    // Download progress is not this call's to show.
    try await VoiceCall.install(module, locale: locale, emit: { _ in })
    guard let format = await SpeechAnalyzer.bestAvailableAudioFormat(compatibleWith: [module]) else {
      throw CallError("Speech recognition has no audio format for this device.")
    }
    guard mine == answers, !stopped else { return }
    let (stream, continuation) = AsyncStream<AnalyzerInput>.makeStream(bufferingPolicy: .bufferingNewest(256))
    let analyzer = SpeechAnalyzer(modules: [module], options: .init(priority: .userInitiated, modelRetention: .lingering))
    try await analyzer.prepareToAnalyze(in: format)
    try await analyzer.start(inputSequence: stream)
    guard mine == answers, !stopped else {
      continuation.finish()
      await analyzer.cancelAndFinishNow()
      return
    }
    self.analyzer = analyzer
    tap.listen(continuation, format: format)
    if let transcriber = module as? SpeechTranscriber {
      results(transcriber.results, answer: mine, session: session)
    } else if let transcriber = module as? DictationTranscriber {
      results(transcriber.results, answer: mine, session: session)
    }
  }

  private func results<S: AsyncSequence & Sendable>(_ results: S, answer mine: Int, session: Int)
  where S.Element: SpeechModuleResult {
    resultsTask = Task { @MainActor [weak self] in
      do {
        for try await result in results {
          guard let self, mine == self.answers else { return }
          let text: String
          if let r = result as? SpeechTranscriber.Result {
            text = String(r.text.characters)
          } else if let r = result as? DictationTranscriber.Result {
            text = String(r.text.characters)
          } else {
            continue
          }
          self.emit(["type": "heard", "text": text, "final": result.isFinal, "session": session])
        }
      } catch {}
      // Ended on its own (not by stopListening): what it heard can no
      // longer be corrected, so the app drops it and the answer is
      // confirmed on screen.
      if let self, mine == self.answers {
        self.emit(["type": "heardEnded", "session": session])
      }
    }
  }

  func stopListening() {
    answers += 1
    tap.listen(nil, format: nil)
    resultsTask?.cancel()
    resultsTask = nil
    if let analyzer {
      Task { await analyzer.cancelAndFinishNow() }
    }
    analyzer = nil
  }

  func stop() {
    stopListening()
    stopped = true
    for observer in observers { NotificationCenter.default.removeObserver(observer) }
    observers = []
    flush()
    tap.close()
    if engine.isRunning { engine.stop() }
    engine.inputNode.removeTap(onBus: 0)
    #if os(iOS)
      if sessionActive {
        sessionActive = false
        try? AVAudioSession.sharedInstance().setActive(false, options: .notifyOthersOnDeactivation)
      }
    #endif
  }
}

/// The microphone tap: converts to 16 kHz 16-bit mono and sends each chunk.
@available(iOS 26.0, macOS 26.0, *)
final class LiveTap: @unchecked Sendable {
  private let lock = NSLock()
  private var converter: AVAudioConverter?
  private let target = AVAudioFormat(
    commonFormat: .pcmFormatInt16, sampleRate: 16_000, channels: 1, interleaved: true)!
  private var isMuted = false
  private var open = false
  /// The answer recognizer's input, while it listens.
  private var answer: AsyncStream<AnalyzerInput>.Continuation?
  private var answerConverter: AVAudioConverter?
  private var answerFormat: AVAudioFormat?
  private var micFormat: AVAudioFormat?
  var emit: (([String: Any]) -> Void)?

  var muted: Bool {
    get { lock.withLock { isMuted } }
    set { lock.withLock { isMuted = newValue } }
  }

  func configure(from source: AVAudioFormat) {
    lock.withLock {
      converter = AVAudioConverter(from: source, to: target)
      micFormat = source
      if let answerFormat { answerConverter = AVAudioConverter(from: source, to: answerFormat) }
      open = true
    }
  }

  func listen(_ continuation: AsyncStream<AnalyzerInput>.Continuation?, format: AVAudioFormat?) {
    lock.withLock {
      answer?.finish()
      answer = continuation
      answerFormat = format
      answerConverter = format.flatMap { f in micFormat.flatMap { AVAudioConverter(from: $0, to: f) } }
    }
  }

  func feed(_ buffer: AVAudioPCMBuffer) {
    let chunk: Data? = lock.withLock {
      if open, !isMuted, let answer, let answerConverter, let answerFormat,
        let ready = Convert.buffer(buffer, with: answerConverter, to: answerFormat)
      {
        answer.yield(AnalyzerInput(buffer: ready))
      }
      guard open, !isMuted, let converter,
        let out = Convert.buffer(buffer, with: converter, to: target),
        let samples = out.int16ChannelData
      else { return nil }
      return Data(bytes: samples[0], count: Int(out.frameLength) * 2)
    }
    if let chunk { emit?(["type": "audio", "data": FlutterStandardTypedData(bytes: chunk)]) }
  }

  func close() {
    lock.withLock {
      open = false
      converter = nil
      answer?.finish()
      answer = nil
      answerConverter = nil
    }
  }
}

/// The Gemini API key, in the Keychain only: never in preferences or logs.
/// Whether Live calls are on is an ordinary preference.
enum LiveKey {
  private static let service = "connecto.gemini-live"
  private static let account = "api-key"

  static var enabled: Bool {
    get { UserDefaults.standard.bool(forKey: "connecto.live.enabled") }
    set { UserDefaults.standard.set(newValue, forKey: "connecto.live.enabled") }
  }

  private static func query(_ dataProtection: Bool) -> [String: Any] {
    var q: [String: Any] = [
      kSecClass as String: kSecClassGenericPassword,
      kSecAttrService as String: service,
      kSecAttrAccount as String: account,
    ]
    #if os(macOS)
      // The modern keychain when the build is entitled to it; the login
      // keychain otherwise (an unsigned debug build).
      if dataProtection { q[kSecUseDataProtectionKeychain as String] = true }
    #endif
    return q
  }

  private static var modes: [Bool] {
    #if os(macOS)
      return [true, false]
    #else
      return [true]
    #endif
  }

  static func read() -> String? {
    for mode in modes {
      var q = query(mode)
      q[kSecReturnData as String] = true
      q[kSecMatchLimit as String] = kSecMatchLimitOne
      var item: CFTypeRef?
      if SecItemCopyMatching(q as CFDictionary, &item) == errSecSuccess, let data = item as? Data {
        return String(data: data, encoding: .utf8)
      }
    }
    return nil
  }

  /// Stores [key], or deletes it when nil or empty. Throws without the key
  /// in the message.
  static func write(_ key: String?) throws {
    for mode in modes { SecItemDelete(query(mode) as CFDictionary) }
    guard let key, !key.isEmpty else { return }
    var status = errSecParam
    for mode in modes {
      var q = query(mode)
      q[kSecValueData as String] = Data(key.utf8)
      q[kSecAttrAccessible as String] = kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly
      status = SecItemAdd(q as CFDictionary, nil)
      if status == errSecSuccess { return }
    }
    throw CallError("The key could not be saved in the Keychain (\(status)).")
  }
}
