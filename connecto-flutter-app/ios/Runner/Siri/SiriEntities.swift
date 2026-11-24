import AppIntents
import Foundation

/// A project of a paired host: `id` is `<host>/<project id>`.
struct ProjectEntity: AppEntity {
  static var typeDisplayRepresentation: TypeDisplayRepresentation = TypeDisplayRepresentation(
    name: LocalizedStringResource("Project", table: "Siri"))
  static var defaultQuery = ProjectQuery()

  let id: String
  let host: String
  let project: String
  let name: String

  init(_ known: KnownProject) {
    id = known.id
    host = known.host
    project = known.project
    name = known.name
  }

  var known: KnownProject { KnownProject(host: host, project: project, name: name) }

  var displayRepresentation: DisplayRepresentation {
    DisplayRepresentation(title: "\(name)", subtitle: "\(host)")
  }
}

/// The projects as the app last saw them (it writes them at each `state`),
/// so Siri can name one while the app is closed and the host not connected.
struct ProjectQuery: EntityStringQuery {
  init() {}

  func entities(for identifiers: [String]) async throws -> [ProjectEntity] {
    let all = SiriStore.projects
    return identifiers.compactMap { id in all.first { $0.id == id }.map(ProjectEntity.init) }
  }

  func entities(matching string: String) async throws -> [ProjectEntity] {
    Self.matches(string, in: SiriStore.projects).map(ProjectEntity.init)
  }

  func suggestedEntities() async throws -> [ProjectEntity] {
    SiriStore.projects.map(ProjectEntity.init)
  }

  /// Projects of `all` whose name, or host and name, match `string`,
  /// ignoring case and accents: exact names only, when there are any.
  static func matches(_ string: String, in all: [KnownProject]) -> [KnownProject] {
    let want = Self.fold(string)
    guard !want.isEmpty else { return [] }
    let exact = all.filter { Self.fold($0.name) == want }
    if !exact.isEmpty { return exact }
    return all.filter {
      Self.fold($0.name).contains(want) || Self.fold("\($0.host) \($0.name)").contains(want)
    }
  }

  static func fold(_ s: String) -> String {
    s.folding(options: [.caseInsensitive, .diacriticInsensitive, .widthInsensitive], locale: nil)
      .replacingOccurrences(of: "-", with: " ")
      .replacingOccurrences(of: "_", with: " ")
      .trimmingCharacters(in: .whitespacesAndNewlines)
  }
}

/// The agents a new app session can run.
enum AgentEnum: String, AppEnum {
  case claude, codex, pi, antigravity

  static var typeDisplayRepresentation: TypeDisplayRepresentation = TypeDisplayRepresentation(
    name: LocalizedStringResource("Agent", table: "Siri"))
  static var caseDisplayRepresentations: [AgentEnum: DisplayRepresentation] = [
    .claude: "Claude", .codex: "Codex", .pi: "Pi", .antigravity: "Antigravity",
  ]

  var label: String {
    switch self {
    case .claude: "Claude"
    case .codex: "Codex"
    case .pi: "Pi"
    case .antigravity: "Antigravity"
    }
  }
}
