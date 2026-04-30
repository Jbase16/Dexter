import AppKit
import ApplicationServices
import Foundation

// ── AX C callback ─────────────────────────────────────────────────────────────
//
// AXObserver callbacks must be plain C functions — they cannot be Swift closures.
// We pass an Unmanaged<EventBridge> pointer as `refcon` and reconstruct it here.
// This callback is registered on CFRunLoopGetMain(), so it always fires on the
// main thread — accessing EventBridge's state here is safe under EventBridge's
// threading contract (see class-level comment).
//
// Matches `AXObserverCallback`: (AXObserver, AXUIElement, CFString, UnsafeMutableRawPointer?) -> Void
private func axFocusCallback(
    _: AXObserver,
    _: AXUIElement,
    _: CFString,
    refcon: UnsafeMutableRawPointer?
) {
    guard let refcon else { return }
    Unmanaged<EventBridge>.fromOpaque(refcon).takeUnretainedValue().handleAXElementChanged()
}

// ── Hotkey CGEventTap C callback ──────────────────────────────────────────────
//
// CGEventTapCreate takes a CGEventTapCallBack — a C function pointer.
// Swift 6 forbids capturing context in C function pointer closures, so this must
// be a module-level free function (same pattern as axFocusCallback above).
// `self` is passed via `refcon` as an Unmanaged<EventBridge> pointer.
//
// Matches CGEventTapCallBack:
//   (CGEventTapProxy, CGEventType, CGEvent, UnsafeMutableRawPointer?) -> Unmanaged<CGEvent>?
//
// CGEventRef bridges to Swift as a non-optional CGEvent (a class type). Only
// `refcon` can be nil — guard defensively, even though this cannot happen in practice
// because `refcon` is set to `Unmanaged.passUnretained(self).toOpaque()` in `startHotkeyTap`.
//
// Returns nil for the hotkey chord (consumes the event — prevents it reaching
// the focused app). Returns the event unchanged for all other keypresses.
private func hotkeyTapCallback(
    _:      CGEventTapProxy,
    _:      CGEventType,
    event:  CGEvent,
    refcon: UnsafeMutableRawPointer?
) -> Unmanaged<CGEvent>? {
    guard let refcon else { return Unmanaged.passRetained(event) }
    // Ignore key-repeat events — macOS fires repeated keyDown at ~12 Hz while
    // a key is held. We only want the initial press, not the flood of repeats.
    guard event.getIntegerValueField(.keyboardEventAutorepeat) == 0 else {
        return nil  // consume the repeat silently
    }
    let bridge = Unmanaged<EventBridge>.fromOpaque(refcon).takeUnretainedValue()
    if bridge.isHotkeyEvent(event) {
        bridge.handleHotkeyActivated()
        return nil   // consume: do not forward to focused app
    }
    return Unmanaged.passRetained(event)
}

// ── EventBridge ───────────────────────────────────────────────────────────────

/// Bridges macOS system events to the Dexter gRPC session stream.
///
/// Observes:
/// - `NSWorkspace` app activation / deactivation  (app focus lifecycle)
/// - `DistributedNotificationCenter` screen lock / unlock  (loginwindow broadcasts)
/// - `AXObserver` focused element changes within the frontmost app  (AX element context)
/// - `CGEventTap` global keyDown events for the activation hotkey (Ctrl+Shift+Space)
///
/// **Threading contract**: All state is accessed exclusively from the main thread.
/// - NSWorkspace observers are registered with `operationQueue: .main`.
/// - DistributedNotificationCenter observers are registered with `operationQueue: .main`.
/// - AXObserver source is added to `CFRunLoopGetMain()`.
///
/// `@unchecked Sendable` is correct here: this class bridges Objective-C / C APIs
/// that predate Swift concurrency. Thread safety is guaranteed by the design above,
/// not by Swift's actor model. This is the same pattern Apple uses for their own
/// ObjC-bridging types.
final class EventBridge: @unchecked Sendable {

