//! Read-only macOS privacy permission preflights for daemon health.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PermissionSnapshot {
    pub accessibility_trusted: bool,
    pub screen_recording_trusted: bool,
}

impl PermissionSnapshot {
    pub fn current() -> Self {
        Self {
            accessibility_trusted: accessibility_trusted(),
            screen_recording_trusted: screen_recording_trusted(),
        }
    }
}

#[cfg(target_os = "macos")]
fn accessibility_trusted() -> bool {
    unsafe extern "C" {
        fn AXIsProcessTrusted() -> u8;
    }

    // This preflight never requests permission or opens System Settings.
    unsafe { AXIsProcessTrusted() != 0 }
}

#[cfg(not(target_os = "macos"))]
fn accessibility_trusted() -> bool {
    false
}

#[cfg(target_os = "macos")]
fn screen_recording_trusted() -> bool {
    unsafe extern "C" {
        fn CGPreflightScreenCaptureAccess() -> bool;
    }

    // This preflight is side-effect free; the request API is intentionally not used.
    unsafe { CGPreflightScreenCaptureAccess() }
}

#[cfg(not(target_os = "macos"))]
fn screen_recording_trusted() -> bool {
    false
}
