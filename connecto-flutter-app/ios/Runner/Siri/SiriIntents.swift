import AppIntents
import Foundation
import SwiftUI
import UserNotifications

private func dialog(_ text: String) -> IntentDialog { IntentDialog(stringLiteral: text) }

/// The full text behind what Siri said.
struct ReplySnippet: View {
  let title: String
  let text: String
  var body: some View {
    VStack(alignment: .leading, spacing: 6) {
      Text(title).font(.headline)
      Text(text).font(.body).lineLimit(30)
    }
    .padding()
  }
}

// MARK: - Status

struct ProjectStatusIntent: AppIntent {
  static var title = LocalizedStringResource("Project status", table: "Siri")
  static var description = IntentDescription(
    LocalizedStringResource("Says what the project's agent is doing.", table: "Siri"))
  static var authenticationPolicy: IntentAuthenticationPolicy = .requiresLocalDeviceAuthentication

  @Parameter(title: LocalizedStringResource("Project", table: "Siri"))
  var project: ProjectEntity?

  func perform() async throws -> some IntentResult & ProvidesDialog {
    let w = SiriWords.current
    do {
      let target = try await SiriTarget.reach(try await SiriTarget.resolve($project, words: w), words: w)
      let chat = try await target.chat()
      let p = target.name
      let line: String
      if !chat.hasSession {
        line = w.noSession(p)
      } else if chat.stopped {
        line = w.sessionStopped(p)
      } else if let q = chat.question {
        line = chat.textAnswer ? w.waiting(p, speakable(q, w)) : w.choiceOnScreen
      } else if chat.status == "Running" {
        let secs = await SiriHost.workingSeconds(target, terminal: chat.id)
        line = w.working(p, minutes: secs.map { Int($0 / 60) })
      } else if chat.status == "Ready" {
        line = w.ready(p)
      } else {
        line = w.status(p, chat.status)
      }
      return .result(dialog: dialog(line))
    } catch let stop as SiriStop {
      return .result(dialog: dialog(stop.dialog))
    }
  }
}

// MARK: - Last reply

struct LastReplyIntent: AppIntent {
  static var title = LocalizedStringResource("Last reply", table: "Siri")
  static var description = IntentDescription(
    LocalizedStringResource("Reads the agent's last reply.", table: "Siri"))
  static var authenticationPolicy: IntentAuthenticationPolicy = .requiresLocalDeviceAuthentication

  @Parameter(title: LocalizedStringResource("Project", table: "Siri"))
  var project: ProjectEntity?

  func perform() async throws -> some IntentResult & ProvidesDialog & ShowsSnippetView {
    let w = SiriWords.current
    do {
      let target = try await SiriTarget.reach(try await SiriTarget.resolve($project, words: w), words: w)
      let chat = try await target.chat()
      guard chat.hasSession else { throw SiriStop(dialog: w.noSession(target.name)) }
      guard let reply = chat.lastReply else { throw SiriStop(dialog: w.noReply(target.name)) }
      let said = await spokenWithin(reply, deadline: target.deadline, words: w)
      return .result(
        dialog: dialog(w.lastSaid(target.name) + said),
        view: ReplySnippet(title: target.name, text: reply))
    } catch let stop as SiriStop {
      return .result(dialog: dialog(stop.dialog), view: ReplySnippet(title: "Connecto", text: stop.dialog))
    }
  }
}

// MARK: - Open

struct OpenProjectIntent: AppIntent {
  static var title = LocalizedStringResource("Open project", table: "Siri")
  static var description = IntentDescription(
    LocalizedStringResource("Opens the project's chat.", table: "Siri"))
  static var authenticationPolicy: IntentAuthenticationPolicy = .requiresLocalDeviceAuthentication
  /// iOS 17-25 brings the app forward this way; 26+ reads `supportedModes`.
  static var openAppWhenRun = true
  @available(iOS 26.0, *)
  static var supportedModes: IntentModes { .foreground(.immediate) }

  @Parameter(title: LocalizedStringResource("Project", table: "Siri"))
  var project: ProjectEntity?