    // ── Constants ─────────────────────────────────────────────────────────────
    //
    // Mirrors Rust's CONTEXT_DEBOUNCE_MS constant. Update both together when tuning.
    private static let contextDebouncMs: Int = 150

    // Mirrors Rust's AX_VALUE_PREVIEW_MAX_CHARS. Update both together when tuning.
    private static let axValuePreviewMaxChars: Int = 200

    // Mirrors Rust's CLIPBOARD_MAX_CHARS. Update both together when tuning.
    private static let clipboardMaxChars: Int = 4_000

    // Mirrors Rust's CLIPBOARD_MIN_CHARS. Update both together when tuning.
    private static let clipboardMinChars: Int = 5

    // Mirrors Rust's CLIPBOARD_POLL_INTERVAL_MS.
    private static let clipboardPollIntervalMs: Int = 1_000

    // Privacy keywords: an AX label containing any of these → is_sensitive = true,
    // value is never read or transmitted.
    private static let sensitiveKeywords = ["password", "credit card", "cvv", "ssn"]

    // ── State (main thread only — see threading contract above) ───────────────

    private let sendEvent: @Sendable (Dexter_V1_ClientEvent) -> Void

    // NSWorkspace observer tokens.
    private var workspaceTokens:   [NSObjectProtocol] = []
    // DistributedNotificationCenter tokens for screen lock.
    private var distributedTokens: [NSObjectProtocol] = []

    // AX observer for the currently focused application.
    private var axObserver: AXObserver?
    private var observedPID: pid_t = 0

    // Debounce for AX focus-changed events. Cancelled + rescheduled on every callback;
    // fires after contextDebouncMs of silence.
    private var debounceWorkItem: DispatchWorkItem?

    // ── Clipboard polling (main thread only — see threading contract) ─────────
    //
    // NSPasteboard has no change-notification API; polling changeCount is the
    // standard pattern. -1 forces a read on the first poll tick — any real
    // changeCount value is ≥ 0, so the comparison will always differ initially.
    private var lastClipboardChangeCount: Int = -1
    private var clipboardTimer:           Timer?

    // ── Hotkey CGEventTap (main thread only — see threading contract) ─────────
    private var hotkeyTap:           CFMachPort?
    private var hotkeyRunLoopSource: CFRunLoopSource?

    // Hotkey detection parameters — initialized to Phase 16 hardcoded defaults.
    // Updated from ConfigSync (Rust → Swift) at session open via updateHotkeyConfig(_:).
    // All accesses on main thread: hotkeyTapCallback runs on main run loop.
    private var hotkeyKeyCode:        Int64 = 49    // kVK_Space
    private var hotkeyRequiresCtrl:   Bool  = true
    private var hotkeyRequiresShift:  Bool  = true
    private var hotkeyRequiresCmd:    Bool  = false
    private var hotkeyRequiresOption: Bool  = false

    // ── Init ──────────────────────────────────────────────────────────────────

    /// Creates an EventBridge that delivers events via `sendEvent`.
    ///
    /// The closure must be `@Sendable` because it is called from the main thread
    /// and creates a `Task` that crosses actor boundaries:
    /// ```swift
    /// EventBridge { [weak self] event in
    ///     Task { [weak self] in await self?.send(event) }
    /// }
    /// ```
    init(sendEvent: @escaping @Sendable (Dexter_V1_ClientEvent) -> Void) {
        self.sendEvent = sendEvent
    }

    // ── Lifecycle ─────────────────────────────────────────────────────────────

    /// Register all observers and begin event bridging.
    ///
    /// AX permission check: if `AXIsProcessTrustedWithOptions(nil)` returns false,
    /// a warning is logged and AXObserver registration is skipped. The bridge
    /// continues in degraded mode (NSWorkspace + screen lock only). This is common
    /// on development machines that have not been granted Accessibility access in
    /// System Settings → Privacy & Security → Accessibility.
    func start() {
        registerWorkspaceObservers()
        registerScreenLockObservers()
        startHotkeyTap()
        startClipboardPolling()

        if AXIsProcessTrustedWithOptions(nil) {
            // Observe the currently frontmost app immediately on session start.
            if let frontmost = NSWorkspace.shared.frontmostApplication {
                startAXObservation(for: frontmost.processIdentifier)
                emitAppFocused(app: frontmost, queryElement: true)
            }
        } else {
            print("[EventBridge] AX permission not granted — element observation disabled")
        }
    }

