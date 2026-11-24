import UserNotifications
import XCTest

@testable import Runner

final class SiriTests: XCTestCase {
  private let en = SiriWords(italian: false)
  private let projects = [
    KnownProject(host: "Mac", project: "p1", name: "Connecto"),
    KnownProject(host: "Mac", project: "p2", name: "connecto-site"),
    KnownProject(host: "Studio", project: "p3", name: "Caffè"),
  ]

  // MARK: ProjectQuery

  func testAnExactNameWinsOverOnesThatContainIt() {
    XCTAssertEqual(ProjectQuery.matches("CONNECTO", in: projects).map(\.project), ["p1"])
  }

  func testNamesMatchWithoutCaseAccentsOrDashes() {
    XCTAssertEqual(ProjectQuery.matches("caffe", in: projects).map(\.project), ["p3"])
    XCTAssertEqual(ProjectQuery.matches("connecto site", in: projects).map(\.project), ["p2"])
    XCTAssertEqual(ProjectQuery.matches("studio caffe", in: projects).map(\.project), ["p3"])
    XCTAssertEqual(ProjectQuery.matches("conn", in: projects).map(\.project), ["p1", "p2"])
    XCTAssertTrue(ProjectQuery.matches("  ", in: projects).isEmpty)
  }

  // MARK: Speech (speakable in lib/call.dart)

  func testMarkdownCodeAndLinksAreNotReadOut() {
    let said = speakable(
      "## Done\n- **Tests** pass, see https://example.com/x\n```\nlet x = 1\n```\nUse `cargo test`.", en)
    XCTAssertEqual(said, "Done. Tests pass, see a link. There is code in the chat. Use cargo test.")
  }

  func testTheFirstSentences() {
    XCTAssertEqual(firstSentences("One. Two! Three? Four.", 2), "One. Two!")
    XCTAssertEqual(firstSentences("Only one", 2), "Only one")
  }

  private func long(_ n: Int) -> String {
    (1...n).map { "Sentence number \($0) is here." }.joined(separator: " ")
  }

  func testAShortReplyIsSaidAsItIs() async {
    let said = await spokenWithin("All tests pass.", deadline: .now + .seconds(5), words: en) { _, _ in
      XCTFail("a short reply is never condensed")
      return nil
    }
    XCTAssertEqual(said, "All tests pass.")
  }

  func testALongReplyNearTheDeadlineIsCutWithoutTheModel() async {
    let start = ContinuousClock.now
    let said = await spokenWithin(long(40), deadline: start + .seconds(1), words: en) { _, _ in
      XCTFail("under 2 s left the model is not asked")
      return nil
    }
    XCTAssertEqual(said, "Sentence number 1 is here. Sentence number 2 is here. The rest is in the chat.")
    XCTAssertLessThan(ContinuousClock.now - start, .milliseconds(200))
  }

  func testALongReplyIsCondensedWhenThereIsTime() async {
    let said = await spokenWithin(long(40), deadline: .now + .seconds(4), words: en) { _, _ in
      "**Forty** sentences, all fine"
    }
    XCTAssertEqual(said, "Forty sentences, all fine.")
  }

  /// §5.7: a long reply that arrives at 7 s still gives a dialog by 8 s,
  /// even when the model would take far longer.
  func testASlowModelNeverOverrunsTheBudget() async {
    let start = ContinuousClock.now
    let deadline = start + .seconds(2) + .milliseconds(500)
    let said = await spokenWithin(long(40), deadline: deadline, words: en) { _, _ in
      // Ignores cancellation, as a stuck model might.
      await blockedFor(5)
      return "too late"
    }
    XCTAssertEqual(said, "Sentence number 1 is here. Sentence number 2 is here. The rest is in the chat.")
    XCTAssertLessThan(ContinuousClock.now, deadline)
  }

  // MARK: Preview seal (what the Mac's siri::seal makes)

