import Flutter
import Foundation

/// The link between Siri's intents and the Flutter app (`lib/siri.dart`):
/// the app hands over the projects it sees, and Siri asks it to show a
/// project, a reply or a call when it comes to the front.
enum SiriBridge {
  private static var channel: FlutterMethodChannel?
  /// What is waiting to be shown: kept until Flutter takes it, so a cold
  /// start opens it once the first page is up.
  private static var pending: [String: Any]?

  static func attach(_ messenger: FlutterBinaryMessenger) {
    let channel = FlutterMethodChannel(name: "connecto/siri", binaryMessenger: messenger)
    channel.setMethodCallHandler { call, result in
      let args = call.arguments as? [String: Any] ?? [:]
      switch call.method {
      case "state":
        SiriStore.remember(state: args)
        result(nil)
      case "takePending":
        result(pending)
        pending = nil
      case "openRequests":
        result(
          SiriStore.openRequests.map {
            [
              "requestId": $0.requestId, "host": $0.host, "project": $0.project, "name": $0.name,
              "text": $0.text, "unconfirmed": $0.unconfirmed, "active": $0.active,
            ]
          })
      case "reconcileRequest":
        if let id = args["requestId"] as? String, let state = args["hostState"] as? String {
          SiriStore.reconcile(id, hostState: state)
        }
        result(nil)
      case "forgetRequest":
        // Never one a Siri ask is still waiting on.
        if let id = args["requestId"] as? String,
          SiriStore.openRequests.first(where: { $0.requestId == id })?.active != true
        {
          SiriStore.forget(id)
        }
        result(nil)
      case "push":
        Task { @MainActor in result(await SiriPush.registration()) }
      case "registerPush":
        // A host connected: it learns where this phone's notices go.
        Task.detached { await SiriPush.registerWithHosts() }
        result(nil)
      default:
        result(FlutterMethodNotImplemented)
      }
    }
    Self.channel = channel
  }

  /// Shows `what` in the app: now when Flutter listens, else at its start.
  static func open(_ what: SiriOpen) {
    let payload = what.payload
    DispatchQueue.main.async {
      pending = payload
      channel?.invokeMethod("open", arguments: payload) { reply in
        // Taken: the app shows it now.
        if reply as? Bool == true { pending = nil }
      }
    }
  }
}