    /// Remove all observers, cancel any pending debounce, and nil AX references.
    ///
    /// Safe to call from any context. All cleanup runs synchronously on the main
    /// thread via DispatchQueue.main.async (no-op delay if called from main).
    func stop() {
        DispatchQueue.main.async { [weak self] in
            self?.performStop()
        }
    }

    // ── NSWorkspace observers ─────────────────────────────────────────────────

    private func registerWorkspaceObservers() {
        let nc = NSWorkspace.shared.notificationCenter

        let activated = nc.addObserver(
            forName: NSWorkspace.didActivateApplicationNotification,
            object:  nil,
            queue:   .main
        ) { [weak self] notification in
            guard let self,
                  let app = notification.userInfo?[NSWorkspace.applicationUserInfoKey]
                      as? NSRunningApplication
            else { return }
            self.handleAppActivated(app)
        }

        let deactivated = nc.addObserver(
            forName: NSWorkspace.didDeactivateApplicationNotification,
            object:  nil,
            queue:   .main
        ) { [weak self] _ in
            self?.sendSystemEvent(.appUnfocused)
        }

        workspaceTokens = [activated, deactivated]
    }

    // ── Screen lock observers ─────────────────────────────────────────────────
    //
    // NSWorkspace does NOT have a screen-lock notification — it has session events
    // (fast user switching) and display sleep events. Screen lock specifically
    // (Ctrl+Cmd+Q or Lock Screen menu) broadcasts via DistributedNotificationCenter.

    private func registerScreenLockObservers() {
        let dnc = DistributedNotificationCenter.default()

        let locked = dnc.addObserver(
            forName: NSNotification.Name("com.apple.screenIsLocked"),
            object:  nil,
            queue:   .main
        ) { [weak self] _ in
            self?.sendSystemEvent(.screenLocked)
        }

        let unlocked = dnc.addObserver(
            forName: NSNotification.Name("com.apple.screenIsUnlocked"),
            object:  nil,
            queue:   .main
        ) { [weak self] _ in
            self?.sendSystemEvent(.screenUnlocked)
        }

        distributedTokens = [locked, unlocked]
    }

    // ── Clipboard polling ─────────────────────────────────────────────────────

    /// Start NSPasteboard.changeCount polling at clipboardPollIntervalMs intervals.
    ///
    /// NSPasteboard has no change-notification API — polling changeCount is the
    /// standard macOS pattern used by every clipboard manager. Timer is added to
    /// `.common` runLoop mode so it fires during tracking and modal run-loop modes
    /// (menus, resize, scroll) as well as default mode — the operator can Cmd+C
    /// inside a menu and the change will be picked up on the next tick.
    private func startClipboardPolling() {
        let interval = Double(Self.clipboardPollIntervalMs) / 1_000.0
        let timer = Timer(timeInterval: interval, repeats: true) { [weak self] _ in
            self?.handleClipboardPoll()
        }
        RunLoop.main.add(timer, forMode: .common)
        clipboardTimer = timer
    }

    private func stopClipboardPolling() {
        clipboardTimer?.invalidate()
        clipboardTimer = nil
    }

    /// Called by clipboardTimer on each tick.
    ///
    /// Compares NSPasteboard.general.changeCount to the last seen value.
    /// On change: reads text content and emits CLIPBOARD_CHANGED when the text
    /// meets the minimum length threshold.
    ///
    /// All accesses on main thread: Timer fires on main run loop.
    private func handleClipboardPoll() {
        let pb           = NSPasteboard.general
        let currentCount = pb.changeCount
        guard currentCount != lastClipboardChangeCount else { return }
        lastClipboardChangeCount = currentCount

        // Read text-only content. Nil if pasteboard holds images, files, or rich-text-only data.
        guard let text = pb.string(forType: .string),
              text.count >= Self.clipboardMinChars else { return }

        // Truncate at clipboardMaxChars. Rust applies a secondary guard on ingestion.
        let content = text.count <= Self.clipboardMaxChars
            ? text
            : String(text.prefix(Self.clipboardMaxChars))

        sendSystemEvent(.clipboardChanged, payload: ["text": content])
    }

