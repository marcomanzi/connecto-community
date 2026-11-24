import ActivityKit
import Foundation

/// The one Live Activity of Siri's questions: started (or reused) when the
/// host takes charge of a question, followed by the host's pushes through
/// its own push token, which the host is told of each time it changes.
enum SiriActivity {
  typealias Attributes = ConnectoActivityAttributes

  /// The activities to end before one for `requestId` starts: every other
  /// request's. An activity is never handed over to a newer request, as the
  /// older request's host holds its token and could end the newer one.
  static func toEnd(existing: [String], for requestId: String) -> [String] {
    existing.filter { $0 != requestId }
  }

  /// Starts the activity for `requestId` (ending any other first: one at a
  /// time), and tells the host its push token when one comes within 500 ms
  /// (later ones at the app's start, see [follow]). Bounded by `t`'s budget.
  static func start(_ t: SiriTarget, requestId: String) async {
    guard ActivityAuthorizationInfo().areActivitiesEnabled else { return }
    let ending = toEnd(
      existing: Activity<Attributes>.activities.map(\.content.state.requestId), for: requestId)
    for activity in Activity<Attributes>.activities
    where ending.contains(activity.content.state.requestId) {
      await activity.end(nil, dismissalPolicy: .immediate)
    }
    let state = Attributes.ContentState(
      project: t.name, state: "sent", requestId: requestId,
      startedAt: Date().timeIntervalSince1970)
    // Without push updates, it goes stale after 30 minutes and the app ends it.
    let content = ActivityContent(state: state, staleDate: Date().addingTimeInterval(30 * 60))
    guard
      let activity = Activity<Attributes>.activities.first(where: {
        $0.content.state.requestId == requestId
      })
        ?? (try? Activity.request(attributes: Attributes(), content: content, pushType: .token))
    else { return }
    let token = await firstToken(activity, within: .milliseconds(500))
    let left = t.left - SiriBudget.speech
    if let token, left > .zero {
      await tell(t.project, requestId: requestId, token: token, within: min(left, .seconds(1)))
    }
  }

  static func end(requestId: String) {
    Task {
      for activity in Activity<Attributes>.activities
      where activity.content.state.requestId == requestId {
        await activity.end(nil, dismissalPolicy: .immediate)
      }
    }
  }

  private static func firstToken(_ activity: Activity<Attributes>, within: Duration) async -> String? {
    if let token = activity.pushToken { return hex(token) }
    return await withTaskGroup(of: String?.self) { group in
      group.addTask {
        for await token in activity.pushTokenUpdates { return hex(token) }
        return nil
      }
      group.addTask {
        try? await Task.sleep(for: within)
        return nil
      }
      let first = await group.next() ?? nil
      group.cancelAll()
      return first
    }
  }

  private static func hex(_ data: Data) -> String { data.map { String(format: "%02x", $0) }.joined() }

  private static func tell(
    _ project: KnownProject, requestId: String, token: String, within: Duration = .seconds(10)
  ) async {
    _ = try? await SiriHost.bounded(
      "activityToken",
      ["host": project.host, "project": project.project, "requestId": requestId, "token": token],
      within: within)
  }

  /// At the app's start: ends a stale activity, and follows the live one's
  /// token, telling the host each new one (`ActivityTokenUpdate`).
  @MainActor
  static func follow() {
    for activity in Activity<Attributes>.activities {
      if let stale = activity.content.staleDate, stale < Date() {
        Task { await activity.end(nil, dismissalPolicy: .immediate) }
        continue
      }
      Task.detached {
        for await token in activity.pushTokenUpdates {
          let id = activity.content.state.requestId
          guard let route = SiriStore.route(id) else { continue }
          await tell(route, requestId: id, token: hex(token))
        }
      }
    }
  }
}
