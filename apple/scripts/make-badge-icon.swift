// 生成"始终缓存"角标图标：apple/FileProvider/Resources/PinnedBadge.icns
// 用法：swift apple/scripts/make-badge-icon.swift <输出 .icns 路径>
import AppKit

let out = URL(fileURLWithPath: CommandLine.arguments.count > 1 ? CommandLine.arguments[1] : "PinnedBadge.icns")
let iconset = FileManager.default.temporaryDirectory.appendingPathComponent("PinnedBadge-\(getpid()).iconset")
try? FileManager.default.removeItem(at: iconset)
try FileManager.default.createDirectory(at: iconset, withIntermediateDirectories: true)

func render(_ px: Int) -> Data {
    let rep = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: px, pixelsHigh: px, bitsPerSample: 8,
                               samplesPerPixel: 4, hasAlpha: true, isPlanar: false,
                               colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)!
    NSGraphicsContext.saveGraphicsState()
    NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
    let s = CGFloat(px)
    let circle = NSBezierPath(ovalIn: NSRect(x: s * 0.04, y: s * 0.04, width: s * 0.92, height: s * 0.92))
    NSColor(calibratedRed: 0.0, green: 0.48, blue: 1.0, alpha: 1).setFill()
    circle.fill()
    NSColor.white.setStroke()
    circle.lineWidth = max(1, s * 0.06)
    circle.stroke()
    let cfg = NSImage.SymbolConfiguration(pointSize: s * 0.5, weight: .bold)
        .applying(.init(paletteColors: [.white]))
    if let sym = NSImage(systemSymbolName: "pin.fill", accessibilityDescription: nil)?.withSymbolConfiguration(cfg) {
        let sz = sym.size
        sym.draw(in: NSRect(x: (s - sz.width) / 2, y: (s - sz.height) / 2, width: sz.width, height: sz.height))
    }
    NSGraphicsContext.restoreGraphicsState()
    return rep.representation(using: .png, properties: [:])!
}

for base in [16, 32, 128, 256, 512] {
    try render(base).write(to: iconset.appendingPathComponent("icon_\(base)x\(base).png"))
    try render(base * 2).write(to: iconset.appendingPathComponent("icon_\(base)x\(base)@2x.png"))
}
let p = Process()
p.executableURL = URL(fileURLWithPath: "/usr/bin/iconutil")
p.arguments = ["-c", "icns", iconset.path, "-o", out.path]
try p.run()
p.waitUntilExit()
try? FileManager.default.removeItem(at: iconset)
print(p.terminationStatus == 0 ? "wrote \(out.path)" : "iconutil failed")
exit(p.terminationStatus)
