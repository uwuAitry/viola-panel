#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
// SPDX-License-Identifier: GPL-3.0-or-later
//! viola-panel - control panel for the orender output side of the viola-bridge chain.
//!
//! The window is a WebView2 (via `wry`) showing three static files embedded with
//! `include_str!`; the panel logic below is the same state machine it always was,
//! now reached over an IPC envelope instead of an inline `egui` render pass.
//! See docs/design.md, section 9, for why the UI layer is a webview.

mod asiodrv;
mod config;
mod launcher;
mod mmdev;
mod registry;

use std::borrow::Cow;
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tao::dpi::LogicalSize;
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoop};
use tao::window::WindowBuilder;
use wry::http::{Request, Response};
use wry::{WebViewBuilder, WebViewId};

use asiodrv::DriverInfo;
use config::{PanelConfig, ResolvedPaths};
use launcher::{Engine, LaunchSpec};
use registry::AsioDevice;

/// The three frontend files, embedded verbatim. There is no build step: the
/// same bytes ship in the exe and can be opened in a browser for review
/// (design.md 9.2).
const INDEX_HTML: &str = include_str!("../ui/index.html");
const STYLES_CSS: &str = include_str!("../ui/styles.css");
const APP_JS: &str = include_str!("../ui/app.js");

/// How many lines of the engine log the status bar shows after a failure.
const LOG_TAIL_LINES: usize = 12;

/// Push the whole state at least this often, so a change made outside the panel
/// (the child process exiting) still reaches the page.
const STATE_PUSH_INTERVAL: Duration = Duration::from_millis(500);

/// How long the event loop sleeps between wake-ups. Short enough that a click
/// is reflected at once, long enough not to spin.
const TICK: Duration = Duration::from_millis(50);

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
    let Some(dir) = config::panel_dir() else { return };
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

/// `%LOCALAPPDATA%\viola-panel\engine.log` - stdout+stderr of the launched pipeline.
fn engine_log_path() -> Option<PathBuf> {
    config::panel_dir().map(|dir| dir.join("engine.log"))
}

/// The last `max_lines` lines of a log file, or an empty string if unreadable.
fn log_tail(path: &Path, max_lines: usize) -> String {
    let Ok(text) = std::fs::read_to_string(path) else {
        return String::new();
    };
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(max_lines)..].join("\n")
}

// --- the IPC contract (design.md 9.3) ---------------------------------------

/// One message from the page: `{ "cmd": "<name>", "args": <anything|null> }`.
#[derive(Deserialize)]
struct Envelope {
    cmd: String,
    #[serde(default)]
    args: Option<serde_json::Value>,
}

/// The settings a page may change. Every field the form owns is required, so a
/// partial or misspelled payload is rejected at the boundary rather than
/// silently defaulted (design.md 9.4 keeps `PanelSettings` as the on-disk
/// contract; this is only the wire shape).
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SettingsPatch {
    speaker_layout: Option<PathBuf>,
    enable_vbap: bool,
    output_backend: String,
    output_device: Option<String>,
    output_sample_rate: u32,
    latency_target_ms: Option<u32>,
}

impl SettingsPatch {
    /// Reject values the panel would happily write and orender would then refuse.
    fn validate(&self) -> Result<(), String> {
        if self.output_backend != "file" && self.output_backend != "asio" {
            return Err(format!(
                "unknown backend {:?}; expected \"file\" or \"asio\"",
                self.output_backend
            ));
        }
        if self.output_sample_rate == 0 || self.output_sample_rate > 768_000 {
            return Err(format!(
                "sample rate {} Hz is out of range",
                self.output_sample_rate
            ));
        }
        if let Some(ms) = self.latency_target_ms {
            if ms == 0 || ms > 5_000 {
                return Err(format!("latency target {ms} ms is out of range"));
            }
        }
        // A device only means something for the asio backend.
        if self.output_backend == "file" && self.output_device.is_some() {
            return Err("a device cannot be selected with the file backend".to_string());
        }
        Ok(())
    }
}

