// Run with swift tool/make_icons.swift [contact-sheet-directory].
import AppKit
import CoreGraphics
import Foundation
import ImageIO
import UniformTypeIdentifiers

let app = URL(fileURLWithPath: #filePath).deletingLastPathComponent().deletingLastPathComponent()
let source = app.deletingLastPathComponent().appendingPathComponent("assets/brand/logo-soft.png")
let colorSpace = CGColorSpace(name: CGColorSpace.sRGB)!
guard let sourceImage = CGImageSourceCreateWithURL(source as CFURL, nil),
      let decoded = CGImageSourceCreateImageAtIndex(sourceImage, 0, nil),
      // The desktop icon uses these RGB samples as sRGB. Keep their values.
      let mark = decoded.copy(colorSpace: colorSpace) else {
    fatalError("Cannot read \(source.path)")
}

func color(_ rgb: UInt32) -> CGColor {
    CGColor(srgbRed: CGFloat((rgb >> 16) & 255) / 255,
            green: CGFloat((rgb >> 8) & 255) / 255,
            blue: CGFloat(rgb & 255) / 255,
            alpha: 1)
}

func canvas(_ width: Int, _ height: Int, opaque: Bool = false) -> CGContext {
    let alpha = opaque ? CGImageAlphaInfo.noneSkipLast : CGImageAlphaInfo.premultipliedLast
    let context = CGContext(
        data: nil, width: width, height: height, bitsPerComponent: 8,
        bytesPerRow: width * 4, space: colorSpace, bitmapInfo: alpha.rawValue
    )!
    context.interpolationQuality = .high
    return context
}

func rounded(_ context: CGContext, _ rect: CGRect, _ radius: CGFloat, _ fill: UInt32) {
    context.setFillColor(color(fill))
    context.addPath(CGPath(roundedRect: rect, cornerWidth: radius, cornerHeight: radius, transform: nil))
    context.fillPath()
}

func drawMark(in context: CGContext, logoSize: CGFloat) {
    let fit = logoSize / CGFloat(max(mark.width, mark.height))
    let width = CGFloat(mark.width) * fit
    let height = CGFloat(mark.height) * fit
    let markRect = CGRect(x: (1 - width) / 2, y: (1 - height) / 2, width: width, height: height)
    let mask = markRect.insetBy(dx: width * 0.008, dy: height * 0.014)
    context.saveGState()
    context.addPath(CGPath(roundedRect: mask, cornerWidth: width * 0.25,
                           cornerHeight: width * 0.25, transform: nil))
    context.clip()
    context.draw(mark, in: markRect)
    context.restoreGState()
}

func icon(_ pixels: Int, opaque: Bool = false) -> CGImage {
    let context = canvas(pixels, pixels, opaque: opaque)
    let size = CGFloat(pixels)
    context.scaleBy(x: size, y: size)
    if opaque {
        context.setFillColor(color(0x0b1118))
        context.fill(CGRect(x: 0, y: 0, width: 1, height: 1))
    }
    drawMark(in: context, logoSize: 0.94)

    let small = pixels <= 32
    let frame = CGRect(x: 0.600, y: 0.050, width: 0.354, height: 0.522)
    let ring: CGFloat = small ? 0.5 / size : 0.019
    rounded(context, frame.insetBy(dx: -ring, dy: -ring), 0.09 + ring, 0x0b1118)
    rounded(context, frame, 0.088, 0x5fe3ea)
    let stroke: CGFloat = small ? max(1.12 / size, 0.041) : 0.034
    let face = frame.insetBy(dx: stroke, dy: stroke)
    rounded(context, face, 0.055, 0x0b1118)
    if !small {
        let screen = CGRect(x: face.minX + 0.022, y: face.minY + 0.032,
                            width: face.width - 0.044, height: face.height - 0.10)
        rounded(context, screen, 0.033, 0x142934)
        let dot = CGRect(x: face.midX - 0.012, y: face.maxY - 0.042,
                         width: 0.024, height: 0.024)
        context.setFillColor(color(0xf0645a))
        context.fillEllipse(in: dot)
    }
    return context.makeImage()!
}

func png(_ image: CGImage) -> Data {
    let bytes = NSMutableData()
    let destination = CGImageDestinationCreateWithData(bytes, UTType.png.identifier as CFString, 1, nil)!
    CGImageDestinationAddImage(destination, image, nil)
    precondition(CGImageDestinationFinalize(destination))
    return bytes as Data
}

func write(_ image: CGImage, to path: URL) throws {
    try FileManager.default.createDirectory(at: path.deletingLastPathComponent(), withIntermediateDirectories: true)
    try png(image).write(to: path, options: .atomic)
}

let mac = app.appendingPathComponent("macos/Runner/Assets.xcassets/AppIcon.appiconset")
for size in [16, 32, 64, 128, 256, 512, 1024] {
    try write(icon(size), to: mac.appendingPathComponent("app_icon_\(size).png"))
}

let ios = app.appendingPathComponent("ios/Runner/Assets.xcassets/AppIcon.appiconset")
struct IconCatalog: Decodable {
    struct Entry: Decodable {
        let size: String
        let scale: String
        let filename: String
    }
    let images: [Entry]
}
let catalog = try JSONDecoder().decode(IconCatalog.self, from: Data(contentsOf: ios.appendingPathComponent("Contents.json")))
for entry in catalog.images {
    let width = Double(entry.size.split(separator: "x")[0])!
    let scale = Double(entry.scale.dropLast())!
    try write(icon(Int(width * scale), opaque: true), to: ios.appendingPathComponent(entry.filename))
}

let android = app.appendingPathComponent("android/app/src/main/res")
for (density, size) in [("mdpi", 48), ("hdpi", 72), ("xhdpi", 96), ("xxhdpi", 144), ("xxxhdpi", 192)] {
    try write(icon(size), to: android.appendingPathComponent("mipmap-\(density)/ic_launcher.png"))
}
try write(icon(1024), to: app.appendingPathComponent("assets/icon/app_icon.png"))

let markCanvas = canvas(256, 256)
markCanvas.scaleBy(x: 256, y: 256)
drawMark(in: markCanvas, logoSize: 1)
try write(markCanvas.makeImage()!, to: app.appendingPathComponent("assets/brand/mark-256.png"))

extension Data {
    mutating func appendLittleEndian<T: FixedWidthInteger>(_ number: T) {
        var littleEndian = number.littleEndian
        Swift.withUnsafeBytes(of: &littleEndian) { append(contentsOf: $0) }
    }
}

let windowsSizes = [16, 24, 32, 48, 64, 128, 256]
let windowsImages = windowsSizes.map { png(icon($0)) }
var ico = Data()
ico.appendLittleEndian(UInt16(0))
ico.appendLittleEndian(UInt16(1))
ico.appendLittleEndian(UInt16(windowsSizes.count))
var offset = 6 + windowsSizes.count * 16
for (size, image) in zip(windowsSizes, windowsImages) {
    let dimension = UInt8(truncatingIfNeeded: size)
    ico.append(contentsOf: [dimension, dimension, 0, 0])
    ico.appendLittleEndian(UInt16(1))
    ico.appendLittleEndian(UInt16(32))
    ico.appendLittleEndian(UInt32(image.count))
    ico.appendLittleEndian(UInt32(offset))
    offset += image.count
}
for image in windowsImages {
    ico.append(image)
}
try ico.write(to: app.appendingPathComponent("windows/runner/resources/app_icon.ico"), options: .atomic)

func drawLabel(_ text: String, at point: CGPoint, in context: CGContext, foreground: UInt32) {
    NSGraphicsContext.saveGraphicsState()
    NSGraphicsContext.current = NSGraphicsContext(cgContext: context, flipped: false)
    (text as NSString).draw(at: point, withAttributes: [
        .font: NSFont.monospacedSystemFont(ofSize: 16, weight: .medium),
        .foregroundColor: NSColor(cgColor: color(foreground))!,
    ])
    NSGraphicsContext.restoreGraphicsState()
}

if CommandLine.arguments.count == 2 {
    let shots = URL(fileURLWithPath: CommandLine.arguments[1], isDirectory: true)
    let sizes = [16, 32, 64, 128, 256, 1024]
    let rowHeight = 1080
    let width = sizes.reduce(0, +) + 44 * (sizes.count + 1)
    let sheet = canvas(width, rowHeight * 2, opaque: true)
    for (row, background, foreground) in [(0, UInt32(0xe7eaee), UInt32(0x201e27)),
                                           (1, 0x14181e, 0xe7eaee)] {
        let y = row * rowHeight
        sheet.setFillColor(color(background))
        sheet.fill(CGRect(x: 0, y: y, width: width, height: rowHeight))
        var x = 44
        for size in sizes {
            sheet.draw(icon(size), in: CGRect(x: x, y: y + 38, width: size, height: size))
            drawLabel("\(size)", at: CGPoint(x: x, y: y + 10), in: sheet, foreground: foreground)
            x += size + 44
        }
    }
    try write(sheet.makeImage()!, to: shots.appendingPathComponent("icon-contact-sheet.png"))
}