    // ── AX observation ────────────────────────────────────────────────────────

    /// Start observing `kAXFocusedUIElementChangedNotification` for the given PID.
    ///
    /// Tears down the previous observer before installing the new one.
    private func startAXObservation(for pid: pid_t) {
        stopAXObservation()
        observedPID = pid

        var observer: AXObserver?
        let status = AXObserverCreate(pid, axFocusCallback, &observer)
        guard status == .success, let observer else {
            print("[EventBridge] AXObserverCreate failed for pid \(pid): \(status.rawValue)")
            return
        }

        let appElement  = AXUIElementCreateApplication(pid)
        let notifName   = kAXFocusedUIElementChangedNotification as CFString
        let selfPointer = Unmanaged.passUnretained(self).toOpaque()

        let addStatus = AXObserverAddNotification(observer, appElement, notifName, selfPointer)
        guard addStatus == .success else {
            print("[EventBridge] AXObserverAddNotification failed: \(addStatus.rawValue)")
            return
        }

        CFRunLoopAddSource(CFRunLoopGetMain(), AXObserverGetRunLoopSource(observer), .defaultMode)
        axObserver = observer
    }

    private func stopAXObservation() {
        guard let obs = axObserver else { return }
        CFRunLoopRemoveSource(CFRunLoopGetMain(), AXObserverGetRunLoopSource(obs), .defaultMode)
        axObserver  = nil
        observedPID = 0
    }

    // ── Callback handlers ─────────────────────────────────────────────────────

    private func handleAppActivated(_ app: NSRunningApplication) {
        startAXObservation(for: app.processIdentifier)
        emitAppFocused(app: app, queryElement: AXIsProcessTrustedWithOptions(nil))
    }

    /// Called from the global AX C callback (always on main thread via CFRunLoopGetMain).
    ///
    /// Debounced: the work item is cancelled and rescheduled on every callback.
    /// Only fires after `contextDebouncMs` of silence — prevents event floods
    /// during rapid keyboard navigation (arrow keys, Tab, etc.).
    func handleAXElementChanged() {
        debounceWorkItem?.cancel()
        let item = DispatchWorkItem { [weak self] in
            self?.emitAXElementChanged()
        }
        debounceWorkItem = item
        DispatchQueue.main.asyncAfter(
            deadline: .now() + .milliseconds(Self.contextDebouncMs),
            execute:  item
        )
    }

    private func emitAppFocused(app: NSRunningApplication, queryElement: Bool) {
        var payload: [String: Any] = [
            "bundle_id": app.bundleIdentifier ?? "",
            "name":      app.localizedName ?? "",
        ]
        if queryElement, let axDict = queryFocusedElement(for: app.processIdentifier) {
            payload["ax_element"] = axDict
        }
        sendSystemEvent(.appFocused, payload: payload)
    }

    private func emitAXElementChanged() {
        guard observedPID != 0 else { return }
        if let axDict = queryFocusedElement(for: observedPID) {
            sendSystemEvent(.axElementChanged, payload: axDict)
        }
    }

    private func performStop() {
        debounceWorkItem?.cancel()
        debounceWorkItem = nil

        stopClipboardPolling()
        stopHotkeyTap()
        stopAXObservation()

        let nc  = NSWorkspace.shared.notificationCenter
        let dnc = DistributedNotificationCenter.default()
        workspaceTokens.forEach   { nc.removeObserver($0)  }
        distributedTokens.forEach { dnc.removeObserver($0) }
        workspaceTokens   = []
        distributedTokens = []
    }