/// What the page is told after a command; mirrors `window.violaPanel.applyResult`.
#[derive(Serialize, Default)]
struct CommandResult {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
}

impl CommandResult {
    fn ok(message: impl Into<String>) -> Self {
        Self { ok: true, message: Some(message.into()), ..Self::default() }
    }

    fn failed(error: impl Into<String>) -> Self {
        Self { ok: false, error: Some(error.into()), ..Self::default() }
    }
}

/// The snapshot pushed to the page on every state change. Borrowed, so building
/// one costs a serialization and nothing else.
#[derive(Serialize)]
struct Snapshot<'a> {
    settings: &'a config::PanelSettings,
    devices: &'a [AsioDevice],
    engine_running: bool,
    engine_pid: Option<u32>,
    /// Set when the engine died on its own; the status line shows this instead.
    engine_error: Option<String>,
    driver: Option<&'a DriverInfo>,
    driver_error: Option<&'a str>,
    default_endpoint: Option<&'a str>,
    default_endpoint_error: Option<&'a str>,
    config_path: String,
}

// --- the state machine -------------------------------------------------------

/// The panel's mutable state. Every field the UI can change lives here, so the
/// snapshot pushed to the page is a pure function of this struct.
struct PanelApp {
    config_path: PathBuf,
    config: PanelConfig,
    resolved: ResolvedPaths,
    devices: Vec<AsioDevice>,
    /// The pipeline we launched, if any. `None` means "not ours to manage".
    engine: Option<Engine>,
    status: String,
    status_is_error: bool,
    /// Result of the most recent on-demand driver probe.
    driver: Option<Result<DriverInfo, String>>,
    driver_rx: Option<mpsc::Receiver<Result<DriverInfo, String>>>,
    panel_rx: Option<mpsc::Receiver<Result<(), String>>>,
    /// Outcome text for the control-panel thread (it outlives the click).
    panel_status: Option<String>,
    default_endpoint: Result<String, String>,
    /// A message the loop should hand to the page once.
    pending_result: Option<CommandResult>,
    /// The page asked for the engine log tail.
    pending_log: bool,
    /// Something changed, so the next tick must push a snapshot.
    dirty: bool,
}

impl PanelApp {
    fn new() -> Self {
        let config_path =
            config::default_config_path().unwrap_or_else(|| PathBuf::from("panel.yaml"));

        // A corrupt file is surfaced, not silently replaced: see config::PanelConfig::load.
        let config = match PanelConfig::load(&config_path) {
            Ok(config) => config,
            Err(e) => {
                log(&format!("config load failed: {e}"));
                PanelConfig::default()
            }
        };
        let resolved = config::resolve_panel_paths(&config.panel);
        let devices = registry::list_asio_devices();
        log(&format!(
            "panel starting; {} ASIO device(s) in registry",
            devices.len()
        ));

        Self {
            config_path,
            config,
            resolved,
            devices,
            engine: None,
            status: "Engine stopped".to_string(),
            status_is_error: false,
            driver: None,
            driver_rx: None,
            panel_rx: None,
            panel_status: None,
            // Read once at startup: the endpoint only changes when the user changes
            // it in Windows, and this panel tells them to go there.
            default_endpoint: mmdev::default_render_endpoint(),
            pending_result: None,
            pending_log: false,
            dirty: true,
        }
    }

    /// The CLSID of the device currently chosen in the dropdown, if it names one.
    fn selected_clsid(&self) -> Option<String> {
        let name = self.config.panel.output_device.as_deref()?;
        self.devices
            .iter()
            .find(|d| d.description == name)
            .map(|d| d.clsid.clone())
            .filter(|clsid| !clsid.is_empty())
    }

