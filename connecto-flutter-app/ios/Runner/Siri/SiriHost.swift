import Foundation
import UserNotifications

/// How a `SiriAsk` went (§5.1).
enum AskOutcome: Equatable {
  /// The host took it, typed it, and (with `notifies`) will tell this phone.
  case accepted(notifies: Bool, revision: UInt64?)
  /// Refused before anything was typed: what Siri says.
  case refused(String)
  /// Sent, but whether it reached the agent is not known: the host may have
  /// typed it. Kept for reconciliation, never sent again.
  case unconfirmed
  /// Nothing reached the host (it was not reachable in time).
  case notSent
  /// It was typed, and the job then failed on the host (its session went,
  /// or no reply came in time): not "not sent". Kept for its route.
  case failedAfterSubmit(String)

  /// Whether the host said for certain what became of this id. After an
  /// ambiguous dispatch only such an answer may let the id go. `unsupported`
  /// is not one: a host that took the first attempt may answer it after a
  /// reconnect, and its fallback, the plain send, carries no id and could
  /// type the question a second time.
  var authoritative: Bool {
    switch self {
    case .accepted, .failedAfterSubmit: true
    default: false
    }
  }
  /// A host from before Siri: nothing was sent.
  case unsupported
}

/// One call to the peer: the op, its fields, and how long it may take.
typealias PeerCall = @Sendable (String, [String: Any]) async throws -> Any?

/// What a call that ran out of time gives.
private struct TimedOut: Error {}

/// Siri's own messages to the host, through the peer's `siri*` operations.
/// Every call is bounded by what is left of Siri's budget, both in the peer
/// (`timeoutMs`, which the network calls honour) and here, so a stalled host
/// never holds Siri past its deadline.
enum SiriHost {
  /// The transport; tests give a stalled or scripted one.
  nonisolated(unsafe) static var call: PeerCall = { op, fields in
    try await SiriPeer.shared.call(op, fields)
  }

  /// The peer call `op` within `within`; `TimedOut` when it does not answer.
  static func bounded(_ op: String, _ fields: [String: Any], within: Duration) async throws -> Any? {
    var fields = fields
    fields["timeoutMs"] = max(0, Int(within / .milliseconds(1)))
    let box = SendableBox(fields)
    let result: Result<SendableBox, Error> = await withDeadline(within, fallback: .failure(TimedOut())) {
      do {
        return .success(SendableBox(try await call(op, box.value as? [String: Any] ?? [:])))
      } catch {
        return .failure(error)
      }
    }
    return try result.get().value
  }

  /// Sends the question once, and again with the same id after a lost
  /// answer while time is left: the host answers a repeat with `duplicate`
  /// and never types it twice.
  static func ask(
    _ t: SiriTarget, requestId: String, terminal: String, text: String, question: String?,
    notify: Bool
  ) async -> AskOutcome {
    var fields = t.fields
    fields["requestId"] = requestId
    fields["id"] = terminal
    fields["text"] = text
    if let question { fields["question"] = question }
    if notify, let destination = SiriPush.destination() { fields["notify"] = destination }
    var dispatched = false
    // Kept back: what Siri needs afterwards (a short wait, then speech).
    let reserve = SiriBudget.speech + .milliseconds(500)
    while t.left > reserve {
      let within = t.left - reserve
      let outcome: AskOutcome
      do {
        let answer = try await bounded("siriAsk", fields, within: within) as? [String: Any] ?? [:]
        outcome = decode(answer, t.words)
      } catch let failure as PeerFailure {
        outcome = failed(failure.reason, t.words)
      } catch {
        // Out of time after it went: it may have reached the host.
        outcome = .unconfirmed
      }
      switch outcome {
      // Once one attempt may have gone, only the host's word on this id
      // settles it: a retry refused (Pause, busy) or not sent says nothing
      // of the first, so the id is kept for reconciliation.
      case _ where dispatched && !outcome.authoritative && outcome != .unconfirmed:
        return .unconfirmed
      case .unconfirmed:
        dispatched = true
        try? await Task.sleep(for: .milliseconds(300))
        continue
      default:
        return outcome
      }
    }
    return dispatched ? .unconfirmed : .notSent
  }

  /// The host's answer to a `SiriAsk`.
  static func decode(_ answer: [String: Any], _ w: SiriWords) -> AskOutcome {
    let revision = (answer["revision"] as? NSNumber)?.uint64Value
    switch answer["outcome"] as? String {
    case "accepted":
      return .accepted(notifies: answer["notifies"] as? Bool ?? false, revision: revision)
    case "duplicate":
      // A repeat of one that went: what became of it, as the host has it.
      let state = answer["state"] as? [String: Any] ?? [:]
      switch state["state"] as? String {
      case "pending", "answered", "needs-answer", "needs-choice":
        return .accepted(
          notifies: state["notifies"] as? Bool ?? answer["notifies"] as? Bool ?? false,
          revision: (state["revision"] as? NSNumber)?.uint64Value)
      case "failed" where state["why"] as? String == "not_sent":
        // The host is not sure it went (its own uncertain, given up on).
        return .unconfirmed
      case "failed":
        switch state["why"] as? String {
        case "session": return .failedAfterSubmit(w.sentSessionEnded)
        case "timeout": return .failedAfterSubmit(w.sentNoReply)
        default: return .failedAfterSubmit(w.sentUnmatched)
        }
      default:
        return .unconfirmed
      }
    case "unsupported":
      return .unsupported
    case "not-sent":
      return .notSent
    case "unconfirmed":
      return .unconfirmed
    case "refused":
      switch answer["reason"] as? String {
      case "busy", "full": return .refused(w.busy)
      case "choice": return .refused(w.choiceOnScreen)
      case "not-ready": return .refused(w.notReady)
      default:
        let message = answer["message"] as? String ?? ""
        return .refused("\(w.notSent) \(message)".trimmingCharacters(in: .whitespaces))
      }
    default:
      return .unconfirmed
    }
  }

