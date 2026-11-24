import ActivityKit
import Foundation

/// The Live Activity of a Siri question (§5.5). Built into the app and the
/// Live Activity extension alike. One at a time: a new question updates the
/// one there is, so everything that changes is in the content state, which
/// holds nothing sensitive (no question, no reply).
struct ConnectoActivityAttributes: ActivityAttributes {
  struct ContentState: Codable, Hashable {
    /// The project's name.
    var project: String
    /// `sent`, `working`, `done`, `needsChoice` or `failed`.
    var state: String
    var requestId: String
    /// When the question was sent, seconds since 1970 (the host's pushes
    /// write it so).
    var startedAt: Double

    var started: Date { Date(timeIntervalSince1970: startedAt) }
  }
}

/// The link a tap on a notice or the Live Activity opens: that request's
/// reply.
enum ConnectoLinks {
  static let scheme = "connecto-chat"

  static func reply(_ requestId: String) -> URL {
    URL(string: "\(scheme)://reply/\(requestId)")!
  }

  /// The request id of a reply link.
  static func requestId(_ url: URL) -> String? {
    guard url.scheme == scheme, url.host == "reply" else { return nil }
    let id = url.lastPathComponent
    return id.isEmpty || id == "/" ? nil : id
  }
}
