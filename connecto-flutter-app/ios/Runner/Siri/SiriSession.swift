import AppIntents
import Foundation

/// The project's app session as `chat` reports it.
struct ProjectChat {
  let id: String?
  let provider: String?
  let status: String
  let question: String?
  let textAnswer: Bool
  let chatReady: Bool
  let messages: [ChatMessage]
  let revision: UInt64?

  init(_ v: [String: Any]) {
    id = v["id"] as? String
    provider = v["provider"] as? String
    status = v["status"] as? String ?? "New"
    question = v["question"] as? String
    textAnswer = v["textAnswer"] as? Bool ?? false
    chatReady = v["chatReady"] as? Bool ?? false
    messages = chatMessages(v["messages"])
    revision = (v["revision"] as? NSNumber)?.uint64Value
  }

  var hasSession: Bool { id != nil && status != "New" }
  var stopped: Bool { status == "Stopped" || status == "Failed" }
  var lastReply: String? { messages.last(where: { !$0.user })?.text }
}

/// Why a Siri request stops before it did anything: said as it is.
struct SiriStop: Error {
  let dialog: String
}

/// The budget of one Siri request (§3: about 8 s in all, speech included).
enum SiriBudget {
  static var total: Duration = .seconds(8)
  static var connect: Duration = .seconds(3)
  /// Kept for speaking what was found.
  static var speech: Duration = .milliseconds(1500)
}

/// One project, reached for one Siri request.
struct SiriTarget {
  let project: KnownProject
  let words: SiriWords
  let deadline: ContinuousClock.Instant

  var name: String { project.name }
  var fields: [String: Any] { ["host": project.host, "project": project.project] }

  /// The project the person named, else the last used, else the only one;
  /// else Siri asks which.
  static func resolve(
    _ param: IntentParameter<ProjectEntity?>, words: SiriWords = .current
  ) async throws -> KnownProject {
    if let chosen = param.wrappedValue { return chosen.known }
    let all = SiriStore.projects
    if let last = SiriStore.lastProject, let known = all.first(where: { $0.id == last }) {
      return known
    }
    if all.count == 1 { return all[0] }
    if all.isEmpty { throw SiriStop(dialog: words.noProjects) }
    let picked = try await param.requestDisambiguation(
      among: all.map(ProjectEntity.init),
      dialog: IntentDialog(stringLiteral: words.pick("Quale progetto?", "Which project?")))
    return picked.known
  }

  /// Starts the peer and reaches the project's host within the connect budget.
  static func reach(_ project: KnownProject, words: SiriWords = .current, notSentWording: Bool = false)
    async throws -> SiriTarget
  {
    let deadline = ContinuousClock.now + SiriBudget.total
    SiriStore.lastProject = project.id
    guard await SiriPeer.shared.connected(project.host, within: SiriBudget.connect) else {
      throw SiriStop(dialog: notSentWording ? words.hostUnreachable : words.hostUnreachableRead)
    }
    return SiriTarget(project: project, words: words, deadline: deadline)
  }

  func chat() async throws -> ProjectChat {
    do {
      return ProjectChat(try await SiriHost.call("chat", fields) as? [String: Any] ?? [:])
    } catch let failure as PeerFailure {
      throw SiriStop(dialog: failure.reason)
    }
  }

  var left: Duration { deadline - ContinuousClock.now }
}

/// What the app is asked to show when it comes to the front for Siri, kept
/// until Flutter takes it.
enum SiriOpen {
  case project(KnownProject)
  case call(KnownProject)
  /// One request's reply: its host and project come from
  /// `SiriStore.route`, the same for every tap, cold or warm.
  case reply(requestId: String)

  var payload: [String: Any] {
    switch self {
    case .project(let p): ["kind": "project", "host": p.host, "project": p.project, "name": p.name]
    case .call(let p): ["kind": "call", "host": p.host, "project": p.project, "name": p.name]
    case .reply(let id):
      {
        var out: [String: Any] = ["kind": "reply", "requestId": id]
        if let p = SiriStore.route(id) {
          out["host"] = p.host
          out["project"] = p.project
          out["name"] = p.name
        }
        if let preview = SiriStore.preview(id) { out["preview"] = preview }
        return out
      }()
    }
  }
}
