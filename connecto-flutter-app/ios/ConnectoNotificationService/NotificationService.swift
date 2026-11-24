import CryptoKit
import Foundation
import Security
import UserNotifications

/// Puts a reply's preview in a Siri notice (§5.5). The host sends a generic
/// alert ("The reply is ready") and the preview in `c`, sealed with the key
/// this phone made: base64 of nonce (12 bytes), ciphertext and tag, AES-256-GCM.
/// When anything fails the generic alert stays as it came.
final class NotificationService: UNNotificationServiceExtension {
  private var handler: ((UNNotificationContent) -> Void)?
  private var content: UNMutableNotificationContent?

  override func didReceive(
    _ request: UNNotificationRequest,
    withContentHandler contentHandler: @escaping (UNNotificationContent) -> Void
  ) {
    handler = contentHandler
    let content = (request.content.mutableCopy() as? UNMutableNotificationContent)
    self.content = content
    guard let content else { return contentHandler(request.content) }
    let info = request.content.userInfo
    if let sealed = info["c"] as? String, let requestId = info["r"] as? String,
      let key = Self.loadKey(),
      let preview = PreviewSeal.open(sealed, requestId: requestId, key: key)
    {
      content.body = preview
      Self.savePreview(requestId, preview)
    }
    contentHandler(content)
  }

  override func serviceExtensionTimeWillExpire() {
    if let handler, let content { handler(content) }
  }

  /// The key the app keeps (`E2EKey` in the app), from the shared group.
  static func loadKey() -> Data? {
    var query: [String: Any] = [
      kSecClass as String: kSecClassGenericPassword,
      kSecAttrService as String: "studio.connecto.siri.preview-key",
      kSecReturnData as String: true,
    ]
    if let group = Bundle.main.object(forInfoDictionaryKey: "ConnectoKeychainGroup") as? String {
      query[kSecAttrAccessGroup as String] = group
    }
    var out: CFTypeRef?
    guard SecItemCopyMatching(query as CFDictionary, &out) == errSecSuccess else { return nil }
    return out as? Data
  }

  /// For the app to show when the Mac is not reachable (`SiriStore.preview`).
  static func savePreview(_ requestId: String, _ text: String) {
    guard let defaults = UserDefaults(suiteName: "group.studio.connecto.chat") else { return }
    var all = defaults.dictionary(forKey: "siri.previews") as? [String: String] ?? [:]
    all[requestId] = text
    defaults.set(all, forKey: "siri.previews")
  }
}
