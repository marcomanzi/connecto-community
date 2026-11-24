import CryptoKit
import Foundation
import LlamaSwift

#if os(iOS)
  import UIKit
#endif

/// The on-phone assistant: a small instruct model run with llama.cpp on
/// Metal, for calls when neither Foundation Models nor the Mac's model is
/// there (offline, or a poor connection). Off until the person turns it on
/// in Settings, which downloads it once (about 1.3 GB) to Application
/// Support, excluded from backup. It loads on first need and unloads under
/// memory pressure, after a while unused, or when turned off (which also
/// deletes it).
final class PhoneModel: NSObject, URLSessionDownloadDelegate, @unchecked Sendable {
  static let shared = PhoneModel()

  /// Qwen3.5 2B instruct, Q4_K_S (bartowski's GGUF, Apache-2.0).
  static let fileName = "Qwen_Qwen3.5-2B-Q4_K_S.gguf"
  static let url = URL(
    string: "https://huggingface.co/bartowski/Qwen_Qwen3.5-2B-GGUF/resolve/main/\(fileName)")!
  static let size: Int64 = 1_327_696_992
  static let sha256 = "55b574899b75180d084238ffbf5d3d165c568a3d73e1edbd3c826682a986c8d4"

  private static let key = "connecto.assistant.phone"

  /// Unloaded after this long unused.
  private static let idleUnload: TimeInterval = 180
  private static let contextTokens: UInt32 = 4096

  /// Guards the state below; llama.cpp work runs on [work], one job at a time.
  private let lock = NSLock()
  private let work = DispatchQueue(label: "connecto.phone-model", qos: .userInitiated)
  private var downloading: URLSessionDownloadTask?
  private var progress: Double?
  /// Why the last download failed: kept until the person turns it on again,
  /// so a failure is never downloaded over and over.
  private var lastError: String?
  /// Waiting for Wi-Fi: it never downloads over cellular or a hotspot.
  private var waitingForWiFi = false
  private var session: URLSession?

  // Only touched on [work].
  private var model: OpaquePointer?
  private var context: OpaquePointer?
  private var lastUse = Date.distantPast
  /// Requests called off (a newer turn made them useless): one waiting in
  /// line never starts, one running stops between batches or tokens.
  private let cancelled = Locked<Set<Int>>([])
  private var idleTimer: DispatchSourceTimer?

  private override init() {
    super.init()
    #if os(iOS)
      NotificationCenter.default.addObserver(
        forName: UIApplication.didReceiveMemoryWarningNotification, object: nil, queue: nil
      ) { [weak self] _ in self?.work.async { self?.unload() } }
    #endif
  }

  static var enabled: Bool {
    get { UserDefaults.standard.bool(forKey: key) }
    set { UserDefaults.standard.set(newValue, forKey: key) }
  }

  private static func directory() throws -> URL {
    var url = try FileManager.default.url(
      for: .applicationSupportDirectory, in: .userDomainMask, appropriateFor: nil, create: true
    ).appendingPathComponent("ConnectoAssistant", isDirectory: true)
    try FileManager.default.createDirectory(at: url, withIntermediateDirectories: true)
    var values = URLResourceValues()
    values.isExcludedFromBackup = true
    try? url.setResourceValues(values)
    return url
  }

  private static func file() throws -> URL { try directory().appendingPathComponent(fileName) }

  /// Downloaded and checked.
  var ready: Bool {
    guard let file = try? Self.file(),
      let attributes = try? FileManager.default.attributesOfItem(atPath: file.path),
      let bytes = attributes[.size] as? Int64
    else { return false }
    return bytes == Self.size
  }

  /// What Settings and the call show: on, ready, the download's progress,
  /// the size, and why the last download failed.
  func status() -> [String: Any] {
    // On but missing, and not on its way (the app was closed meanwhile):
    // the download goes on, unless it failed (then only on a new try).
    if Self.enabled && !ready { startDownload(retry: false) }
    lock.lock()
    defer { lock.unlock() }
    var out: [String: Any] = ["on": Self.enabled, "ready": ready, "bytes": Self.size]
    if let progress { out["progress"] = progress }
    if let lastError { out["error"] = lastError }
    if waitingForWiFi { out["waiting"] = true }
    return out
  }

  /// Turns it on (downloading it when needed) or off (stopping a download,
  /// unloading it and deleting it, to give the space back).
  func set(on: Bool) {
    Self.enabled = on
    if on {
      startDownload(retry: true)
      return
    }
    // Under the lock, as an install is: a download finishing now cannot
    // put the file back after it is deleted.
    lock.lock()
    let task = downloading
    downloading = nil
    progress = nil
    lastError = nil
    waitingForWiFi = false
    if let file = try? Self.file() { try? FileManager.default.removeItem(at: file) }
    lock.unlock()
    task?.cancel()
    // A loaded model keeps its mapping until it is unloaded.
    work.async { self.unload() }
  }