    // ── Global hotkey tap ─────────────────────────────────────────────────────

    /// Register a CGEventTap to listen for the global activation hotkey (Ctrl+Shift+Space).
    ///
    /// Requires Accessibility permission (kTCCServiceAccessibility) — already checked at
    /// startup. If CGEventTapCreate fails (permission revoked, sandbox restriction, etc.),
    /// logs a warning and continues in degraded mode — voice still works, hotkey unavailable.
    ///
    /// Installed at `.cgSessionEventTap` with `.headInsertEventTap`: sees key events before
    /// the focused application. `hotkeyTapCallback` returns nil for the hotkey chord to
    /// consume it; all other events are passed through unchanged.
    private func startHotkeyTap() {
        let eventMask = CGEventMask(1 << CGEventType.keyDown.rawValue)
        let bridge    = Unmanaged.passUnretained(self).toOpaque()

        guard let tap = CGEvent.tapCreate(
            tap:              .cgSessionEventTap,
            place:            .headInsertEventTap,
            options:          .defaultTap,
            eventsOfInterest: eventMask,
            callback:         hotkeyTapCallback,
            userInfo:         bridge
        ) else {
            print("[EventBridge] CGEvent.tapCreate failed — Ctrl+Shift+Space hotkey unavailable (check Accessibility permission)")
            return
        }

        let source = CFMachPortCreateRunLoopSource(kCFAllocatorDefault, tap, 0)
        CFRunLoopAddSource(CFRunLoopGetMain(), source, .commonModes)
        CGEvent.tapEnable(tap: tap, enable: true)

        hotkeyTap           = tap
        hotkeyRunLoopSource = source
    }

    private func stopHotkeyTap() {
        if let tap = hotkeyTap, let source = hotkeyRunLoopSource {
            CGEvent.tapEnable(tap: tap, enable: false)
            CFRunLoopRemoveSource(CFRunLoopGetMain(), source, .commonModes)
            CFMachPortInvalidate(tap)
        }
        hotkeyTap           = nil
        hotkeyRunLoopSource = nil
    }

    /// Returns true if `event` matches the operator's configured activation hotkey.
    ///
    /// Uses stored properties populated by `updateHotkeyConfig(_:)` at session open.
    /// Defaults to Ctrl+Shift+Space (keyCode 49) — identical to Phase 16 hardcoded behavior.
    ///
    /// The `== Bool` form handles both required and excluded modifiers uniformly:
    /// - `hotkeyRequiresCmd = false` → `flags.contains(.maskCommand) == false`
    ///   correctly rejects keypresses that include Cmd.
    /// - `hotkeyRequiresCmd = true`  → `flags.contains(.maskCommand) == true`
    ///   correctly requires Cmd.
    ///
    /// Called from `hotkeyTapCallback` on the main run loop.
    func isHotkeyEvent(_ event: CGEvent) -> Bool {
        let keyCode = event.getIntegerValueField(.keyboardEventKeycode)
        let flags   = event.flags
        return keyCode == hotkeyKeyCode
            && flags.contains(.maskControl)   == hotkeyRequiresCtrl
            && flags.contains(.maskShift)     == hotkeyRequiresShift
            && flags.contains(.maskCommand)   == hotkeyRequiresCmd
            && flags.contains(.maskAlternate) == hotkeyRequiresOption
    }

    /// Update hotkey detection parameters from a `ConfigSync` proto message.
    ///
    /// Called on `@MainActor` by `DexterClient.onResponse` when a `ConfigSync` event
    /// arrives. `isHotkeyEvent(_:)` is also called on the main thread (via the
    /// CGEventTap run loop source), so there is no data race on the stored properties.
    func updateHotkeyConfig(_ config: Dexter_V1_HotkeyConfig) {
        hotkeyKeyCode        = Int64(config.keyCode)
        hotkeyRequiresCtrl   = config.ctrl
        hotkeyRequiresShift  = config.shift
        hotkeyRequiresCmd    = config.cmd
        hotkeyRequiresOption = config.option
        print("[EventBridge] Hotkey config updated: keyCode=\(config.keyCode)")
    }