    /// Build the launch spec, or explain which path is missing.
    fn build_spec(&self) -> Result<LaunchSpec, String> {
        let Some(orender_exe) = self.resolved.orender_exe.clone() else {
            return Err("orender.exe not found (set panel.orender_exe in panel.yaml)".to_string());
        };
        let Some(ffplay_exe) = self.resolved.ffplay_exe.clone() else {
            return Err("ffplay.exe not found on PATH (set panel.ffplay_exe)".to_string());
        };
        let Some(bridge_dll) = self.resolved.bridge_dll.clone() else {
            return Err("viola_bridge.dll not found (set panel.bridge_dll)".to_string());
        };
        let Some(log_path) = engine_log_path() else {
            return Err("no %LOCALAPPDATA% to write the engine log to".to_string());
        };

        Ok(LaunchSpec {
            orender_exe,
            ffplay_exe,
            bridge_dll,
            speaker_layout: self.config.panel.speaker_layout.clone(),
            enable_vbap: self.config.panel.enable_vbap,
            output_backend: self.config.panel.output_backend.clone(),
            output_device: self.config.panel.output_device.clone(),
            output_sample_rate: self.config.panel.output_sample_rate,
            latency_target_ms: self.config.panel.latency_target_ms,
            config_path: self.config_path.clone(),
            log_path,
        })
    }

    /// Persist the panel's own settings and the `render:` block orender will read.
    fn save_config(&mut self) -> Result<(), String> {
        self.config.save(&self.config_path)
    }

    /// Stop the pipeline we launched. Only ever touches our own PID (design.md 4.3).
    fn stop_engine(&mut self) {
        if let Some(mut engine) = self.engine.take() {
            let pid = engine.pid();
            engine.kill();
            log(&format!("stopped engine pid {pid}"));
            self.status = "Engine stopped".to_string();
            self.status_is_error = false;
        }
        // The driver is free again once our process tree is gone.
        self.driver = None;
        self.panel_status = None;
    }

    /// Write the config, then (re)start the pipeline.
    fn start_engine(&mut self) -> Result<String, String> {
        self.stop_engine();

        self.save_config()?;
        let spec = self.build_spec()?;

        match launcher::start(&spec) {
            Ok(engine) => {
                let pid = engine.pid();
                log(&format!(
                    "started engine pid {pid}: {}",
                    launcher::build_command_line(&spec)
                ));
                self.status = format!("Engine running (PID {pid})");
                self.status_is_error = false;
                self.engine = Some(engine);
                Ok(format!("Engine running (PID {pid})"))
            }
            Err(e) => {
                log(&format!("engine start failed: {e}"));
                self.status = format!("cannot start engine: {e}");
                self.status_is_error = true;
                Err(format!("cannot start engine: {e}"))
            }
        }
    }

    /// Poll the child process; a finished engine must not keep claiming to run.
    fn poll_engine(&mut self) {
        let exited = match self.engine.as_mut() {
            Some(engine) => match engine.try_exit() {
                Ok(Some(exit)) => Some(exit.to_string()),
                Ok(None) => None,
                Err(e) => Some(format!("poll failed: {e}")),
            },
            None => None,
        };
        if let Some(text) = exited {
            self.engine = None;
            self.status = format!("Engine exited ({text})");
            self.status_is_error = true;
            self.dirty = true;
            log(&format!("engine exited: {text}"));
            let tail = engine_log_path()
                .map(|p| log_tail(&p, LOG_TAIL_LINES))
                .unwrap_or_default();
            let mut result = CommandResult::failed(format!("Engine exited ({text})"));
            if !tail.is_empty() {
                log(&format!("engine log tail:\n{tail}"));
                result.detail = Some(tail);
            }
            self.pending_result = Some(result);
        }
    }