  /// Starts the download unless it runs, is done, or failed and this is
  /// not a new try ([retry]: the person turned it on).
  private func startDownload(retry: Bool) {
    lock.lock()
    defer { lock.unlock() }
    guard downloading == nil, !ready, retry || lastError == nil else { return }
    let session =
      session
      ?? {
        let configuration = URLSessionConfiguration.default
        // 1.3 GB: Wi-Fi only, waiting for it rather than failing.
        configuration.allowsCellularAccess = false
        configuration.allowsExpensiveNetworkAccess = false
        configuration.allowsConstrainedNetworkAccess = false
        configuration.waitsForConnectivity = true
        return URLSession(configuration: configuration, delegate: self, delegateQueue: nil)
      }()
    self.session = session
    lastError = nil
    waitingForWiFi = false
    progress = 0
    let task = session.downloadTask(with: Self.url)
    downloading = task
    task.resume()
  }

  func urlSession(_ session: URLSession, taskIsWaitingForConnectivity task: URLSessionTask) {
    lock.lock()
    if downloading === task { waitingForWiFi = true }
    lock.unlock()
  }

  func urlSession(
    _ session: URLSession, downloadTask: URLSessionDownloadTask, didWriteData bytesWritten: Int64,
    totalBytesWritten: Int64, totalBytesExpectedToWrite: Int64
  ) {
    lock.lock()
    if downloading === downloadTask {
      waitingForWiFi = false
      progress = min(0.99, Double(totalBytesWritten) / Double(Self.size))
    }
    lock.unlock()
  }

  func urlSession(
    _ session: URLSession, downloadTask: URLSessionDownloadTask, didFinishDownloadingTo location: URL
  ) {
    // The file is gone once this returns: it is checked now (outside the
    // lock: hashing 1.3 GB takes a while), then installed under the lock,
    // only if this download is still the one wanted.
    var failure: String?
    let checked: Bool
    do {
      let (bytes, digest) = try Self.measure(location)
      checked = bytes == Self.size && digest == Self.sha256
      if !checked { failure = "The download was damaged. Turn it on again to retry." }
    } catch {
      checked = false
      failure = error.localizedDescription
    }
    lock.lock()
    defer { lock.unlock() }
    guard downloading === downloadTask else { return }
    downloading = nil
    progress = nil
    waitingForWiFi = false
    guard checked, Self.enabled else {
      lastError = Self.enabled ? failure : nil
      return
    }
    do {
      let target = try Self.file()
      try? FileManager.default.removeItem(at: target)
      try FileManager.default.moveItem(at: location, to: target)
    } catch {
      lastError = error.localizedDescription
    }
  }

  func urlSession(_ session: URLSession, task: URLSessionTask, didCompleteWithError error: Error?) {
    guard let error else { return }
    lock.lock()
    if downloading === task {
      downloading = nil
      progress = nil
      waitingForWiFi = false
      if (error as NSError).code != NSURLErrorCancelled { lastError = error.localizedDescription }
    }
    lock.unlock()
  }

  /// The file's length and SHA-256, read in pieces.
  private static func measure(_ url: URL) throws -> (Int64, String) {
    let handle = try FileHandle(forReadingFrom: url)
    defer { try? handle.close() }
    var hasher = SHA256()
    var bytes: Int64 = 0
    while let chunk = try handle.read(upToCount: 8 << 20), !chunk.isEmpty {
      hasher.update(data: chunk)
      bytes += Int64(chunk.count)
    }
    return (bytes, hasher.finalize().map { String(format: "%02x", $0) }.joined())
  }

  // MARK: - Running it

  /// Loads it ahead of a call's first need. Cheap once loaded.
  func prewarm() {
    guard Self.enabled, ready else { return }
    work.async { _ = self.load() }
  }

  /// The answer to [messages] (`(role, text)`), at most [maxTokens] long,
  /// or nil: not on, not downloaded, too slow for [deadline], or failed.
  func complete(
    _ messages: [(String, String)], maxTokens: Int, temperature: Double, deadline: Date, request: Int? = nil
  ) async -> String? {
    // Speech is being made: it has the GPU, and this answer can wait for a
    // later turn (the call does without it now).
    guard Self.enabled, ready, !SpeechLoad.busy else { return nil }
    let stop: () -> Bool = { [cancelled] in request.map { cancelled.value.contains($0) } ?? false }
    return await withCheckedContinuation { done in
      work.async {
        defer { if let request { self.cancelled.update { $0.remove(request) } } }
        done.resume(
          returning: stop()
            ? nil
            : self.generate(
              messages, maxTokens: maxTokens, temperature: temperature, deadline: deadline, stop: stop))
      }
    }
  }

  /// Calls off [request]: it stops as soon as it can, and answers nil.
  func cancel(_ request: Int) {
    cancelled.update { ids in
      ids.insert(request)
      // One called off after it ended is never removed: keep the newest.
      if ids.count > 64 { ids = Set(ids.sorted().suffix(32)) }
    }
  }

  /// On [work]: used now; unloaded once unused for [idleUnload].
  private func touch() {
    lastUse = Date()
    scheduleUnload()
  }

