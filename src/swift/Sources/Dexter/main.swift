import AppKit
import Darwin

// Disable stdout buffering so print() output appears immediately when stdout is
// redirected to a file (default block-buffering would otherwise hold output
// until the 8 KB buffer fills up or the process exits — useless for log tailing).
_ = setvbuf(stdout, nil, _IONBF, 0)

// Set activation policy BEFORE app.run() — this is the only reliable point.
// Dexter is now a normal Dock application so the operator can quit or restart
// it without finding the Terminal window that launched `make run`.
let app = NSApplication.shared
app.setActivationPolicy(.regular)

let delegate = DexterApp()
app.delegate = delegate
app.run()
