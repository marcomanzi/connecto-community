import CryptoKit
import Foundation

/// A reply preview as the host seals it for this phone (`siri::seal` in
/// connecto-net): base64 of a 12-byte nonce, the AES-256-GCM ciphertext and
/// its tag, with the request id as additional data, so a preview cannot be
/// shown under another request. Built into the app and the notification
/// service extension.
enum PreviewSeal {
  static func open(_ sealed: String, requestId: String, key: Data) -> String? {
    guard let combined = Data(base64Encoded: sealed),
      let box = try? AES.GCM.SealedBox(combined: combined),
      let plain = try? AES.GCM.open(
        box, using: SymmetricKey(data: key), authenticating: Data(requestId.utf8))
    else { return nil }
    return String(data: plain, encoding: .utf8)
  }
}