  @MainActor
  func perform() async throws -> some IntentResult & ProvidesDialog {
    let w = SiriWords.current
    do {
      let known = try await SiriTarget.resolve($project, words: w)
      SiriStore.lastProject = known.id
      SiriBridge.open(.project(known))
      return .result(dialog: dialog(w.pick("Apro \(known.name).", "Opening \(known.name).")))
    } catch let stop as SiriStop {
      return .result(dialog: dialog(stop.dialog))
    }
  }
}

/// Opens the exact reply of one Siri request: a notification's or the Live
/// Activity's tap (§5.5). Not offered as a phrase.
struct OpenReplyIntent: AppIntent {
  static var title = LocalizedStringResource("Open reply", table: "Siri")
  static var isDiscoverable = false
  static var openAppWhenRun = true
  @available(iOS 26.0, *)
  static var supportedModes: IntentModes { .foreground(.immediate) }
  static var authenticationPolicy: IntentAuthenticationPolicy = .requiresLocalDeviceAuthentication

  @Parameter(title: LocalizedStringResource("Request", table: "Siri"))
  var requestId: String

  init() {}
  init(requestId: String) { self.requestId = requestId }

  @MainActor
  func perform() async throws -> some IntentResult {
    SiriBridge.open(.reply(requestId: requestId))
    return .result()
  }
}

// MARK: - Ask

/// The question went out, or did not; what Siri says about it.
struct AskResult {
  let dialog: String
  let snippet: String?
}

/// A menu choice is waiting: only the screen can answer it (ADR 0028).
struct ChoiceWaiting: Error {}

struct AskConnectoIntent: AppIntent, ForegroundContinuableIntent, LiveActivityIntent {
  static var title = LocalizedStringResource("Ask Connecto", table: "Siri")
  static var description = IntentDescription(
    LocalizedStringResource("Sends a message to the project's agent and says its reply.", table: "Siri"))
  static var authenticationPolicy: IntentAuthenticationPolicy = .requiresLocalDeviceAuthentication

  @Parameter(title: LocalizedStringResource("Project", table: "Siri"))
  var project: ProjectEntity?

  @Parameter(
    title: LocalizedStringResource("Request", table: "Siri"),
    requestValueDialog: IntentDialog(LocalizedStringResource("What do you want to ask Connecto?", table: "Siri")))
  var text: String

  func perform() async throws -> some IntentResult & ProvidesDialog & ShowsSnippetView {
    try await SiriAsker.perform(self, project: $project, text: text, answering: false)
  }
}

struct AnswerQuestionIntent: AppIntent, ForegroundContinuableIntent, LiveActivityIntent {
  static var title = LocalizedStringResource("Answer the agent", table: "Siri")
  static var description = IntentDescription(
    LocalizedStringResource("Answers the question the project's agent is asking.", table: "Siri"))
  static var authenticationPolicy: IntentAuthenticationPolicy = .requiresLocalDeviceAuthentication

  @Parameter(title: LocalizedStringResource("Project", table: "Siri"))
  var project: ProjectEntity?

  @Parameter(
    title: LocalizedStringResource("Answer", table: "Siri"),
    requestValueDialog: IntentDialog(LocalizedStringResource("What's your answer?", table: "Siri")))
  var text: String

  func perform() async throws -> some IntentResult & ProvidesDialog & ShowsSnippetView {
    try await SiriAsker.perform(self, project: $project, text: text, answering: true)
  }
}

extension ForegroundContinuableIntent {
  /// The shared body of Ask and Answer: a menu choice goes to the screen.
  fileprivate func continueOnScreen(_ project: KnownProject, _ w: SiriWords) -> Error {
    needsToContinueInForegroundError(dialog(w.choiceOnScreen)) {
      SiriBridge.open(.project(project))
    }
  }
}

