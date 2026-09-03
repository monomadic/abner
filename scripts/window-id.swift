// Prints the CGWindowID of every on-screen abner window, one per line —
// the argument `screencapture -l` wants for a targeted capture:
//
//   screencapture -x -l "$(swift scripts/window-id.swift | head -1)" shot.png
//
// (CLAUDE.md's rule: verify visual changes by capturing the window,
// never by injecting global keystrokes.)
import CoreGraphics

let list = CGWindowListCopyWindowInfo([.optionOnScreenOnly, .excludeDesktopElements], kCGNullWindowID) as! [[String: Any]]
for w in list where (w["kCGWindowOwnerName"] as? String) == "abner" {
    print(w["kCGWindowNumber"] as! Int)
}
