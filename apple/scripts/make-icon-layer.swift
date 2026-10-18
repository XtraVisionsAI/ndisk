import AppKit
// 透明底的白色图形层，1024px；系统负责背景与质感。
let px = 1024
let rep = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: px, pixelsHigh: px, bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)!
NSGraphicsContext.saveGraphicsState(); NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
let s = CGFloat(px)
let cfg = NSImage.SymbolConfiguration(pointSize: s * 0.46, weight: .regular).applying(.init(paletteColors: [NSColor(calibratedRed: 0.12, green: 0.50, blue: 0.96, alpha: 1)]))
let sym = NSImage(systemSymbolName: "externaldrive.connected.to.line.below.fill", accessibilityDescription: nil)!.withSymbolConfiguration(cfg)!
let sz = sym.size
sym.draw(in: NSRect(x: (s - sz.width)/2, y: (s - sz.height)/2, width: sz.width, height: sz.height))
NSGraphicsContext.restoreGraphicsState()
try rep.representation(using: .png, properties: [:])!.write(to: URL(fileURLWithPath: CommandLine.arguments[1]))
print("glyph ok")