  /// A peer error: before the host was reached, nothing was sent.
  static func failed(_ reason: String, _ w: SiriWords) -> AskOutcome {
    let why = reason.lowercased()
    if why.contains("paused") || why.contains("in pausa") { return .refused(w.paused) }
    if why.contains("disconnected") || why.contains("not connected") { return .notSent }
    return .unconfirmed
  }

  /// The host's own result for this request: its reply, exactly, however
  /// the phone's copy of the chat lags. Nil when not known in time.
  static func result(_ t: SiriTarget, requestId: String, within: Duration) async -> [String: Any]? {
    var fields = t.fields
    fields["requestId"] = requestId
    return try? await bounded("siriResult", fields, within: within) as? [String: Any]
  }

  /// Siri read the reply: the host drops the notice it would send.
  static func delivered(_ t: SiriTarget, requestId: String) {
    var fields = t.fields
    fields["requestId"] = requestId
    let box = SendableBox(fields)
    Task.detached {
      _ = try? await bounded("siriDelivered", box.value as? [String: Any] ?? [:], within: .seconds(5))
    }
  }

  /// How long the app session has worked on its turn, when the host knows.
  static func workingSeconds(_ t: SiriTarget, terminal: String?) async -> Double? {
    guard let terminal else { return nil }
    var fields = t.fields
    fields["id"] = terminal
    let within = min(t.left - SiriBudget.speech, .seconds(2))
    guard let v = try? await bounded("siriStatus", fields, within: within) as? [String: Any] else {
      return nil
    }
    return (v["workingSecs"] as? NSNumber)?.doubleValue
  }
}

/// Local notifications: authorization, and the reminder of §5.4.
enum SiriNotices {
  /// Whether this app may notify, as far as the system says in time.
  enum Permission: Sendable, Equatable {
    case allowed, denied
    /// No answer in time, or no choice made yet: neither promised nor
    /// called off.
    case unknown

    /// "Off" only when the person turned them off: a prompt not answered
    /// yet (`notDetermined`), or a status iOS adds later, is `unknown`.
    init(_ status: UNAuthorizationStatus) {
      switch status {
      case .authorized, .provisional, .ephemeral: self = .allowed
      case .denied: self = .denied
      case .notDetermined: self = .unknown
      @unknown default: self = .unknown
      }
    }
  }

  /// The system's answer; tests give their own.
  nonisolated(unsafe) static var permission: @Sendable () async -> Permission = {
    Permission(await UNUserNotificationCenter.current().notificationSettings().authorizationStatus)
  }

  /// [permission] within `within`: the system's answer can take far longer
  /// than Siri waits (seen at 48 s). A late one is `unknown`: nothing is
  /// promised on it, and Siri does not say notifications are off either.
  static func permitted(within: Duration = .milliseconds(500)) async -> Permission {
    await withDeadline(within, fallback: .unknown) { await permission() }
  }

  static func authorized(within: Duration = .milliseconds(500)) async -> Bool {
    await permitted(within: within) == .allowed
  }

  /// Schedules the reminder; tests give their own.
  nonisolated(unsafe) static var schedule: @Sendable (UNNotificationRequest) async throws -> Void = {
    try await UNUserNotificationCenter.current().add($0)
  }

  /// "Check Connecto's reply in <project>" in three minutes. It names the
  /// project, never a request: an old host cannot look one up. True only
  /// once it is scheduled within `within` (what is left of Siri's budget),
  /// so Siri never promises a reminder that is not, nor waits for one.
  static func remind(_ project: KnownProject, _ w: SiriWords, within: Duration) async -> Bool {
    await withDeadline(within, fallback: false) { await scheduleReminder(project, w) }
  }

  private static func scheduleReminder(_ project: KnownProject, _ w: SiriWords) async -> Bool {
    let content = UNMutableNotificationContent()
    content.title = "Connecto · \(project.name)"
    content.body = w.reminderBody(project.name)
    content.threadIdentifier = project.project
    content.userInfo = [
      "kind": "reminder", "host": project.host, "project": project.project, "name": project.name,
    ]
    let request = UNNotificationRequest(
      identifier: "reminder-\(project.id)", content: content,
      trigger: UNTimeIntervalNotificationTrigger(timeInterval: 180, repeats: false))
    do {
      try await schedule(request)
      return true
    } catch {
      return false
    }
  }

  /// What Siri says when the reply is not read now and the host will not
  /// notify: a reminder only once one is scheduled and notifications are
  /// known to be allowed; "notifications are off" only when the system said
  /// so; otherwise words that promise nothing.
  static func fallback(
    _ project: KnownProject, _ w: SiriWords, permission: Permission, within: Duration
  ) async -> String {
    let scheduled =
      permission == .denied ? false : await remind(project, w, within: within)
    switch (permission, scheduled) {
    case (.allowed, true): return w.sentWithReminder
    case (.denied, _): return w.sentNotificationsOff
    default: return w.sentNeutral
    }
  }

  /// What a reminder may take at the end of an intent: never past Siri's
  /// deadline.
  static func reminderBudget(_ t: SiriTarget) -> Duration {
    max(.zero, min(t.left - .milliseconds(300), .seconds(1)))
  }
}