  /// Made by connecto_net::siri::seal(key, "req-42", "Tutti i test passano ✅").
  private let vector = "Y+xjQN7hM73j0944zjMPLwU7n7FmxvX+x+RxQD5vU9nKLHMpeS1Q7JVo6mx0E7+PNndOpQ=="
  private let key = Data(base64Encoded: "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=")!

  func testThePhoneOpensAPreviewTheMacSealed() {
    XCTAssertEqual(PreviewSeal.open(vector, requestId: "req-42", key: key), "Tutti i test passano ✅")
  }

  func testAPreviewDoesNotOpenUnderAnotherRequest() {
    XCTAssertNil(PreviewSeal.open(vector, requestId: "req-43", key: key))
    XCTAssertNil(PreviewSeal.open(vector, requestId: "req-42", key: Data(repeating: 7, count: 32)))
  }

  // MARK: Host calls within Siri's budget (review 5, 6)

  private func target(_ budget: Duration) -> SiriTarget {
    SiriTarget(
      project: KnownProject(host: "Mac", project: "p1", name: "connecto"), words: en,
      deadline: .now + budget)
  }

  override func tearDown() {
    SiriHost.call = { op, fields in try await SiriPeer.shared.call(op, fields) }
    SiriStore.override = nil
    SiriNotices.permission = {
      SiriNotices.Permission(await UNUserNotificationCenter.current().notificationSettings().authorizationStatus)
    }
    SiriNotices.schedule = { try await UNUserNotificationCenter.current().add($0) }
    super.tearDown()
  }

  /// A host that takes the question and never answers: Siri still answers
  /// in time, and says it could not confirm (never "not sent").
  func testAStalledHostIsUnconfirmedWithinTheBudget() async {
    let sent = Counter()
    SiriHost.call = { _, _ in
      sent.add()
      await blockedFor(30)  // ignores cancellation, as a blocked FFI call does
      return nil
    }
    let start = ContinuousClock.now
    let t = target(.seconds(4))
    let outcome = await SiriHost.ask(
      t, requestId: "r1", terminal: "7", text: "hi", question: nil, notify: false)
    XCTAssertEqual(outcome, .unconfirmed)
    XCTAssertLessThan(ContinuousClock.now, t.deadline)
    XCTAssertLessThan(ContinuousClock.now - start, .seconds(4))
    XCTAssertEqual(sent.value, 1)
  }

  /// A lost answer is asked again with the same id while time is left.
  func testALostAnswerIsRetriedWithTheSameId() async {
    let ids = Recorder()
    let attempts = Counter()
    SiriHost.call = { _, fields in
      ids.add(fields["requestId"] as? String ?? "")
      attempts.add()
      if attempts.value == 1 { throw PeerFailure(reason: "stream reset") }
      return ["outcome": "duplicate", "state": ["state": "pending", "notifies": true]]
    }
    let outcome = await SiriHost.ask(
      target(.seconds(6)), requestId: "r1", terminal: "7", text: "hi", question: nil, notify: false)
    XCTAssertEqual(outcome, .accepted(notifies: true, revision: nil))
    XCTAssertEqual(ids.values, ["r1", "r1"])
  }

  func testTheHostsTimeoutIsWhatSiriHasLeft() async {
    let timeouts = Recorder()
    SiriHost.call = { _, fields in
      timeouts.add("\(fields["timeoutMs"] ?? "none")")
      return ["outcome": "accepted", "notifies": false, "revision": 3]
    }
    _ = await SiriHost.ask(
      target(.seconds(5)), requestId: "r1", terminal: "7", text: "hi", question: nil, notify: false)
    let ms = Int(timeouts.values.first ?? "") ?? 0
    XCTAssertGreaterThan(ms, 2000)
    XCTAssertLessThan(ms, 5000)
  }

  func testADuplicateSaysWhatBecameOfIt() {
    func dup(_ state: [String: Any]) -> AskOutcome {
      SiriHost.decode(["outcome": "duplicate", "state": state], en)
    }
    // Pasted but Enter failed on the host: not "sent".
    XCTAssertEqual(dup(["state": "uncertain"]), .unconfirmed)
    XCTAssertEqual(dup(["state": "pending"]), .accepted(notifies: false, revision: nil))
    XCTAssertEqual(
      dup(["state": "answered", "notifies": true, "revision": 9]),
      .accepted(notifies: true, revision: 9))
    XCTAssertEqual(dup(["state": "failed", "why": "session"]), .failedAfterSubmit(en.sentSessionEnded))
  }