    /// Collect results from the detached probe / control-panel threads.
    fn poll_channels(&mut self) {
        if let Some(rx) = &self.driver_rx {
            match rx.try_recv() {
                Ok(result) => {
                    self.driver = Some(result);
                    self.driver_rx = None;
                    self.dirty = true;
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.driver = Some(Err("driver probe thread died".to_string()));
                    self.driver_rx = None;
                    self.dirty = true;
                }
            }
        }

        if let Some(rx) = &self.panel_rx {
            match rx.try_recv() {
                Ok(Ok(())) => {
                    self.panel_status = Some("Driver control panel closed.".to_string());
                    self.panel_rx = None;
                    self.pending_result = Some(CommandResult::ok("Driver control panel closed."));
                }
                Ok(Err(e)) => {
                    self.panel_status = Some(format!("Control panel failed: {e}"));
                    self.panel_rx = None;
                    self.pending_result =
                        Some(CommandResult::failed(format!("Control panel failed: {e}")));
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.panel_status =
                        Some("Control panel closed; the thread reported no result.".to_string());
                    self.panel_rx = None;
                    self.pending_result = Some(CommandResult::ok(
                        "Control panel closed; the thread reported no result.",
                    ));
                }
            }
        }
    }

    /// Ask a detached thread to open the selected driver's own control panel. It
    /// blocks until the user closes that window, so it must never be the UI thread.
    fn open_driver_control_panel(&mut self) -> Result<String, String> {
        let Some(clsid) = self.selected_clsid() else {
            return Err("No driver CLSID for the selected device.".to_string());
        };
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let result = asiodrv::open_control_panel(&clsid);
            let _ = tx.send(result);
        });
        self.panel_rx = Some(rx);
        self.panel_status = Some("Opening the driver control panel...".to_string());
        self.dirty = true;
        Ok("Opening the driver control panel…".to_string())
    }

    /// Ask a detached thread to read the selected driver's capabilities.
    fn probe_driver(&mut self) -> Result<String, String> {
        let Some(clsid) = self.selected_clsid() else {
            let message = "No driver CLSID for the selected device.".to_string();
            self.driver = Some(Err(message.clone()));
            self.dirty = true;
            return Err(message);
        };
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let result = asiodrv::probe(&clsid);
            let _ = tx.send(result);
        });
        self.driver_rx = Some(rx);
        self.driver = None;
        self.dirty = true;
        Ok("Reading driver…".to_string())
    }

    /// Fold the page's settings into the config, after validating them.
    fn apply_patch(&mut self, args: Option<&serde_json::Value>) -> Result<(), String> {
        let value = args.ok_or("this command needs the panel settings")?;
        let patch: SettingsPatch = serde_json::from_value(value.clone())
            .map_err(|e| format!("bad settings: {e}"))?;
        patch.validate()?;

        self.config.panel.speaker_layout = patch.speaker_layout;
        self.config.panel.enable_vbap = patch.enable_vbap;
        self.config.panel.output_backend = patch.output_backend;
        self.config.panel.output_device = patch.output_device;
        self.config.panel.output_sample_rate = patch.output_sample_rate;
        self.config.panel.latency_target_ms = patch.latency_target_ms;
        self.dirty = true;
        Ok(())
    }

    /// Handle one command from the page. Mutates state only — never touches the
    /// webview, so it cannot re-enter the engine from inside its own message.
    fn handle_command(&mut self, body: &str) -> Result<(), String> {
        let envelope: Envelope =
            serde_json::from_str(body).map_err(|e| format!("bad envelope: {e}"))?;
        self.dirty = true;

        let outcome = match envelope.cmd.as_str() {
            // The loop pushes a snapshot every tick anyway; nothing to do here.
            "get_state" => return Ok(()),
            "read_engine_log" => {
                self.pending_log = true;
                return Ok(());
            }
            "save" => self
                .apply_patch(envelope.args.as_ref())
                .and_then(|()| self.save_config())
                .map(|()| "Settings saved".to_string()),
            "start" => self
                .apply_patch(envelope.args.as_ref())
                .and_then(|()| self.start_engine()),
            "apply" => self
                .apply_patch(envelope.args.as_ref())
                .and_then(|()| self.start_engine())
                .map(|message| format!("Saved; {message}")),
            "stop" => {
                self.stop_engine();
                self.dirty = true;
                Ok("Engine stopped".to_string())
            }
            "probe_driver" => self.probe_driver(),
            "open_driver_panel" => self.open_driver_control_panel(),
            other => Err(format!("unknown command {other:?}")),
        };

        self.pending_result = Some(match outcome {
            Ok(message) => CommandResult::ok(message),
            Err(error) => CommandResult::failed(error),
        });
        Ok(())
    }

    /// The snapshot the page renders from.
    fn snapshot(&self) -> Snapshot<'_> {
        let (driver, driver_error) = match &self.driver {
            Some(Ok(info)) => (Some(info), None),
            Some(Err(e)) => (None, Some(e.as_str())),
            None => (None, None),
        };
        let (default_endpoint, default_endpoint_error) = match &self.default_endpoint {
            Ok(name) => (Some(name.as_str()), None),
            Err(e) => (None, Some(e.as_str())),
        };
        Snapshot {
            settings: &self.config.panel,
            devices: &self.devices,
            engine_running: self.engine.is_some(),
            engine_pid: self.engine.as_ref().map(|engine| engine.pid()),
            // The page shows the error text in the status line; `status` is the
            // same string, so only surface it when it is an error.
            engine_error: self
                .status_is_error
                .then(|| self.status.clone()),
            driver,
            driver_error,
            default_endpoint,
            default_endpoint_error,
            config_path: self.config_path.display().to_string(),
        }
    }
}

