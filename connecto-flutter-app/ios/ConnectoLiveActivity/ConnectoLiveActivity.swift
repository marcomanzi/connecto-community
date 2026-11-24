import ActivityKit
import SwiftUI
import WidgetKit

@main
struct ConnectoWidgets: WidgetBundle {
  var body: some Widget {
    ConnectoLiveActivity()
  }
}

/// A Siri question's progress on the Lock Screen and in the Dynamic Island.
/// A tap opens its reply.
struct ConnectoLiveActivity: Widget {
  var body: some WidgetConfiguration {
    ActivityConfiguration(for: ConnectoActivityAttributes.self) { context in
      HStack(spacing: 12) {
        Image(systemName: Self.symbol(context.state.state))
          .font(.title2)
        VStack(alignment: .leading, spacing: 2) {
          Text("Connecto · \(context.state.project)").font(.headline)
          Text(Self.words(context.state.state)).font(.subheadline).foregroundStyle(.secondary)
        }
        Spacer()
        if Self.open(context.state.state) {
          Text(context.state.started, style: .timer)
            .font(.subheadline.monospacedDigit())
            .multilineTextAlignment(.trailing)
            .frame(maxWidth: 64)
        }
      }
      .padding()
      .widgetURL(ConnectoLinks.reply(context.state.requestId))
    } dynamicIsland: { context in
      DynamicIsland {
        DynamicIslandExpandedRegion(.leading) {
          Image(systemName: Self.symbol(context.state.state))
        }
        DynamicIslandExpandedRegion(.center) {
          Text(context.state.project).font(.headline)
        }
        DynamicIslandExpandedRegion(.bottom) {
          Text(Self.words(context.state.state)).font(.subheadline)
        }
      } compactLeading: {
        Image(systemName: Self.symbol(context.state.state))
      } compactTrailing: {
        if Self.open(context.state.state) {
          Text(context.state.started, style: .timer).monospacedDigit().frame(maxWidth: 44)
        } else {
          Image(systemName: "checkmark")
        }
      } minimal: {
        Image(systemName: Self.symbol(context.state.state))
      }
      .widgetURL(ConnectoLinks.reply(context.state.requestId))
    }
  }

  static func open(_ state: String) -> Bool { state == "sent" || state == "working" }

  static func symbol(_ state: String) -> String {
    switch state {
    case "done": "checkmark.circle.fill"
    case "needsChoice": "questionmark.circle.fill"
    case "failed": "exclamationmark.triangle.fill"
    default: "ellipsis.bubble"
    }
  }

  static func words(_ state: String) -> String {
    let italian = Locale.preferredLanguages.first?.hasPrefix("it") == true
    switch state {
    case "done": return italian ? "La risposta è pronta" : "The reply is ready"
    case "needsChoice": return italian ? "Aspetta una tua risposta" : "Waiting for you"
    case "failed": return italian ? "Non è andata" : "It didn't work"
    case "working": return italian ? "Sta lavorando" : "Working"
    default: return italian ? "Domanda inviata" : "Question sent"
    }
  }
}