  func testNoReminderIsPromisedWhenItCannotBeScheduled() async {
    SiriNotices.schedule = { _ in throw PeerFailure(reason: "denied") }
    defer { SiriNotices.schedule = { try await UNUserNotificationCenter.current().add($0) } }
    let scheduled = await SiriNotices.remind(KnownProject(host: "Mac", project: "p1", name: "x"), en, within: .seconds(1))
    XCTAssertFalse(scheduled)
  }

  // MARK: Ambiguous dispatch (second review 3)

  /// The first attempt went with no answer; the retry is refused by Pause.
  /// That says nothing of the first: the id stays, unconfirmed.
  func testARefusedRetryKeepsAnAmbiguousFirstAttempt() async {
    let attempts = Counter()
    SiriHost.call = { _, _ in
      attempts.add()
      if attempts.value == 1 { throw PeerFailure(reason: "stream reset") }
      throw PeerFailure(reason: "Connecto is paused on that machine")
    }
    let outcome = await SiriHost.ask(
      target(.seconds(6)), requestId: "r1", terminal: "7", text: "hi", question: nil, notify: false)
    XCTAssertEqual(outcome, .unconfirmed)
    XCTAssertEqual(attempts.value, 2)
  }

  func testARefusalBeforeAnythingWentStillLetsItGo() async {
    SiriHost.call = { _, _ in throw PeerFailure(reason: "Connecto is paused on that machine") }
    let outcome = await SiriHost.ask(
      target(.seconds(6)), requestId: "r1", terminal: "7", text: "hi", question: nil, notify: false)
    XCTAssertEqual(outcome, .refused(en.paused))
  }

  func testAJobThatFailedAfterItWentIsNotNotSent() {
    func failed(_ why: String) -> AskOutcome {
      SiriHost.decode(["outcome": "duplicate", "state": ["state": "failed", "why": why]], en)
    }
    XCTAssertEqual(failed("session"), .failedAfterSubmit(en.sentSessionEnded))
    XCTAssertEqual(failed("timeout"), .failedAfterSubmit(en.sentNoReply))
    XCTAssertEqual(failed("unmatched"), .failedAfterSubmit(en.sentUnmatched))
    // The host gave up on one it was unsure had gone: still unsure.
    XCTAssertEqual(failed("not_sent"), .unconfirmed)
  }

  // MARK: Third review 1, 2

  private static let readyChat: [String: Any] = [
    "id": "7", "status": "Ready", "chatReady": true, "revision": 1, "messages": [[true, "older"]],
  ]

  /// The first attempt reached the host but its answer was lost; after a
  /// reconnect the host says Siri is unavailable. The plain send carries no
  /// id: it must not run, or the question could be typed twice.
  func testAnUnsupportedRetryAfterALostAnswerNeverSendsAgain() async throws {
    SiriNotices.permission = { .denied }
    SiriStore.override = UserDefaults(suiteName: "siri-tests-\(UUID().uuidString)")!
    let asks = Counter()
    let sends = Counter()
    SiriHost.call = { op, _ in
      switch op {
      case "chat": return Self.readyChat
      case "siriAsk":
        asks.add()
        if asks.value == 1 { throw PeerFailure(reason: "stream reset") }
        return ["outcome": "unsupported"]
      case "send":
        sends.add()
        return nil
      default: return nil
      }
    }
    let result = try await SiriAsker.ask(target(.seconds(6)), text: "hi", answering: false)
    XCTAssertGreaterThanOrEqual(asks.value, 2)
    XCTAssertEqual(sends.value, 0, "the plain send ran after an ambiguous dispatch")
    XCTAssertEqual(result.dialog, en.unconfirmed)
    // Its route stays, for the app to reconcile.
    XCTAssertEqual(SiriStore.openRequests.count, 1)
    XCTAssertTrue(SiriStore.openRequests[0].unconfirmed)
    XCTAssertFalse(SiriStore.openRequests[0].active)
  }

