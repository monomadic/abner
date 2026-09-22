// Render what macOS actually composites for an icon. Switchblade's
// packaging/check-icon.swift, plus one thing abner needs: it takes the icon SLOT
// (a PNG) as well as a built bundle, so a new render can be checked BEFORE it is
// built and installed.
//
//     swift packaging/check-icon.swift assets/app-icon.png [out.png]
//     swift packaging/check-icon.swift /Applications/Abner.app [out.png]
//
// macOS 26 does not draw a legacy .icns as authored: it re-renders it into the
// system tile, and artwork that isn't square, full-bleed, opaque and unmasked comes
// back SHRUNK inside a lighter plate (HISTORY.md #18; scripts/trim-icon.py is the
// fix). Nothing short of asking the OS shows it, which is why this exists.
//
// A PNG is wrapped in a throwaway bundle the same way build-app.sh's make_icns
// does it (sips to the standard sizes, iconutil), with its own bundle id, in a temp
// dir, never registered with LaunchServices.
//
// LOOK at the output: the artwork should reach the tile edge. If it sits inside a
// lighter rounded plate, the system shrank it.

import AppKit

let args = CommandLine.arguments
guard args.count >= 2 else {
    FileHandle.standardError.write("usage: check-icon.swift <Bundle.app | icon.png> [out.png]\n".data(using: .utf8)!)
    exit(2)
}
let target = args[1]
let out = args.count >= 3 ? args[2] : "icon-composite.png"
let px = 512

func run(_ tool: String, _ argv: [String]) {
    let p = Process()
    p.executableURL = URL(fileURLWithPath: tool)
    p.arguments = argv
    p.standardOutput = FileHandle.nullDevice
    try! p.run()
    p.waitUntilExit()
    guard p.terminationStatus == 0 else {
        FileHandle.standardError.write("error: \(tool) failed\n".data(using: .utf8)!)
        exit(1)
    }
}

// PNG → throwaway .app carrying it as AppIcon.icns.
var bundle = target
var scratch: URL? = nil
if target.lowercased().hasSuffix(".png") {
    let fm = FileManager.default
    let dir = fm.temporaryDirectory.appendingPathComponent("abner-check-icon-\(getpid())")
    let iconset = dir.appendingPathComponent("icon.iconset")
    let app = dir.appendingPathComponent("IconCheck.app")
    let res = app.appendingPathComponent("Contents/Resources")
    try! fm.createDirectory(at: iconset, withIntermediateDirectories: true)
    try! fm.createDirectory(at: res, withIntermediateDirectories: true)
    for size in [16, 32, 128, 256, 512] {
        run("/usr/bin/sips", ["-z", "\(size)", "\(size)", target, "--out", iconset.appendingPathComponent("icon_\(size)x\(size).png").path])
        run("/usr/bin/sips", ["-z", "\(size * 2)", "\(size * 2)", target, "--out", iconset.appendingPathComponent("icon_\(size)x\(size)@2x.png").path])
    }
    run("/usr/bin/iconutil", ["-c", "icns", iconset.path, "-o", res.appendingPathComponent("AppIcon.icns").path])
    let plist: [String: Any] = [
        "CFBundleIdentifier": "com.abner.iconcheck.\(getpid())",
        "CFBundleName": "IconCheck",
        "CFBundlePackageType": "APPL",
        "CFBundleIconFile": "AppIcon",
        "CFBundleExecutable": "stub",
    ]
    (plist as NSDictionary).write(to: app.appendingPathComponent("Contents/Info.plist"), atomically: true)
    // Without a runnable executable the OS badges the icon as unlaunchable.
    let macos = app.appendingPathComponent("Contents/MacOS")
    try! fm.createDirectory(at: macos, withIntermediateDirectories: true)
    let stub = macos.appendingPathComponent("stub")
    try! "#!/bin/sh\n".write(to: stub, atomically: true, encoding: .utf8)
    try! fm.setAttributes([.posixPermissions: 0o755], ofItemAtPath: stub.path)
    bundle = app.path
    scratch = dir
}
defer { if let scratch { try? FileManager.default.removeItem(at: scratch) } }

let icon = NSWorkspace.shared.icon(forFile: bundle)
guard let rep = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: px, pixelsHigh: px,
        bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true, isPlanar: false,
        colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0) else { exit(1) }
NSGraphicsContext.saveGraphicsState()
NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
icon.draw(in: NSRect(x: 0, y: 0, width: px, height: px))
NSGraphicsContext.restoreGraphicsState()

// There is deliberately no "how much did it shrink" number: the plate the system
// paints is opaque too, so any measurement of the composite alone reads the same
// whether the artwork fills the tile or sits shrunk inside a plate. The comparison
// that matters is against the SOURCE art, by eye.
try! rep.representation(using: .png, properties: [:])!.write(to: URL(fileURLWithPath: out))
print("\(target)")
print("  wrote  \(out)")
print("  Compare it with the source art. The artwork should reach the tile edge;")
print("  if it sits inside a lighter rounded plate, the system shrank it.")