enum SiriAsker {
  static func perform<I: ForegroundContinuableIntent>(
    _ intent: I, project param: IntentParameter<ProjectEntity?>, text: String, answering: Bool
  ) async throws -> some IntentResult & ProvidesDialog & ShowsSnippetView {
    let w = SiriWords.current
    var known: KnownProject?
    do {
      let project = try await SiriTarget.resolve(param, words: w)
      known = project
      let target = try await SiriTarget.reach(project, words: w, notSentWording: true)
      let result = try await ask(target, text: text, answering: answering)
      return .result(
        dialog: dialog(result.dialog),
        view: ReplySnippet(title: target.name, text: result.snippet ?? result.dialog))
    } catch let stop as SiriStop {
      return .result(dialog: dialog(stop.dialog), view: ReplySnippet(title: "Connecto", text: stop.dialog))
    } catch is ChoiceWaiting {
      throw intent.continueOnScreen(known!, w)
    }
  }

  /// The flow of §5.3: send (with the host taking charge of the notice when
  /// it can), wait for the reply while the budget lasts, then say it, or
  /// say honestly where it will be.
  static func ask(_ target: SiriTarget, text: String, answering: Bool) async throws -> AskResult {
    let w = target.words
    let before = try await target.chat()
    guard before.hasSession else { throw SiriStop(dialog: w.noSession(target.name)) }
    if before.stopped { throw SiriStop(dialog: w.sessionStopped(target.name)) }
    if before.question != nil && !before.textAnswer { throw ChoiceWaiting() }
    if answering && before.question == nil { throw SiriStop(dialog: w.noQuestion) }
    guard before.status == "Ready" || before.status == "Needs an answer", before.chatReady,
      let terminal = before.id
    else {
      throw SiriStop(
        dialog: before.status == "Running"
          ? "\(w.working(target.name, minutes: nil)) \(w.notReady)" : w.notReady)
    }
    let permission = await SiriNotices.permitted()
    let notifications = permission == .allowed
    let requestId = UUID().uuidString
    let asked = Date()
    /// The whole record, written as it is at each step: an upsert, so a
    /// route the app's reconciliation removed meanwhile comes back.
    func record(unconfirmed: Bool, hostState: String?, activeUntil: Date?) {
      var open = OpenRequest(
        requestId: requestId, host: target.project.host, project: target.project.project,
        name: target.name, text: text, unconfirmed: unconfirmed, at: asked)
      open.hostState = hostState
      open.activeUntil = activeUntil
      SiriStore.save(open)
    }
    // Saved before it goes: a lost answer is checked later, never resent.
    // Marked active until Siri's deadline (and a little after), so the app
    // does not reconcile it while this ask may still get its answer; after a
    // crash the mark simply expires.
    record(
      unconfirmed: true, hostState: nil,
      activeUntil: Date().addingTimeInterval(target.left / .seconds(1) + 5))

    var hostNotifies = false
    var tracked = false
    switch await SiriHost.ask(
      target, requestId: requestId, terminal: terminal, text: text, question: before.question,
      notify: notifications)
    {
    case .accepted(let notifies, _):
      tracked = true
      hostNotifies = notifies && notifications
      // Kept for the host's day: the route a notice's tap takes.
      record(unconfirmed: false, hostState: "pending", activeUntil: nil)
      if hostNotifies {
        await SiriActivity.start(target, requestId: requestId)
      }
    case .refused(let words):
      SiriStore.forget(requestId)
      throw SiriStop(dialog: words)
    case .notSent:
      SiriStore.forget(requestId)
      throw SiriStop(dialog: w.hostUnreachable)
    case .failedAfterSubmit(let words):
      // It went: its route stays for the chat and the host's record.
      record(unconfirmed: false, hostState: "failed", activeUntil: nil)
      throw SiriStop(dialog: words)
    case .unconfirmed:
      // Kept unconfirmed, and no longer active: the app asks the host what
      // became of it, and never sends it again by itself.
      record(unconfirmed: true, hostState: nil, activeUntil: nil)
      if permission != .denied {
        _ = await SiriNotices.remind(
          target.project, w, within: SiriNotices.reminderBudget(target))
      }
      return AskResult(dialog: w.unconfirmed, snippet: nil)
    case .unsupported:
      // A host from before Siri: the plain send, and no promise (§5.4).
      SiriStore.forget(requestId)
      do {
        var fields = target.fields
        fields["id"] = terminal
        fields["text"] = text
        if let q = before.question { fields["question"] = q }
        _ = try await SiriHost.call("send", fields)
      } catch let failure as PeerFailure {
        throw SiriStop(dialog: "\(w.notSent) \(failure.reason)")
      }
    }

    // The reply, if it comes while Siri still listens.
    if let full = await replyInTime(
      target, requestId: tracked ? requestId : nil, before: before, text: text)
    {
      let said = await spokenWithin(full, deadline: target.deadline, words: w)
      if tracked {
        // The host's own answer for this request was said: no notice.
        record(unconfirmed: false, hostState: "answered", activeUntil: nil)
        SiriHost.delivered(target, requestId: requestId)
        SiriActivity.end(requestId: requestId)
      }
      return AskResult(dialog: said, snippet: full)
    }
    if hostNotifies { return AskResult(dialog: w.accepted, snippet: text) }
    let said = await SiriNotices.fallback(
      target.project, w, permission: permission, within: SiriNotices.reminderBudget(target))
    return AskResult(dialog: said, snippet: text)
  }