  /// The app reconciles while the ask is still waiting for Accepted (the
  /// host answers "unknown" then) and removes the route; Accepted then
  /// arrives. The route must come back, so the notice opens the exact reply.
  func testAReconciliationBeforeAcceptedNeverLosesTheRoute() async throws {
    SiriNotices.permission = { .denied }
    SiriStore.override = UserDefaults(suiteName: "siri-tests-\(UUID().uuidString)")!
    let sawActive = Counter()
    SiriHost.call = { op, _ in
      switch op {
      case "chat": return Self.readyChat
      case "siriAsk":
        guard let open = SiriStore.openRequests.first else { return nil }
        if open.active { sawActive.add() }
        // The app's reconciliation, as an older app (or a race) would do it.
        SiriStore.forget(open.requestId)
        return ["outcome": "accepted", "notifies": true, "revision": 2]
      case "siriResult": return ["state": "pending"]
      default: return nil
      }
    }
    _ = try await SiriAsker.ask(target(.seconds(3)), text: "hi", answering: false)
    XCTAssertEqual(sawActive.value, 1, "the ask was not marked active while it waited")
    let open = try XCTUnwrap(SiriStore.openRequests.first, "the route was lost")
    XCTAssertEqual(open.hostState, "pending")
    let payload = SiriOpen.reply(requestId: open.requestId).payload
    XCTAssertEqual(payload["host"] as? String, "Mac")
    XCTAssertEqual(payload["project"] as? String, "p1")
  }

  /// The system's notification settings can answer far later than Siri
  /// waits: a late answer is "no", in time.
  func testSlowNotificationSettingsDoNotHoldSiri() async {
    SiriNotices.permission = {
      await blockedFor(30)
      return .allowed
    }
    let start = ContinuousClock.now
    let permission = await SiriNotices.permitted()
    XCTAssertEqual(permission, .unknown)
    XCTAssertLessThan(ContinuousClock.now - start, .seconds(1))
  }

  /// A prompt not answered yet is not "notifications are off".
  func testOnlyATurnedOffPermissionIsDenied() {
    XCTAssertEqual(SiriNotices.Permission(.notDetermined), .unknown)
    XCTAssertEqual(SiriNotices.Permission(.denied), .denied)
    XCTAssertEqual(SiriNotices.Permission(.authorized), .allowed)
    XCTAssertEqual(SiriNotices.Permission(.provisional), .allowed)
    XCTAssertEqual(SiriNotices.Permission(.ephemeral), .allowed)
  }

  // MARK: Fourth review 3: the reminder within Siri's budget

  /// An old host (no notice from the host), so the ask ends on the
  /// fallback: what it says, and how long it takes.
  private func fallbackAsk(_ budget: Duration) async throws -> (String, Duration) {
    SiriStore.override = UserDefaults(suiteName: "siri-tests-\(UUID().uuidString)")!
    SiriHost.call = { op, _ in
      switch op {
      case "chat": return Self.readyChat
      case "siriAsk": return ["outcome": "unsupported"]
      default: return nil
      }
    }
    let start = ContinuousClock.now
    let result = try await SiriAsker.ask(target(budget), text: "hi", answering: false)
    return (result.dialog, ContinuousClock.now - start)
  }

  func testAStalledReminderNeverHoldsSiriNorIsPromised() async throws {
    SiriNotices.permission = { .allowed }
    SiriNotices.schedule = { _ in await blockedFor(30) }
    let (said, took) = try await fallbackAsk(.seconds(3))
    XCTAssertEqual(said, en.sentNeutral)
    XCTAssertLessThan(took, .seconds(3))
  }

