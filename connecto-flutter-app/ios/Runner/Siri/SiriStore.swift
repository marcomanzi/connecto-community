import Foundation

/// A project of a paired host, as Siri names it.
struct KnownProject: Codable, Equatable, Hashable {
  let host: String
  let project: String
  let name: String
  var id: String { "\(host)/\(project)" }
}

/// A Siri question whose fate is not known yet: sent, but the host's answer
/// was lost, or accepted with a promise to notify.
struct OpenRequest: Codable, Equatable {
  let requestId: String
  let host: String
  let project: String
  let name: String
  let text: String
  /// True while it is not known whether the host received it (§5.4).
  var unconfirmed: Bool
  let at: Date
  /// What the host last said of it (`pending`, `answered`…), once asked.
  /// The record stays for the host's day of retention either way: it is
  /// the route a later notice's tap takes to the exact reply.
  var hostState: String? = nil
  /// While a Siri ask may still get its answer: the app does not reconcile
  /// it before then (it would find the host does not know it yet). An ask
  /// that dies leaves a mark that simply expires.
  var activeUntil: Date? = nil

  var active: Bool { activeUntil.map { $0 > Date() } ?? false }
}

/// An APNs token that replaced another: each paired host must hear of it
/// (`PushRegister` with the old token) before it can forget the old one.
struct TokenRotation: Codable, Equatable {
  let old: String
  let new: String
  var told: Set<String> = []
  let at: Date
}

/// What Siri keeps between runs: the projects as last seen, the last one
/// used, open requests and reply previews. In the App Group when this build
/// has one (the notification extension reads it too), else the app's own.
enum SiriStore {
  static let group = "group.studio.connecto.chat"

  /// Tests keep their own.
  nonisolated(unsafe) static var override: UserDefaults?

  static var defaults: UserDefaults {
    if let override { return override }
    if FileManager.default.containerURL(forSecurityApplicationGroupIdentifier: group) != nil,
      let shared = UserDefaults(suiteName: group)
    {
      return shared
    }
    return .standard
  }

  private static func read<T: Decodable>(_ key: String, as _: T.Type) -> T? {
    guard let data = defaults.data(forKey: key) else { return nil }
    return try? JSONDecoder().decode(T.self, from: data)
  }

  private static func write<T: Encodable>(_ key: String, _ value: T) {
    if let data = try? JSONEncoder().encode(value) { defaults.set(data, forKey: key) }
  }

  // MARK: projects

  static var projects: [KnownProject] {
    get { read("siri.projects", as: [KnownProject].self) ?? [] }
    set {
      let old = projects
      write("siri.projects", newValue)
      if old != newValue { ShortcutsRefresh.projectsChanged() }
    }
  }

  /// The projects of every connected host in a peer `state`, kept with
  /// those of hosts not connected now (their projects as last seen).
  static func remember(state: [String: Any]) {
    let hosts = state["hosts"] as? [[String: Any]] ?? []
    var seen: [KnownProject] = []
    var live = Set<String>()
    for host in hosts {
      guard let name = host["name"] as? String else { continue }
      guard host["link"] as? String == "Connected" else { continue }
      live.insert(name)
      for p in host["projects"] as? [[String: Any]] ?? [] {
        if let id = p["id"] as? String, let title = p["name"] as? String {
          seen.append(KnownProject(host: name, project: id, name: title))
        }
      }
    }
    let paired = Set(hosts.compactMap { $0["name"] as? String })
    let kept = projects.filter { !live.contains($0.host) && paired.contains($0.host) }
    projects = seen + kept
  }

  static var lastProject: String? {
    get { defaults.string(forKey: "siri.lastProject") }
    set { defaults.set(newValue, forKey: "siri.lastProject") }
  }

  // MARK: requests

  /// How long a request's route is kept after its ask: the host may work
  /// on it for 6 hours and then keeps the result a day after it completes,
  /// so the route outlives both (with an hour to spare).
  static let routeLifetime: TimeInterval = (6 + 24 + 1) * 3600

  /// Serializes every read-modify-write of the requests, so two intents
  /// (or an intent and the app) never drop each other's routes.
  private static let requestsLock = NSLock()

  static var openRequests: [OpenRequest] {
    read("siri.open", as: [OpenRequest].self) ?? []
  }

  /// Changes the requests under the lock: `change` sees the latest list.
  static func mutateRequests(_ change: (inout [OpenRequest]) -> Void) {
    requestsLock.withLock {
      var all = openRequests
      change(&all)
      let cutoff = Date().addingTimeInterval(-routeLifetime)
      write("siri.open", all.filter { $0.at > cutoff })
    }
  }

  static func save(_ request: OpenRequest) {
    mutateRequests { all in
      all.removeAll { $0.requestId == request.requestId }
      all.append(request)
    }
  }

  static func forget(_ requestId: String) {
    mutateRequests { $0.removeAll { $0.requestId == requestId } }
  }

  /// Records what the host said of a request, keeping its route.
  static func reconcile(_ requestId: String, hostState: String) {
    mutateRequests { all in
      guard let i = all.firstIndex(where: { $0.requestId == requestId }) else { return }
      all[i].unconfirmed = false
      all[i].hostState = hostState
    }
  }

  /// Where a request's reply lives: every tap (notice, Live Activity, cold
  /// or warm start) resolves it here.
  static func route(_ requestId: String) -> KnownProject? {
    guard let o = openRequests.first(where: { $0.requestId == requestId }) else { return nil }
    return projects.first { $0.host == o.host && $0.project == o.project }
      ?? KnownProject(host: o.host, project: o.project, name: o.name)
  }

  // MARK: token rotations

  private static let rotationsLock = NSLock()

  static var rotations: [TokenRotation] {
    get { read("siri.rotations", as: [TokenRotation].self) ?? [] }
    set { mutateRotations { $0 = newValue } }
  }

  static func mutateRotations(_ change: (inout [TokenRotation]) -> Void) {
    rotationsLock.withLock {
      var all = read("siri.rotations", as: [TokenRotation].self) ?? []
      change(&all)
      // A host gone a month is not waited for.
      let cutoff = Date().addingTimeInterval(-30 * 24 * 3600)
      write("siri.rotations", all.filter { $0.at > cutoff })
    }
  }

  /// The old tokens `host` has not been told were replaced, oldest first.
  static func untold(_ host: String) -> [TokenRotation] {
    rotations.filter { !$0.told.contains(host) }
  }

  /// `host` heard of `rotation`; once every paired host has, it goes.
  static func told(_ host: String, _ rotation: TokenRotation, paired: Set<String>) {
    mutateRotations { all in
      all = all.compactMap { r in
        guard r.old == rotation.old && r.new == rotation.new else { return r }
        var r = r
        r.told.insert(host)
        return paired.isSubset(of: r.told) ? nil : r
      }
    }
  }

  /// Reply previews the notification extension decrypted, by request id.
  static func preview(_ requestId: String) -> String? {
    (defaults.dictionary(forKey: "siri.previews") as? [String: String])?[requestId]
  }

  static func savePreview(_ requestId: String, _ text: String) {
    var all = defaults.dictionary(forKey: "siri.previews") as? [String: String] ?? [:]
    all[requestId] = text
    if all.count > 50 { all = Dictionary(uniqueKeysWithValues: all.suffix(50)) }
    defaults.set(all, forKey: "siri.previews")
  }
}

/// Tells the system the projects Siri can name have changed.
enum ShortcutsRefresh {
  static func projectsChanged() {
    ConnectoShortcuts.updateAppShortcutParameters()
  }
}
