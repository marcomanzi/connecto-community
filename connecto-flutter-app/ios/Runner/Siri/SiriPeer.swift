import Darwin
import Foundation

/// Why a call to the Rust peer failed: its own reason, as the app shows it.
struct PeerFailure: Error, CustomStringConvertible {
  let reason: String
  var description: String { reason }
}

/// The Rust peer (`connecto_flutter_peer`) that Flutter talks to through
/// `lib/peer.dart`, called from Swift for Siri. It is the same library image
/// in the same process, so the intents share the one `RemoteManager` with the
/// app when it is open, and start it themselves when it is not.
final class SiriPeer: @unchecked Sendable {
  static let shared = SiriPeer()

  private typealias CallFn = @convention(c) (UnsafePointer<CChar>?) -> UnsafeMutablePointer<CChar>?
  private typealias FreeFn = @convention(c) (UnsafeMutablePointer<CChar>?) -> Void
  private let callFn: CallFn?
  private let freeFn: FreeFn?
  private let started = NSLock()
  private var didInit = false

  private init() {
    var handle: UnsafeMutableRawPointer?
    if let dir = Bundle.main.privateFrameworksPath {
      handle = dlopen(
        dir + "/connecto_flutter_peer.framework/connecto_flutter_peer", RTLD_NOW | RTLD_GLOBAL)
    }
    // RTLD_DEFAULT: already loaded by Flutter's native assets.
    let image = handle ?? UnsafeMutableRawPointer(bitPattern: -2)
    callFn = dlsym(image, "connecto_call").map { unsafeBitCast($0, to: CallFn.self) }
    freeFn = dlsym(image, "connecto_free").map { unsafeBitCast($0, to: FreeFn.self) }
  }

  /// Where Flutter keeps the peer's state (`getApplicationSupportDirectory()/remote`).
  static var directory: URL {
    FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
      .appendingPathComponent("remote", isDirectory: true)
  }

  /// One blocking call, as `connecto_call` takes it: `{"op", ...fields}` in,
  /// its `data` out.
  func callSync(_ op: String, _ fields: [String: Any] = [:]) throws -> Any? {
    guard let callFn, let freeFn else { throw PeerFailure(reason: "The Connecto peer is missing.") }
    var request = fields
    request["op"] = op
    let body = try JSONSerialization.data(withJSONObject: request)
    guard let text = String(data: body, encoding: .utf8) else {
      throw PeerFailure(reason: "Bad request")
    }
    guard let output = text.withCString({ callFn($0) }) else {
      throw PeerFailure(reason: "Peer returned an empty response")
    }
    defer { freeFn(output) }
    let data = Data(bytes: output, count: strlen(output))
    guard let response = try JSONSerialization.jsonObject(with: data, options: [.fragmentsAllowed])
      as? [String: Any]
    else { throw PeerFailure(reason: "Bad response") }
    if let reason = response["error"] as? String { throw PeerFailure(reason: reason) }
    let value = response["data"]
    return value is NSNull ? nil : value
  }

  /// [callSync] off the caller's thread: peer calls may wait on the host.
  func call(_ op: String, _ fields: [String: Any] = [:]) async throws -> Any? {
    try await withCheckedThrowingContinuation { continuation in
      DispatchQueue.global(qos: .userInitiated).async {
        do {
          let value = try self.callSync(op, fields)
          continuation.resume(returning: SendableBox(value))
        } catch {
          continuation.resume(throwing: error)
        }
      }
    }.value
  }

  /// Starts the peer once (idempotent in Rust too).
  func start() async throws {
    if started.withLock({ didInit }) { return }
    try FileManager.default.createDirectory(at: Self.directory, withIntermediateDirectories: true)
    _ = try await call("init", ["dir": Self.directory.path])
    started.withLock { didInit = true }
  }

  /// The peer's `state`: hosts, their link and projects.
  func state() async throws -> [String: Any] {
    try await start()
    return try await call("state") as? [String: Any] ?? [:]
  }

  /// Waits until `host` is connected, asking to connect if it is not, for at
  /// most `within`. False when the time ran out.
  func connected(_ host: String, within: Duration) async -> Bool {
    let deadline = ContinuousClock.now + within
    var asked = false
    while ContinuousClock.now < deadline {
      guard let state = try? await state() else { return false }
      let hosts = state["hosts"] as? [[String: Any]] ?? []
      guard let entry = hosts.first(where: { $0["name"] as? String == host }) else { return false }
      if entry["link"] as? String == "Connected" {
        SiriStore.remember(state: state)
        return true
      }
      if !asked {
        asked = true
        _ = try? await call("connect", ["host": host])
      }
      try? await Task.sleep(for: .milliseconds(150))
    }
    return false
  }
}

/// Carries a JSON value across the continuation.
struct SendableBox: @unchecked Sendable {
  let value: Any?
  init(_ value: Any?) { self.value = value }
}
