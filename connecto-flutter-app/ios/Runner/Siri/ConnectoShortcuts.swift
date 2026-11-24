import AppIntents

/// The phrases Siri knows without setup (§6). Italian ones are in
/// AppShortcuts.xcstrings. Six of the ten allowed.
struct ConnectoShortcuts: AppShortcutsProvider {
  static var shortcutTileColor: ShortcutTileColor = .teal

  static var appShortcuts: [AppShortcut] {
    AppShortcut(
      intent: AskConnectoIntent(),
      phrases: [
        "Ask \(.applicationName)",
        "Ask \(.applicationName) about \(\.$project)",
        "Tell \(.applicationName)",
        "Send a message to \(.applicationName)",
      ],
      shortTitle: LocalizedStringResource("Ask", table: "Siri"),
      systemImageName: "bubble.left.and.text.bubble.right")
    AppShortcut(
      intent: ProjectStatusIntent(),
      phrases: [
        "What is \(.applicationName) doing",
        "\(\.$project) status in \(.applicationName)",
        "Is \(.applicationName) done",
      ],
      shortTitle: LocalizedStringResource("Status", table: "Siri"),
      systemImageName: "waveform.path.ecg")
    AppShortcut(
      intent: LastReplyIntent(),
      phrases: [
        "Read the last \(.applicationName) reply",
        "What did \(.applicationName) say",
      ],
      shortTitle: LocalizedStringResource("Last reply", table: "Siri"),
      systemImageName: "text.bubble")
    AppShortcut(
      intent: StopAgentIntent(),
      phrases: [
        "Stop \(.applicationName)",
        "Stop the agent in \(.applicationName)",
        "Stop \(\.$project) in \(.applicationName)",
      ],
      shortTitle: LocalizedStringResource("Stop", table: "Siri"),
      systemImageName: "stop.circle")
    AppShortcut(
      intent: StartCallIntent(),
      phrases: [
        "Call \(.applicationName)",
        "Talk to \(\.$project) in \(.applicationName)",
      ],
      shortTitle: LocalizedStringResource("Call", table: "Siri"),
      systemImageName: "phone")
    AppShortcut(
      intent: OpenProjectIntent(),
      phrases: [
        "Open \(\.$project) in \(.applicationName)"
      ],
      shortTitle: LocalizedStringResource("Open", table: "Siri"),
      systemImageName: "folder")
  }
}
