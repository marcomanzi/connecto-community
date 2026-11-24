import Cocoa
import FlutterMacOS

class MainFlutterWindow: NSWindow {
  override func awakeFromNib() {
    let flutterViewController = FlutterViewController()
    self.contentViewController = flutterViewController
    self.setContentSize(NSSize(width: 1000, height: 720))
    self.minSize = NSSize(width: 420, height: 560)
    self.center()
    self.appearance = NSAppearance(named: .darkAqua)
    self.title = "Connecto"

    RegisterGeneratedPlugins(registry: flutterViewController)

    super.awakeFromNib()
  }
}