  /// The reply while Siri still listens: only one the host attributes to
  /// this request (its result for it, from its own prompt position, board
  /// reports left out). An old host gives no such boundary, and the phone's
  /// copy of the chat may lag behind an earlier identical turn, so nothing
  /// is said for it: the reminder or the chat takes over (§5.4).
  static func replyInTime(
    _ target: SiriTarget, requestId: String?, before: ProjectChat, text: String
  ) async -> String? {
    guard let requestId else { return nil }
    while target.left > SiriBudget.speech {
      try? await Task.sleep(for: .milliseconds(250))
      let within = min(target.left - SiriBudget.speech, .seconds(2))
      guard let result = await SiriHost.result(target, requestId: requestId, within: within) else {
        continue
      }
      if result["state"] as? String == "answered", let reply = result["reply"] as? String,
        !reply.isEmpty
      {
        return reply
      }
    }
    return nil
  }
}

// MARK: - Stop

struct StopAgentIntent: AppIntent {
  static var title = LocalizedStringResource("Stop the agent", table: "Siri")
  static var description = IntentDescription(
    LocalizedStringResource("Asks the project's agent to stop its turn.", table: "Siri"))
  static var authenticationPolicy: IntentAuthenticationPolicy = .requiresLocalDeviceAuthentication

  @Parameter(title: LocalizedStringResource("Project", table: "Siri"))
  var project: ProjectEntity?

  func perform() async throws -> some IntentResult & ProvidesDialog {
    let w = SiriWords.current
    do {
      let target = try await SiriTarget.reach(try await SiriTarget.resolve($project, words: w), words: w)
      let chat = try await target.chat()
      guard chat.hasSession, chat.status == "Running", let id = chat.id else {
        throw SiriStop(dialog: w.notRunning)
      }
      // A turn that just began is stopped at once; one that has run a while
      // (or whose start is not known) is confirmed first.
      let secs = await SiriHost.workingSeconds(target, terminal: id)
      if secs.map({ $0 > 60 }) ?? true {
        try await requestConfirmation(
          result: .result(dialog: dialog(w.confirmStop(target.name))),
          confirmationActionName: .custom(
            acceptLabel: LocalizedStringResource("Stop", table: "Siri"), acceptAlternatives: [],
            denyLabel: LocalizedStringResource("Cancel", table: "Siri"), denyAlternatives: [],
            destructive: true))
      }
      var fields = target.fields
      fields["id"] = id
      do {
        _ = try await SiriPeer.shared.call("stop", fields)
      } catch let failure as PeerFailure {
        throw SiriStop(dialog: failure.reason)
      }
      // "Stopped" only once it is.
      let until = ContinuousClock.now + .seconds(2)
      while ContinuousClock.now < until {
        try? await Task.sleep(for: .milliseconds(200))
        if let now = try? await target.chat(), now.status != "Running" {
          return .result(dialog: dialog(w.stopped))
        }
      }
      return .result(dialog: dialog(w.stopAsked))
    } catch let stop as SiriStop {
      return .result(dialog: dialog(stop.dialog))
    }
  }
}

