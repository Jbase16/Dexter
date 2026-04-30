import AppKit
import Darwin

// Disable stdout buffering so print() output appears immediately when stdout is
// redirected to a file (default block-buffering would otherwise hold output
// until the 8 KB buffer fills up or the process exits — useless for log tailing).
_ = setvbuf(stdout, nil, _IONBF, 0)

// Set activation policy BEFORE app.run() — this is the only reliable point.
// Calling it inside applicationDidFinishLaunching is too late: the run loop
// has already made its first activation decision, which can cause a transient
// Dock icon. Setting it here ensures .accessory from the very first frame.
let app = NSApplication.shared
app.setActivationPolicy(.accessory)

let delegate = DexterApp()
app.delegate = delegate
app.run()