    /// Send a HOTKEY_ACTIVATED SystemEvent to the Rust orchestrator.
    /// Called from `hotkeyTapCallback` on the main run loop.
    func handleHotkeyActivated() {
        sendSystemEvent(.hotkeyActivated)
    }

    // ── AX element query ──────────────────────────────────────────────────────

    /// Query the currently focused AX element for the process with `pid`.
    ///
    /// Returns a dictionary suitable for:
    /// - The `ax_element` sub-object inside an APP_FOCUSED payload
    /// - The root payload of an AX_ELEMENT_CHANGED event
    ///
    /// Returns `nil` when the focused element cannot be read (app does not expose AX,
    /// AX query failed, or no element is currently focused).
    ///
    /// Privacy: `AXSecureTextField` role → `is_sensitive = true`, value never read.
    /// Secondary check: label containing a sensitive keyword → treated as sensitive.
    private func queryFocusedElement(for pid: pid_t) -> [String: Any]? {
        let appElement = AXUIElementCreateApplication(pid)

        // Get the focused element handle.
        var focusedRef: CFTypeRef?
        guard AXUIElementCopyAttributeValue(
            appElement, kAXFocusedUIElementAttribute as CFString, &focusedRef
        ) == .success, let focusedRef else { return nil }
        // swiftlint:disable:next force_cast
        let focused = focusedRef as! AXUIElement

        // Role (always present on valid elements).
        guard let role = readStringAttribute(focused, kAXRoleAttribute as CFString) else {
            return nil
        }

        // Secure text field: return immediately — never read label or value.
        if role == "AXSecureTextField" {
            return ["role": role, "is_sensitive": true, "label": "", "value_preview": ""]
        }

        // Label: prefer AXDescription (semantic description), fall back to AXLabelValue.
        let label = readStringAttribute(focused, kAXDescriptionAttribute as CFString)
            ?? readStringAttribute(focused, kAXLabelValueAttribute as CFString)
            ?? ""

        // Secondary privacy check on label text.
        let isSensitive = Self.sensitiveKeywords.contains(where: label.lowercased().contains)

        let valuePreview: String
        if isSensitive {
            valuePreview = ""
        } else {
            let raw = readStringAttribute(focused, kAXValueAttribute as CFString) ?? ""
            // Truncate to preview max. Rust performs a secondary guard on ingestion.
            valuePreview = raw.count <= Self.axValuePreviewMaxChars
                ? raw
                : String(raw.prefix(Self.axValuePreviewMaxChars))
        }

        return [
            "role":          role,
            "label":         label,
            "value_preview": valuePreview,
            "is_sensitive":  isSensitive,
        ]
    }

    /// Read a string AX attribute from `element`. Returns `nil` on any failure.
    private func readStringAttribute(_ element: AXUIElement, _ attribute: CFString) -> String? {
        var ref: CFTypeRef?
        guard AXUIElementCopyAttributeValue(element, attribute, &ref) == .success else { return nil }
        return ref as? String
    }

    // ── gRPC event emission ───────────────────────────────────────────────────

    /// Serialize `payload` to JSON and emit a `ClientEvent` with the given `SystemEventType`.
    private func sendSystemEvent(
        _ type:  Dexter_V1_SystemEventType,
        payload: [String: Any] = [:]
    ) {
        let jsonString: String
        if payload.isEmpty {
            jsonString = "{}"
        } else {
            let data = (try? JSONSerialization.data(withJSONObject: payload)) ?? Data()
            jsonString = String(data: data, encoding: .utf8) ?? "{}"
        }

        let event = Dexter_V1_ClientEvent.with {
            $0.traceID   = UUID().uuidString
            $0.sessionID = ""   // session ID not tracked by EventBridge (set by DexterClient)
            $0.systemEvent = Dexter_V1_SystemEvent.with {
                $0.type    = type
                $0.payload = jsonString
            }
        }
        sendEvent(event)
    }
}
