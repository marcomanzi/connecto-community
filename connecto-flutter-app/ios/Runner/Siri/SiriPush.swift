import CryptoKit
import Foundation
import Security
import UIKit

/// The phone's APNs registration and the key that hides reply previews from
/// everyone but this phone (§5.5): a symmetric key made here, kept in the
/// Keychain group the notification extension shares, and sent only inside
/// the authenticated link to the host.
enum SiriPush {
  private static let tokenKey = "siri.apnsToken"

  /// The APNs device token, hex, once the system gave one.
  static var token: String? {
    get { SiriStore.defaults.string(forKey: tokenKey) }
    set { SiriStore.defaults.set(newValue, forKey: tokenKey) }
  }

  /// Asks for notification permission (once; later calls return at once)
  /// and an APNs token.
  @MainActor
  static func start() {
    Task {
      _ = try? await UNUserNotificationCenter.current().requestAuthorization(options: [
        .alert, .sound, .badge,
      ])
      UIApplication.shared.registerForRemoteNotifications()
    }
  }

  /// The system gave `data`: registered with every connected host when it
  /// changed.
  static func received(_ data: Data) {
    let hex = data.map { String(format: "%02x", $0) }.joined()
    let old = token
    rotate(from: old, to: hex)
    if old != hex { Task.detached { await registerWithHosts() } }
  }

  /// Takes `new` as the token, keeping the old one for every host that has
  /// not heard of it yet (one disconnected now hears at its next connect).
  static func rotate(from old: String?, to new: String) {
    token = new
    guard let old, old != new else { return }
    SiriStore.mutateRotations { $0.append(TokenRotation(old: old, new: new, at: Date())) }
  }

  /// Where the host sends this phone's notices, for `SiriAsk.notify`; nil
  /// without an APNs token or a key.
  static func destination() -> [String: Any]? {
    guard let token, let key = E2EKey.base64() else { return nil }
    return [
      "deviceToken": token, "key": key, "env": environment,
      "topic": Bundle.main.bundleIdentifier ?? "", "locale": SiriWords.current.italian ? "it" : "en",
    ]
  }

  /// `PushRegister` with every connected host, through any of its projects
  /// (the host keeps destinations by device, not by project).
  static func registerWithHosts() async {
    guard let state = try? await SiriPeer.shared.state() else { return }
    let hosts = state["hosts"] as? [[String: Any]] ?? []
    let paired = Set(hosts.compactMap { $0["name"] as? String })
    for host in hosts {
      guard host["link"] as? String == "Connected", let name = host["name"] as? String,
        let project = (host["projects"] as? [[String: Any]])?.first?["id"] as? String
      else { continue }
      await register(host: name, project: project, paired: paired)
    }
  }

  /// `PushRegister` with one host: each replacement it has not heard of
  /// (old → new, in order, each kept until the host answers), then the
  /// current destination.
  static func register(
    host: String, project: String, paired: Set<String>,
    destination: [String: Any]? = destination(),
    call: PeerCall = { try await SiriHost.bounded($0, $1, within: .seconds(10)) }
  ) async {
    guard let fields = destination else { return }
    var base = fields
    base["host"] = host
    base["project"] = project
    for rotation in SiriStore.untold(host) {
      var ask = base
      ask["deviceToken"] = rotation.new
      ask["oldDeviceToken"] = rotation.old
      guard (try? await call("pushRegister", ask)) != nil else { return }
      SiriStore.told(host, rotation, paired: paired)
    }
    _ = try? await call("pushRegister", base)
  }

  /// What `lib/siri.dart` shows about push in Settings.
  @MainActor
  static func registration() async -> [String: Any] {
    ["token": token as Any? ?? NSNull(), "env": environment, "notifications": await SiriNotices.authorized(within: .seconds(5))]
  }

  /// APNs's sandbox for development-signed builds, production otherwise
  /// (App Store and TestFlight builds carry no embedded profile).
  static let environment: String = {
    guard let path = Bundle.main.path(forResource: "embedded", ofType: "mobileprovision"),
      let data = FileManager.default.contents(atPath: path),
      let text = String(data: data, encoding: .isoLatin1),
      let start = text.range(of: "<key>aps-environment</key>")
    else { return "production" }
    let rest = text[start.upperBound...].prefix(80)
    return rest.contains("development") ? "sandbox" : "production"
  }()
}

/// The preview key, shared with the notification service extension.
enum E2EKey {
  static let service = "studio.connecto.siri.preview-key"

  /// `$(AppIdentifierPrefix)studio.connecto.connectoFlutterApp.shared`, from Info.plist.
  static var accessGroup: String? {
    Bundle.main.object(forInfoDictionaryKey: "ConnectoKeychainGroup") as? String
  }

  static func base64() -> String? {
    (load() ?? make())?.base64EncodedString()
  }

  static func load() -> Data? {
    var query: [String: Any] = [
      kSecClass as String: kSecClassGenericPassword,
      kSecAttrService as String: service,
      kSecReturnData as String: true,
    ]
    if let accessGroup { query[kSecAttrAccessGroup as String] = accessGroup }
    var out: CFTypeRef?
    guard SecItemCopyMatching(query as CFDictionary, &out) == errSecSuccess else { return nil }
    return out as? Data
  }

  private static func make() -> Data? {
    let key = SymmetricKey(size: .bits256).withUnsafeBytes { Data($0) }
    var item: [String: Any] = [
      kSecClass as String: kSecClassGenericPassword,
      kSecAttrService as String: service,
      kSecValueData as String: key,
      // The extension decrypts while the phone is locked.
      kSecAttrAccessible as String: kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly,
    ]
    if let accessGroup { item[kSecAttrAccessGroup as String] = accessGroup }
    let status = SecItemAdd(item as CFDictionary, nil)
    return status == errSecSuccess ? key : load()
  }
}