  /// On [work].
  private func load() -> Bool {
    touch()
    if context != nil { return true }
    guard let path = try? Self.file().path else { return false }
    llama_backend_init()
    var params = llama_model_default_params()
    params.n_gpu_layers = -1
    guard let loaded = llama_model_load_from_file(path, params) else { return false }
    var contextParams = llama_context_default_params()
    contextParams.n_ctx = Self.contextTokens
    contextParams.n_batch = 512
    let threads = Int32(max(1, min(4, ProcessInfo.processInfo.activeProcessorCount - 2)))
    contextParams.n_threads = threads
    contextParams.n_threads_batch = threads
    guard let made = llama_init_from_model(loaded, contextParams) else {
      llama_model_free(loaded)
      return false
    }
    model = loaded
    context = made
    return true
  }

  /// On [work].
  private func unload() {
    idleTimer?.cancel()
    idleTimer = nil
    if let context { llama_free(context) }
    if let model { llama_model_free(model) }
    context = nil
    model = nil
  }

  /// On [work].
  private func scheduleUnload() {
    idleTimer?.cancel()
    let timer = DispatchSource.makeTimerSource(queue: work)
    timer.schedule(deadline: .now() + Self.idleUnload)
    timer.setEventHandler { [weak self] in
      guard let self, Date().timeIntervalSince(self.lastUse) >= Self.idleUnload - 1 else { return }
      self.unload()
    }
    timer.resume()
    idleTimer = timer
  }

  /// Qwen's chat format (ChatML), with an empty reasoning block so it
  /// answers at once.
  static func prompt(_ messages: [(String, String)]) -> String {
    var out = ""
    for (role, text) in messages {
      out += "<|im_start|>\(role)\n\(text)<|im_end|>\n"
    }
    return out + "<|im_start|>assistant\n<think>\n\n</think>\n\n"
  }

  /// On [work].
  private func generate(
    _ messages: [(String, String)], maxTokens: Int, temperature: Double, deadline: Date,
    stop: () -> Bool = { false }
  ) -> String? {
    guard load(), let model, let context, let vocab = llama_model_get_vocab(model) else { return nil }
    // However it ends, the idle wait counts from now.
    defer { touch() }
    llama_memory_clear(llama_get_memory(context), true)

    let text = Self.prompt(messages)
    let capacity = Int32(text.utf8.count + 16)
    var tokens = [llama_token](repeating: 0, count: Int(capacity))
    let count = llama_tokenize(vocab, text, Int32(text.utf8.count), &tokens, capacity, true, true)
    guard count > 0 else { return nil }
    tokens.removeSubrange(Int(count)...)
    // Room for the answer: the conversation's oldest part is what goes.
    let room = Int(Self.contextTokens) - maxTokens - 8
    guard tokens.count <= room else { return nil }

    // The prompt, in batches.
    let batchSize = Int(llama_n_batch(context))
    var at = 0
    while at < tokens.count {
      if Date() >= deadline || SpeechLoad.busy || stop() { return nil }
      let end = min(tokens.count, at + batchSize)
      let ok = tokens[at..<end].withContiguousStorageIfAvailable { slice -> Bool in
        let batch = llama_batch_get_one(
          UnsafeMutablePointer(mutating: slice.baseAddress!), Int32(slice.count))
        return llama_decode(context, batch) == 0
      }
      guard ok == true else { return nil }
      at = end
    }

    let sampler = llama_sampler_chain_init(llama_sampler_chain_default_params())
    defer { llama_sampler_free(sampler) }
    if temperature <= 0 {
      llama_sampler_chain_add(sampler, llama_sampler_init_greedy())
    } else {
      llama_sampler_chain_add(sampler, llama_sampler_init_top_p(0.9, 1))
      llama_sampler_chain_add(sampler, llama_sampler_init_temp(Float(temperature)))
      llama_sampler_chain_add(sampler, llama_sampler_init_dist(UInt32.random(in: 0...UInt32.max)))
    }

    var bytes = [UInt8]()
    var piece = [CChar](repeating: 0, count: 256)
    for _ in 0..<maxTokens {
      // Out of time, speech began (it gets the GPU), or a newer turn made
      // this answer useless: it is dropped.
      if Date() >= deadline || SpeechLoad.busy || stop() { return nil }
      var next = llama_sampler_sample(sampler, context, -1)
      if llama_vocab_is_eog(vocab, next) { break }
      let n = llama_token_to_piece(vocab, next, &piece, Int32(piece.count), 0, false)
      if n > 0 { bytes.append(contentsOf: piece[0..<Int(n)].map { UInt8(bitPattern: $0) }) }
      // The batch points at the token: decoded while the pointer is valid.
      let decoded = withUnsafeMutablePointer(to: &next) { token in
        llama_decode(context, llama_batch_get_one(token, 1)) == 0
      }
      guard decoded else { return nil }
    }
    let answer = String(decoding: bytes, as: UTF8.self).trimmingCharacters(in: .whitespacesAndNewlines)
    return answer.isEmpty ? nil : answer
  }
}
