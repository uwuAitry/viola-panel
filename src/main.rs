// SPDX-License-Identifier: GPL-3.0-or-later
//! viola-panel - control panel for the orender output side of the viola-bridge chain.
//!
//! M0: single-instance guard, window skeleton, log file.
//! See docs/design.md for the full design.

/// Declares the small slice of the Win32 API this crate needs, straight from the
/// local Windows SDK headers. Hand-written rather than pulled from a crate so the
/// signatures are verifiable against the SDK that ships with the toolchain.
#[cfg(windows)]
mod win {
    pub type Handle = isize;
    pub const ERROR_ALREADY_EXISTS: u32 = 183;

    #[link(name = "kernel32")]
    extern "system" {
        pub fn CreateMutexW(
            attrs: *const core::ffi::c_void,
            initial_owner: i32,
            name: *const u16,
        ) -> Handle;
        pub fn GetLastError() -> u32;
        pub fn SetLastError(code: u32);
    }
}

/// Take the process-wide single-instance mutex. The handle is intentionally never
/// released: it lives as long as the process, which is exactly the semantics we want.
#[cfg(windows)]
fn acquire_single_instance() -> bool {
    let name: Vec<u16> = "Local\\viola-panel-single-instance\0".encode_utf16().collect();
    // SAFETY: no preconditions. Clear the stale last-error so that the guard below
    // cannot be fooled by a value left behind by an unrelated earlier Win32 call on
    // this thread.
    unsafe { win::SetLastError(0) };
    // SAFETY: `name` is a valid NUL-terminated UTF-16 buffer that outlives the call.
    let handle = unsafe { win::CreateMutexW(core::ptr::null(), 0, name.as_ptr()) };
    if handle == 0 {
        // Could not even create the mutex; do not block the user over it.
        return true;
    }
    // SAFETY: no preconditions.
    unsafe { win::GetLastError() != win::ERROR_ALREADY_EXISTS }
}

#[cfg(not(windows))]
fn acquire_single_instance() -> bool {
    true
}

/// Append a line to the panel log. Best effort: a failure to log must never take
/// the UI down with it.
fn log(line: &str) {
    use std::io::Write;
    let Some(dir) = panel_dir() else { return };
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("viola-panel.log"))
    {
        let _ = writeln!(f, "{} {line}", timestamp());
    }
}

fn timestamp() -> String {
    // Seconds since the epoch is enough to order log lines; avoid a date-formatting
    // dependency for M0.
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => format!("+{}s", d.as_secs()),
        Err(_) => "+?s".to_string(),
    }
}

/// `%LOCALAPPDATA%\viola-panel`, the one directory this panel owns.
fn panel_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|v| std::path::PathBuf::from(v).join("viola-panel"))
}

struct PanelApp;

impl eframe::App for PanelApp {
    fn ui(&mut self, ui: &mut eframe::egui::Ui, _frame: &mut eframe::Frame) {
        // The `Ui` handed to `App::ui` has no margin or background of its own.
        ui.heading("viola-panel");
        ui.label("Control panel for the orender output side.");
        ui.separator();
        ui.label("M0 skeleton - no controls wired up yet.");
    }
}

fn main() -> eframe::Result {
    if !acquire_single_instance() {
        log("second instance refused (already running)");
        return Ok(());
    }
    log("panel starting");

    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("viola-panel")
            .with_inner_size([560.0, 420.0])
            .with_min_inner_size([420.0, 320.0]),
        ..Default::default()
    };

    let result = eframe::run_native(
        "viola-panel",
        options,
        Box::new(|_cc| Ok(Box::new(PanelApp))),
    );
    let message = match &result {
        Ok(()) => "panel exited normally".to_string(),
        Err(e) => format!("panel exited with error: {e}"),
    };
    log(&message);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The guard is a cross-process contract: a second acquisition inside the same
    /// process must be refused (Win32 reports ERROR_ALREADY_EXISTS).
    #[cfg(windows)]
    #[test]
    fn second_instance_is_refused() {
        assert!(acquire_single_instance(), "first acquisition should succeed");
        assert!(!acquire_single_instance(), "second acquisition must be refused");
    }

    #[test]
    fn panel_dir_is_under_localappdata() {
        let dir = panel_dir().expect("LOCALAPPDATA should be set on Windows");
        assert!(dir.ends_with("viola-panel"));
    }
}