// --- the embedded frontend ---------------------------------------------------

/// Serve one of the three embedded files. Content types matter: WebView2 refuses
/// to execute a stylesheet or script sent as `text/plain`.
fn serve(path: &str) -> Response<Cow<'static, [u8]>> {
    let (content_type, bytes): (&str, &'static [u8]) = match path {
        "/" | "/index.html" => ("text/html; charset=utf-8", INDEX_HTML.as_bytes()),
        "/styles.css" => ("text/css; charset=utf-8", STYLES_CSS.as_bytes()),
        "/app.js" => ("text/javascript; charset=utf-8", APP_JS.as_bytes()),
        _ => {
            return Response::builder()
                .status(404)
                .header("Content-Type", "text/plain; charset=utf-8")
                .body(Cow::Borrowed(&b"not found"[..]))
                .expect("a static 404 response is always valid");
        }
    };
    Response::builder()
        .status(200)
        .header("Content-Type", content_type)
        .body(Cow::Borrowed(bytes))
        .expect("a static asset response is always valid")
}

/// A handler matching `with_custom_protocol`'s signature.
fn protocol_handler(
    _id: WebViewId<'_>,
    request: Request<Vec<u8>>,
) -> Response<Cow<'static, [u8]>> {
    serve(request.uri().path())
}

// --- entry point -------------------------------------------------------------