  func testAnUnknownPermissionNeitherPromisesNorSaysNotificationsAreOff() async throws {
    SiriNotices.permission = {
      await blockedFor(30)
      return .allowed
    }
    SiriNotices.schedule = { _ in }
    let (said, took) = try await fallbackAsk(.seconds(3))
    XCTAssertEqual(said, en.sentNeutral)
    XCTAssertLessThan(took, .seconds(3))
  }

  func testTheReminderIsPromisedOnlyWhenScheduledAndAllowed() async throws {
    SiriNotices.permission = { .allowed }
    SiriNotices.schedule = { _ in }
    let reminded = try await fallbackAsk(.seconds(3)).0
    XCTAssertEqual(reminded, en.sentWithReminder)
    SiriNotices.permission = { .denied }
    let off = try await fallbackAsk(.seconds(3)).0
    XCTAssertEqual(off, en.sentNotificationsOff)
  }

  // MARK: An old host's reply is never guessed (second review 5)

  /// The phone's copy was two messages behind (revision 10) when the
  /// question went; it then catches up with an earlier identical prompt and
  /// its answer (revision 12). With no boundary from the host, nothing is
  /// said: the reminder takes over.
  func testAnOldHostsLaggingCopyIsNotSpokenAsTheReply() async {
    let chats = Counter()
    SiriHost.call = { op, _ in
      if op == "chat" {
        chats.add()
        return [
          "id": "7", "status": "Ready", "chatReady": true, "revision": 12,
          "messages": [[true, "older"], [true, "tests?"], [false, "old answer"]],
        ]
      }
      return nil
    }
    let before = ProjectChat([
      "id": "7", "status": "Ready", "chatReady": true, "revision": 10,
      "messages": [[true, "older"]],
    ])
    let start = ContinuousClock.now
    let reply = await SiriAsker.replyInTime(target(.seconds(3)), requestId: nil, before: before, text: "tests?")
    XCTAssertNil(reply)
    XCTAssertLessThan(ContinuousClock.now - start, .milliseconds(200))
    XCTAssertEqual(chats.value, 0)
  }

  // MARK: Routes outlive the host's result window (second review 6)

  func testARouteOutlivesADayAfterItsAsk() {
    SiriStore.override = UserDefaults(suiteName: "siri-tests-\(UUID().uuidString)")!
    // Asked 25 h ago; the host worked on it for hours and keeps the result
    // a day after that.
    SiriStore.save(
      OpenRequest(
        requestId: "slow", host: "Mac", project: "p1", name: "connecto", text: "q",
        unconfirmed: false, at: Date().addingTimeInterval(-25 * 3600)))
    SiriStore.save(
      OpenRequest(
        requestId: "later", host: "Mac", project: "p1", name: "connecto", text: "q2",
        unconfirmed: false, at: Date()))
    XCTAssertNotNil(SiriStore.route("slow"))
  }

  func testOverlappingWritesKeepEveryRoute() async {
    SiriStore.override = UserDefaults(suiteName: "siri-tests-\(UUID().uuidString)")!
    await withTaskGroup(of: Void.self) { group in
      for i in 0..<64 {
        group.addTask {
          SiriStore.save(
            OpenRequest(
              requestId: "r\(i)", host: "Mac", project: "p1", name: "connecto", text: "q",
              unconfirmed: true, at: Date()))
          SiriStore.reconcile("r\(i)", hostState: "pending")
        }
      }
    }
    XCTAssertEqual(Set(SiriStore.openRequests.map(\.requestId)).count, 64)
    XCTAssertTrue(SiriStore.openRequests.allSatisfy { $0.hostState == "pending" })
  }

  // MARK: Token rotation and activities (review 11)

