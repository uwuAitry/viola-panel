#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
// SPDX-License-Identifier: GPL-3.0-or-later
//! viola-panel - control panel for the orender output side of the viola-bridge chain.
//!
//! M5: status bar, output settings, render settings, Start / Stop / Apply.
//! See docs/design.md for the full design.
//!
//! All UI strings are English because egui's default fonts carry no CJK glyphs;
//! Chinese labels would render as blank boxes unless a font is bundled (design.md
//! 10 records this as a known gap).
mod asiodrv;
mod config;
mod launcher;
mod mmdev;
mod registry;

use eframe::egui;
use std::path::{Path, PathBuf};
use std::sync::mpsc;

use asiodrv::DriverInfo;
use config::{PanelConfig, ResolvedPaths};
use launcher::{Engine, LaunchSpec};
use registry::AsioDevice;

/// How many lines of the engine log the status bar shows after a failure.
const LOG_TAIL_LINES: usize = 12;

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

/// The window's mutable state. Everything the UI can change lives here, so the
/// render pass is a pure function of this struct.
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
    fn start_engine(&mut self) {
        self.stop_engine();

        if let Err(e) = self.save_config() {
            self.status = format!("cannot save config: {e}");
            self.status_is_error = true;
            return;
        }
        let spec = match self.build_spec() {
            Ok(spec) => spec,
            Err(e) => {
                self.status = e;
                self.status_is_error = true;
                return;
            }
        };

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
            }
            Err(e) => {
                log(&format!("engine start failed: {e}"));
                self.status = format!("cannot start engine: {e}");
                self.status_is_error = true;
            }
        }
    }

    /// Apply = save, then restart, because orender reads its output settings only
    /// at startup (design.md 4.2).
    fn apply(&mut self) {
        self.start_engine();
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
            log(&format!("engine exited: {text}"));
            let tail = engine_log_path()
                .map(|p| log_tail(&p, LOG_TAIL_LINES))
                .unwrap_or_default();
            if !tail.is_empty() {
                log(&format!("engine log tail:\n{tail}"));
            }
        }
    }

    /// Collect results from the detached probe / control-panel threads.
    fn poll_channels(&mut self) {
        if let Some(rx) = &self.driver_rx {
            match rx.try_recv() {
                Ok(result) => {
                    self.driver = Some(result);
                    self.driver_rx = None;
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.driver = Some(Err("driver probe thread died".to_string()));
                    self.driver_rx = None;
                }
            }
        }

        if let Some(rx) = &self.panel_rx {
            match rx.try_recv() {
                Ok(Ok(())) => {
                    self.panel_status = Some("Driver control panel closed.".to_string());
                    self.panel_rx = None;
                }
                Ok(Err(e)) => {
                    self.panel_status = Some(format!("Control panel failed: {e}"));
                    self.panel_rx = None;
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.panel_status =
                        Some("Control panel closed; the thread reported no result.".to_string());
                    self.panel_rx = None;
                }
            }
        }
    }

    /// Ask a detached thread to open the selected driver's own control panel. It
    /// blocks until the user closes that window, so it must never be the UI thread.
    fn open_driver_control_panel(&mut self) {
        let Some(clsid) = self.selected_clsid() else {
            self.panel_status = Some("No driver CLSID for the selected device.".to_string());
            return;
        };
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let result = asiodrv::open_control_panel(&clsid);
            let _ = tx.send(result);
        });
        self.panel_rx = Some(rx);
        self.panel_status = Some("Opening the driver control panel...".to_string());
    }

    /// Ask a detached thread to read the selected driver's capabilities.
    fn probe_driver(&mut self) {
        let Some(clsid) = self.selected_clsid() else {
            self.driver = Some(Err("No driver CLSID for the selected device.".to_string()));
            return;
        };
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let result = asiodrv::probe(&clsid);
            let _ = tx.send(result);
        });
        self.driver_rx = Some(rx);
        self.driver = None;
    }

    // --- rendering ---------------------------------------------------------

    fn status_bar(&self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let (dot, colour) = if self.engine.is_some() {
                ("●", egui::Color32::from_rgb(90, 190, 90))
            } else {
                ("○", egui::Color32::from_rgb(150, 150, 150))
            };
            ui.colored_label(colour, dot);
            ui.label(self.status.as_str());
            ui.separator();
            ui.label(format!("{} Hz", self.config.panel.output_sample_rate));
            ui.separator();
            ui.label(format!("{}", self.config.panel.output_backend));
        });
        if self.status_is_error {
            ui.colored_label(egui::Color32::from_rgb(220, 90, 90), "See the panel log for details.");
        }
    }

    fn output_section(&mut self, ui: &mut egui::Ui) {
        ui.heading("Output");
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label("Backend");
                let backends = ["file", "asio"];
                egui::ComboBox::from_id_salt("backend-combo")
                    .selected_text(self.config.panel.output_backend.clone())
                    .show_ui(ui, |ui| {
                        for backend in backends {
                            ui.selectable_value(
                                &mut self.config.panel.output_backend,
                                backend.to_string(),
                                backend,
                            );
                        }
                    });
            });

            // The device only exists for the `asio` backend; `file` writes to stdout
            // and the real sink is ffplay's own default device.
            let is_asio = self.config.panel.output_backend == "asio";
            if is_asio {
                ui.horizontal(|ui| {
                    ui.label("Device");
                    let devices = &self.devices;
                    let target = &mut self.config.panel.output_device;
                    let selected = target.clone().unwrap_or_else(|| "(none)".to_string());
                    egui::ComboBox::from_id_salt("device-combo")
                        .selected_text(selected)
                        .width(280.0)
                        .show_ui(ui, |ui| {
                            for device in devices {
                                ui.selectable_value(
                                    target,
                                    Some(device.description.clone()),
                                    device.description.as_str(),
                                );
                            }
                        });
                });
                if self.devices.is_empty() {
                    ui.colored_label(
                        egui::Color32::from_rgb(220, 160, 60),
                        "No ASIO devices in HKLM\\SOFTWARE\\ASIO.",
                    );
                }
            } else {
                ui.label("Backend `file` writes raw f32 to ffplay, which uses the Windows default device.");
            }

            ui.horizontal(|ui| {
                ui.label("Sample rate");
                let rates = [44_100u32, 48_000, 88_200, 96_000, 176_400, 192_000];
                egui::ComboBox::from_id_salt("rate-combo")
                    .selected_text(format!("{}", self.config.panel.output_sample_rate))
                    .show_ui(ui, |ui| {
                        for rate in rates {
                            ui.selectable_value(
                                &mut self.config.panel.output_sample_rate,
                                rate,
                                format!("{rate}"),
                            );
                        }
                    });
            });

            ui.horizontal(|ui| {
                ui.label("Latency target (ms)");
                let mut enabled = self.config.panel.latency_target_ms.is_some();
                if ui.checkbox(&mut enabled, "").changed() {
                    self.config.panel.latency_target_ms = if enabled { Some(220) } else { None };
                }
                if let Some(latency) = self.config.panel.latency_target_ms.as_mut() {
                    ui.add(egui::Slider::new(latency, 25..=250).text("ms"));
                } else {
                    ui.label("orender default (220)");
                }
            });

            ui.separator();

            // Driver information needs the driver, which orender owns while running.
            let probe_enabled = self.engine.is_none() && self.selected_clsid().is_some();
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(probe_enabled, egui::Button::new("Read driver info"))
                    .on_hover_text("Probes the selected driver. Only possible while the engine is stopped.")
                    .clicked()
                {
                    self.probe_driver();
                }
                if ui
                    .add_enabled(probe_enabled, egui::Button::new("Open driver control panel"))
                    .on_hover_text("Opens the driver's own settings window.")
                    .clicked()
                {
                    self.open_driver_control_panel();
                }
            });

            match &self.driver {
                Some(Ok(info)) => {
                    ui.monospace(format!(
                        "{} v{}  in {}ch / out {}ch  {} Hz",
                        info.driver_name,
                        info.driver_version,
                        info.input_channels,
                        info.output_channels,
                        info.current_sample_rate,
                    ));
                    ui.monospace(format!(
                        "buffer {}..{} (preferred {}, gran {})  latency in {} / out {}",
                        info.buffer_min,
                        info.buffer_max,
                        info.buffer_preferred,
                        info.buffer_granularity,
                        info.input_latency.0,
                        info.output_latency.0
                    ));
                }
                Some(Err(e)) => {
                    ui.colored_label(egui::Color32::from_rgb(220, 160, 60), e.as_str());
                }
                None => {
                    if self.driver_rx.is_some() {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label("Probing driver...");
                        });
                    } else if self.engine.is_some() {
                        ui.label("Driver info unavailable while the engine is running.");
                    }
                }
            }
            if let Some(text) = &self.panel_status {
                ui.label(text.as_str());
            }

            ui.separator();
            ui.horizontal(|ui| {
                ui.label("System default endpoint");
                match &self.default_endpoint {
                    Ok(name) => {
                        ui.monospace(name.as_str());
                    }
                    Err(e) => {
                        ui.colored_label(egui::Color32::from_rgb(220, 160, 60), e.as_str());
                    }
                }
            });
            ui.label("Change it in Windows Settings > System > Sound. This panel cannot.");
        });
    }

    fn render_section(&mut self, ui: &mut egui::Ui) {
        ui.heading("Render");
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.checkbox(&mut self.config.panel.enable_vbap, "Enable VBAP");

            ui.horizontal(|ui| {
                ui.label("Speaker layout");
                let mut text = self
                    .config
                    .panel
                    .speaker_layout
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default();
                let response = ui.add(
                    egui::TextEdit::singleline(&mut text)
                        .desired_width(300.0)
                        .hint_text("(orender's built-in 7.1.4 preset)"),
                );
                if response.changed() {
                    self.config.panel.speaker_layout =
                        if text.trim().is_empty() { None } else { Some(PathBuf::from(&text)) };
                }
            });
            ui.label("Leave empty to use orender's built-in 7.1.4 (12 ch) preset.");

            ui.separator();
            // Design.md 6.1: the driver's own counters live in the ASIO DLL's address
            // space and are not readable from here. Only our own process state is real.
            ui.label("Live counters from the driver are not available to a separate process.");
            match self.engine.as_ref() {
                Some(engine) => {
                    ui.monospace(format!("engine: running (PID {})", engine.pid()));
                }
                None => {
                    ui.monospace("engine: stopped");
                }
            }
            ui.monospace(format!("config: {}", self.config_path.display()));
        });
    }

    fn buttons(&mut self, ui: &mut egui::Ui) {
        let running = self.engine.is_some();
        ui.horizontal(|ui| {
            if ui
                .add_enabled(!running, egui::Button::new("Start"))
                .on_hover_text("Launch orender | ffplay and connect the input pipe.")
                .clicked()
            {
                self.start_engine();
            }
            if ui
                .add_enabled(running, egui::Button::new("Stop"))
                .on_hover_text("Stop the pipeline this panel started.")
                .clicked()
            {
                self.stop_engine();
            }
            if ui
                .add_enabled(running, egui::Button::new("Apply (restart)"))
                .on_hover_text("Save panel.yaml and restart orender. Audio drops once.")
                .clicked()
            {
                self.apply();
            }
        });
    }
}

impl eframe::App for PanelApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.poll_channels();
        self.poll_engine();
        // Keep the status bar honest while the child runs.
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(500));

        egui::CentralPanel::default().show(ui, |ui| {
            self.status_bar(ui);
            ui.separator();
            egui::ScrollArea::vertical().show(ui, |ui| {
                self.output_section(ui);
                ui.add_space(8.0);
                self.render_section(ui);
                ui.add_space(8.0);
                self.buttons(ui);
                ui.add_space(8.0);
                ui.label("ASIO is a trademark and software of Steinberg Media Technologies GmbH.");
            });
        });
    }
}

fn main() -> eframe::Result {
    if !acquire_single_instance() {
        log("second instance refused (already running)");
        return Ok(());
    }
    log("panel starting");

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("viola-panel")
            .with_inner_size([720.0, 620.0])
            .with_min_inner_size([560.0, 480.0]),
        ..Default::default()
    };

    let result = eframe::run_native(
        "viola-panel",
        options,
        Box::new(|_cc| Ok(Box::new(PanelApp::new()))),
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
}
