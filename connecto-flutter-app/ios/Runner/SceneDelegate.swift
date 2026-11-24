import Flutter
import UIKit

class SceneDelegate: FlutterSceneDelegate {
  // A Live Activity's tap: `connecto-chat://reply/<request id>`.
  override func scene(_ scene: UIScene, openURLContexts URLContexts: Set<UIOpenURLContext>) {
    for context in URLContexts {
      if let requestId = ConnectoLinks.requestId(context.url) {
        SiriBridge.open(.reply(requestId: requestId))
      }
    }
    super.scene(scene, openURLContexts: URLContexts)
  }

  override func scene(
    _ scene: UIScene, willConnectTo session: UISceneSession,
    options connectionOptions: UIScene.ConnectionOptions
  ) {
    super.scene(scene, willConnectTo: session, options: connectionOptions)
    // A cold start from the Live Activity.
    for context in connectionOptions.urlContexts {
      if let requestId = ConnectoLinks.requestId(context.url) {
        SiriBridge.open(.reply(requestId: requestId))
      }
    }
  }
}