  func testARotationReachesAHostThatWasAway() async {
    let defaults = UserDefaults(suiteName: "siri-tests-\(UUID().uuidString)")!
    SiriStore.override = defaults
    defaults.set("old", forKey: "siri.apnsToken")
    SiriPush.rotate(from: "old", to: "new")
    let paired: Set<String> = ["Mac", "Studio"]
    // Only the Mac is connected now.
    let mac = Recorder()
    await SiriPush.register(host: "Mac", project: "p1", paired: paired, destination: dest) { _, f in
      mac.add("\(f["oldDeviceToken"] ?? "-")>\(f["deviceToken"] ?? "")")
      return ["done": true]
    }
    XCTAssertEqual(mac.values, ["old>new", "->new"])
    XCTAssertEqual(SiriStore.untold("Studio").map(\.old), ["old"])
    // The Studio connects later and still hears of the old token.
    let studio = Recorder()
    await SiriPush.register(host: "Studio", project: "p9", paired: paired, destination: dest) { _, f in
      studio.add("\(f["oldDeviceToken"] ?? "-")>\(f["deviceToken"] ?? "")")
      return ["done": true]
    }
    XCTAssertEqual(studio.values, ["old>new", "->new"])
    XCTAssertTrue(SiriStore.rotations.isEmpty)
  }

  func testARotationIsKeptUntilTheHostAnswers() async {
    SiriStore.override = UserDefaults(suiteName: "siri-tests-\(UUID().uuidString)")!
    SiriPush.rotate(from: "old", to: "new")
    await SiriPush.register(host: "Mac", project: "p1", paired: ["Mac"], destination: dest) { _, _ in
      throw PeerFailure(reason: "Host disconnected")
    }
    XCTAssertEqual(SiriStore.untold("Mac").map(\.old), ["old"])
  }

  func testANewRequestNeverTakesOverAnotherRequestsActivity() {
    XCTAssertEqual(SiriActivity.toEnd(existing: ["first"], for: "second"), ["first"])
    XCTAssertEqual(SiriActivity.toEnd(existing: ["second"], for: "second"), [])
  }

  // MARK: Routes (review 7)

  func testEveryTapFindsTheSameRoute() {
    SiriStore.override = UserDefaults(suiteName: "siri-tests-\(UUID().uuidString)")!
    SiriStore.save(
      OpenRequest(
        requestId: "first", host: "Mac", project: "p1", name: "connecto", text: "q",
        unconfirmed: true, at: Date()))
    SiriStore.reconcile("first", hostState: "pending")
    XCTAssertEqual(SiriStore.route("first"), KnownProject(host: "Mac", project: "p1", name: "connecto"))
    let payload = SiriOpen.reply(requestId: "first").payload
    XCTAssertEqual(payload["host"] as? String, "Mac")
    XCTAssertEqual(payload["project"] as? String, "p1")
  }

  // MARK: Links

  func testAReplyLinkNamesItsRequest() {
    XCTAssertEqual(ConnectoLinks.requestId(ConnectoLinks.reply("abc-1")), "abc-1")
    XCTAssertNil(ConnectoLinks.requestId(URL(string: "connecto-chat://project/x")!))
    XCTAssertNil(ConnectoLinks.requestId(URL(string: "https://reply/abc")!))
  }
}

final class Counter: @unchecked Sendable {
  private let lock = NSLock()
  private var n = 0
  func add() { lock.withLock { n += 1 } }
  var value: Int { lock.withLock { n } }
}

final class Recorder: @unchecked Sendable {
  private let lock = NSLock()
  private var items: [String] = []
  func add(_ s: String) { lock.withLock { items.append(s) } }
  var values: [String] { lock.withLock { items } }
}

/// A destination as `SiriPush.destination()` makes it, without the Keychain.
private let dest: [String: Any] = [
  "deviceToken": "new", "key": "AAECAwQFBgcI" + "CQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=", "env": "sandbox",
  "topic": "studio.connecto.connectoFlutterApp", "locale": "en",
]

/// Waits `seconds` ignoring cancellation, blocking a GCD thread as the real
/// peer's FFI call does (`SiriPeer.call`), never a Swift concurrency thread:
/// a stub that blocks one of those starves later tests.
func blockedFor(_ seconds: Double) async {
  await withCheckedContinuation { (done: CheckedContinuation<Void, Never>) in
    DispatchQueue.global().async {
      Thread.sleep(forTimeInterval: seconds)
      done.resume()
    }
  }
}