// MARK: - Call

struct StartCallIntent: AppIntent {
  static var title = LocalizedStringResource("Call the agent", table: "Siri")
  static var description = IntentDescription(
    LocalizedStringResource("Starts a voice call with the project.", table: "Siri"))
  static var authenticationPolicy: IntentAuthenticationPolicy = .requiresLocalDeviceAuthentication
  static var openAppWhenRun = true
  @available(iOS 26.0, *)
  static var supportedModes: IntentModes { .foreground(.immediate) }

  @Parameter(title: LocalizedStringResource("Project", table: "Siri"))
  var project: ProjectEntity?

  @MainActor
  func perform() async throws -> some IntentResult & ProvidesDialog {
    let w = SiriWords.current
    do {
      let known = try await SiriTarget.resolve($project, words: w)
      SiriStore.lastProject = known.id
      if #available(iOS 26.0, *) {
        SiriBridge.open(.call(known))
        return .result(dialog: dialog(w.pick("Chiamo \(known.name).", "Calling \(known.name).")))
      }
      SiriBridge.open(.project(known))
      return .result(dialog: dialog(w.callNeeds26(known.name)))
    } catch let stop as SiriStop {
      return .result(dialog: dialog(stop.dialog))
    }
  }
}

// MARK: - Session

struct StartSessionIntent: AppIntent {
  static var title = LocalizedStringResource("Start a session", table: "Siri")
  static var description = IntentDescription(
    LocalizedStringResource("Starts or resumes the project's app session.", table: "Siri"))
  static var authenticationPolicy: IntentAuthenticationPolicy = .requiresLocalDeviceAuthentication

  @Parameter(title: LocalizedStringResource("Project", table: "Siri"))
  var project: ProjectEntity?

  @Parameter(title: LocalizedStringResource("Agent", table: "Siri"), default: .claude)
  var agent: AgentEnum

  func perform() async throws -> some IntentResult & ProvidesDialog {
    let w = SiriWords.current
    do {
      let target = try await SiriTarget.reach(try await SiriTarget.resolve($project, words: w), words: w)
      let chat = try await target.chat()
      let call = { (op: String, extra: [String: Any]) async throws in
        do {
          _ = try await SiriPeer.shared.call(op, target.fields.merging(extra) { $1 })
        } catch let failure as PeerFailure {
          throw SiriStop(dialog: failure.reason)
        }
      }
      if !chat.hasSession {
        try await call("create", ["tool": agent.rawValue])
        return .result(dialog: dialog(w.started(agent.label)))
      }
      let running = chat.provider ?? agent.label
      guard let id = chat.id else { throw SiriStop(dialog: w.noSession(target.name)) }
      if running.caseInsensitiveCompare(agent.label) != .orderedSame {
        // Another agent's history goes with it: asked first.
        try await requestConfirmation(
          result: .result(dialog: dialog(w.otherAgent(running, agent.label))),
          confirmationActionName: .custom(
            acceptLabel: LocalizedStringResource("Start over", table: "Siri"), acceptAlternatives: [],
            denyLabel: LocalizedStringResource("Cancel", table: "Siri"), denyAlternatives: [],
            destructive: true))
        try await call("clear", ["id": id])
        // The host lists the new session once the old one is gone.
        let until = ContinuousClock.now + .seconds(3)
        while ContinuousClock.now < until, (try? await target.chat())?.hasSession == true {
          try? await Task.sleep(for: .milliseconds(200))
        }
        try await call("create", ["tool": agent.rawValue])
        return .result(dialog: dialog(w.started(agent.label)))
      }
      if chat.stopped {
        try await call("resume", ["id": id])
        return .result(dialog: dialog(w.resumed(running)))
      }
      return .result(dialog: dialog(w.alreadyRunning(running)))
    } catch let stop as SiriStop {
      return .result(dialog: dialog(stop.dialog))
    }
  }
}