fn main() -> Result<(), Box<dyn std::error::Error>> {
    if !acquire_single_instance() {
        log("second instance refused (already running)");
        return Ok(());
    }
    log("panel starting");

    let event_loop = EventLoop::new();
    let window = WindowBuilder::new()
        .with_title("viola-panel")
        .with_inner_size(LogicalSize::new(720.0, 620.0))
        .with_min_inner_size(LogicalSize::new(560.0, 480.0))
        .build(&event_loop)?;

    let app = Rc::new(RefCell::new(PanelApp::new()));

    // The custom protocol is reached as `http://app.localhost/...` on Windows
    // (wry rewrites `<scheme>://` to `http://<scheme>.localhost/`).
    let handler_app = Rc::clone(&app);
    let webview = WebViewBuilder::new()
        .with_custom_protocol("app".into(), protocol_handler)
        .with_ipc_handler(move |request: Request<String>| {
            let body = request.body().clone();
            if let Err(e) = handler_app.borrow_mut().handle_command(&body) {
                log(&format!("ipc command failed: {e}"));
            }
        })
        .with_url("http://app.localhost/index.html")
        .build(&window)?;

    log("panel window shown");
    let mut last_push = Instant::now() - STATE_PUSH_INTERVAL;

    // One place talks back to the page. A command only ever sets flags, so the
    // reply cannot land while the webview is still dispatching that command.
    event_loop.run(move |event, _, control_flow| {
        if let Event::WindowEvent {
            event: WindowEvent::CloseRequested,
            ..
        } = event
        {
            *control_flow = ControlFlow::Exit;
            return;
        }
        // A short bounded wait: an IPC command is reflected on the next tick,
        // without a busy spin.
        *control_flow = ControlFlow::WaitUntil(Instant::now() + TICK);

        let (snapshot, result, log_text) = {
            let mut app = app.borrow_mut();
            app.poll_engine();
            app.poll_channels();

            let log_text = app.pending_log.then(|| {
                app.pending_log = false;
                engine_log_path()
                    .map(|p| log_tail(&p, LOG_TAIL_LINES))
                    .unwrap_or_default()
            });
            let result = app.pending_result.take();
            let snapshot = (app.dirty || last_push.elapsed() >= STATE_PUSH_INTERVAL)
                .then(|| app.snapshot())
                .map(|snapshot| serde_json::to_string(&snapshot));
            (snapshot, result, log_text)
        };

        if let Some(Ok(json)) = snapshot {
            let _ = webview.evaluate_script(&format!("window.violaPanel.applyState({json});"));
        }
        if let Some(Ok(json)) = result.map(|r| serde_json::to_string(&r)) {
            let _ = webview.evaluate_script(&format!("window.violaPanel.applyResult({json});"));
        }
        if let Some(text) = log_text {
            if let Ok(json) = serde_json::to_string(&text) {
                let _ = webview.evaluate_script(&format!("window.violaPanel.applyLog({json});"));
            }
        }

        if snapshot.is_some() {
            app.borrow_mut().dirty = false;
            last_push = Instant::now();
        }
    });
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
        let dir = config::panel_dir().expect("LOCALAPPDATA should be set on Windows");
        assert!(dir.ends_with("viola-panel"));
    }

    #[test]
    fn log_tail_keeps_the_last_lines_and_survives_a_missing_file() {
        let path = std::env::temp_dir().join("viola-panel-log-tail-test.txt");
        std::fs::write(&path, "one\ntwo\nthree\nfour\n").expect("write temp log");
        assert_eq!(log_tail(&path, 2), "three\nfour");
        assert_eq!(log_tail(&path, 99), "one\ntwo\nthree\nfour");
        let _ = std::fs::remove_file(&path);
        assert_eq!(log_tail(Path::new("does-not-exist"), 3), "");
    }

    /// The embedded frontend is what the exe actually serves; a truncated
    /// `include_str!` would otherwise only show up as a blank window.
    #[test]
    fn embedded_frontend_is_served_with_usable_content_types() {
        let index = serve("/index.html");
        assert_eq!(index.status(), 200);
        assert!(index.headers()["Content-Type"].to_str().unwrap().starts_with("text/html"));
        assert!(String::from_utf8_lossy(index.body()).contains("<title>viola-panel</title>"));

        let script = serve("/app.js");
        assert!(script.headers()["Content-Type"].to_str().unwrap().contains("javascript"));
        assert!(String::from_utf8_lossy(script.body()).contains("window.violaPanel"));

        let style = serve("/styles.css");
        assert!(style.headers()["Content-Type"].to_str().unwrap().starts_with("text/css"));
        assert!(String::from_utf8_lossy(style.body()).contains("prefers-reduced-motion"));

        // `/` is the same document, and anything else is a 404 rather than a
        // silent fallback to the page.
        assert_eq!(serve("/").status(), 200);
        assert_eq!(serve("/secret").status(), 404);
    }

    /// Every command the page sends must be recognised, and an unknown one must
    /// be refused rather than ignored.
    #[test]
    fn ipc_commands_are_validated_at_the_boundary() {
        let mut app = PanelApp::new();
        // Never write the user's real config while testing.
        app.config_path = std::env::temp_dir().join("viola-panel-ipc-test.yaml");

        assert!(app.handle_command("{\"cmd\":\"get_state\"}").is_ok());
        assert!(app.handle_command("{\"cmd\":\"nonsense\"}").is_err());
        assert!(app.handle_command("not json").is_err());

        // An unknown key is refused rather than ignored, so a typo in the page
        // cannot look like it did something.
        let typo = r#"{"cmd":"save","args":{"speaker_layout":null,"enable_vbap":true,
            "output_backend":"file","output_device":null,"output_sample_rate":48000,
            "latency_target_ms":null,"latency":220}}"#;
        assert!(app.handle_command(typo).is_err());

        // A payload that omits the sample rate leaves it at 0, which validation
        // must refuse rather than write.
        let partial = r#"{"cmd":"save","args":{"output_backend":"file"}}"#;
        assert!(app.handle_command(partial).is_err());
        // So is a backend orender does not know, and a device under `file`.
        let bad_backend = r#"{"cmd":"save","args":{"speaker_layout":null,"enable_vbap":true,
            "output_backend":"wasapi","output_device":null,"output_sample_rate":48000,
            "latency_target_ms":null}}"#;
        assert!(app.handle_command(bad_backend).is_err());

        let device_under_file = r#"{"cmd":"save","args":{"speaker_layout":null,"enable_vbap":true,
            "output_backend":"file","output_device":"X","output_sample_rate":48000,
            "latency_target_ms":null}}"#;
        assert!(app.handle_command(device_under_file).is_err());

        // A well-formed patch is folded in without touching orender's paths.
        let good = r#"{"cmd":"save","args":{"speaker_layout":null,"enable_vbap":false,
            "output_backend":"asio","output_device":"ASIO4ALL v2","output_sample_rate":96000,
            "latency_target_ms":128}}"#;
        assert!(app.handle_command(good).is_ok());
        assert_eq!(app.config.panel.output_backend, "asio");
        assert_eq!(app.config.panel.output_sample_rate, 96_000);
        assert_eq!(app.config.panel.latency_target_ms, Some(128));
        assert_eq!(app.config.panel.enable_vbap, false);
    }

    /// The render block orender reads is derived from the panel block, so a
    /// patch must reach it — otherwise orender would start with stale settings.
    #[test]
    fn a_patch_reaches_the_render_block_orender_reads() {
        let mut app = PanelApp::new();
        // Never write the user's real config while testing.
        app.config_path = std::env::temp_dir().join("viola-panel-render-test.yaml");
        let patch = r#"{"cmd":"save","args":{"speaker_layout":null,"enable_vbap":false,
            "output_backend":"asio","output_device":"ASIO4ALL v2","output_sample_rate":96000,
            "latency_target_ms":128}}"#;
        app.handle_command(patch).expect("well-formed patch");
        app.config.sync_render_block();
        assert_eq!(app.config.get_render("output_backend").unwrap().as_str(), Some("asio"));
        assert_eq!(app.config.get_render("output_sample_rate").unwrap().as_u64(), Some(96_000));
        assert_eq!(
            app.config.get_render("output_device").unwrap().as_str(),
            Some("ASIO4ALL v2")
        );
        // `file`-only keys must be gone once the backend is asio.
        assert!(app.config.get_render("output_file").is_none());
        assert!(app.config.get_render("output_file_format").is_none());
        // VBAP off removes the key rather than writing false.
        assert!(app.config.get_render("enable_vbap").is_none());
    }
}
