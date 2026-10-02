// SPDX-License-Identifier: GPL-3.0-or-later
//! `panel.yaml`: the one file this panel owns.
//!
//! It plays two roles at once (design decision T3):
//!
//! * the panel's own persistence (paths, last selection), under the `panel:` key;
//! * the `--config` document handed to `orender`, under the `render:` key.
//!
//! orender's config schema carries `#[serde(flatten)] extra` at every level and
//! never sets `deny_unknown_fields`, so the `panel:` block is silently accepted
//! and ignored by orender. The `render:` block is kept as a raw mapping and only
//! the keys this panel owns are written, so anything the user put there by hand
//! survives a round trip (design decision T1).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_yaml_ng::{Mapping, Value};

fn key(name: &str) -> Value {
    Value::String(name.to_string())
}

/// The `panel:` block. Every field has a default so a partial (or absent) block
/// still loads.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PanelSettings {
    /// `orender.exe`; `None` means "resolve by probing".
    pub orender_exe: Option<PathBuf>,
    /// `ffplay.exe`; `None` means "resolve from PATH".
    pub ffplay_exe: Option<PathBuf>,
    /// `viola_bridge.dll` passed to orender via `--bridge-path`.
    pub bridge_dll: Option<PathBuf>,
    /// Speaker layout YAML; `None` means "use the built-in 7.1.4 preset".
    pub speaker_layout: Option<PathBuf>,
    /// `--enable-vbap`.
    pub enable_vbap: bool,
    /// `file` or `asio`.
    pub output_backend: String,
    /// Exact ASIO device name; only meaningful for the `asio` backend.
    pub output_device: Option<String>,
    /// Output sample rate in Hz.
    pub output_sample_rate: u32,
    /// `--latency-target-ms`; `None` leaves orender's own default (220).
    pub latency_target_ms: Option<u32>,
}

impl Default for PanelSettings {
    fn default() -> Self {
        Self {
            orender_exe: None,
            ffplay_exe: None,
            bridge_dll: None,
            speaker_layout: None,
            enable_vbap: true,
            output_backend: "file".to_string(),
            output_device: None,
            output_sample_rate: 48_000,
            latency_target_ms: None,
        }
    }
}

/// The whole document. Top-level keys other than `panel` and `render` are dropped
/// on save; that is deliberate — this panel owns the file, and orender's own
/// extras live under `render:` where they are preserved.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PanelConfig {
    pub panel: PanelSettings,
    /// The `render:` mapping, preserved verbatim.
    pub render: Mapping,
}

impl Default for PanelConfig {
    fn default() -> Self {
        let mut config = Self {
            panel: PanelSettings::default(),
            render: Mapping::new(),
        };
        config.sync_render_block();
        config
    }
}

impl PanelConfig {
    /// Copy the panel's output settings into the `render:` mapping, leaving every
    /// other key in that mapping untouched.
    ///
    /// Note that `latency_target_ms` is deliberately *not* written here: it is a
    /// CLI argument (`--latency-target-ms`), and inventing a config key for it
    /// would be a lie that orender would silently ignore.
    pub fn sync_render_block(&mut self) {
        self.set("output_backend", Value::from(self.panel.output_backend.clone()));
        self.set(
            "output_sample_rate",
            Value::from(u64::from(self.panel.output_sample_rate)),
        );

        if self.panel.enable_vbap {
            self.set("enable_vbap", Value::from(true));
        } else {
            self.render.remove(key("enable_vbap"));
        }

        // The `file` backend writes raw f32 LE to stdout, so orender must be told
        // both the sink and the encoding. The `asio` backend writes to a device
        // and must not see either key.
        if self.panel.output_backend == "file" {
            self.set("output_file", Value::from("-"));
            self.set("output_file_format", Value::from("raw_f32"));
        } else {
            self.render.remove(key("output_file"));
            self.render.remove(key("output_file_format"));
        }

        match &self.panel.output_device {
            Some(device) if self.panel.output_backend == "asio" => {
                self.set("output_device", Value::from(device.clone()));
            }
            _ => {
                self.render.remove(key("output_device"));
            }
        }

        match &self.panel.speaker_layout {
            Some(path) => self.set("speaker_layout", Value::from(path.display().to_string())),
            None => {
                self.render.remove(key("speaker_layout"));
            }
        }
    }

    fn set(&mut self, name: &str, value: Value) {
        self.render.insert(key(name), value);
    }

    /// Read a key from the `render:` mapping.
    pub fn get_render(&self, name: &str) -> Option<&Value> {
        self.render.get(key(name))
    }

    /// Read `panel.yaml`, or return defaults if it does not exist yet.
    ///
    /// A file that exists but does not parse is an error rather than a silent
    /// reset: orender swallows its own parse failures and quietly reverts to
    /// defaults, and reproducing that behaviour here would destroy the user's
    /// settings without a word.
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
        };
        if text.trim().is_empty() {
            return Ok(Self::default());
        }
        let mut config: Self = serde_yaml_ng::from_str(&text)
            .map_err(|e| format!("cannot parse {}: {e}", path.display()))?;
        config.sync_render_block();
        Ok(config)
    }

    /// Write the file back, creating the parent directory if needed.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        let mut config = self.clone();
        config.sync_render_block();
        let text = serde_yaml_ng::to_string(&config)
            .map_err(|e| format!("cannot serialise panel config: {e}"))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
        }
        std::fs::write(path, text).map_err(|e| format!("cannot write {}: {e}", path.display()))
    }
}

// --- path resolution (design 4.5) -------------------------------------------

/// `%LOCALAPPDATA%\viola-panel`, the one directory this panel owns.
pub fn panel_dir() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|v| PathBuf::from(v).join("viola-panel"))
}

/// `%LOCALAPPDATA%\viola-panel\panel.yaml`.
pub fn default_config_path() -> Option<PathBuf> {
    panel_dir().map(|dir| dir.join("panel.yaml"))
}

/// Where Omniphony Studio installs `orender.exe` by default.
pub fn probe_orender() -> Option<PathBuf> {
    let dir = std::env::var_os("LOCALAPPDATA")?;
    let candidate = PathBuf::from(dir)
        .join("Programs")
        .join("Omniphony Studio")
        .join("orender.exe");
    candidate.is_file().then_some(candidate)
}

/// Resolve `ffplay.exe` on PATH.
pub fn probe_ffplay() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|dir| {
        let candidate = dir.join("ffplay.exe");
        candidate.is_file().then_some(candidate)
    })
}

/// The `viola_bridge.dll` the live chain uses by default.
pub fn probe_bridge_dll() -> Option<PathBuf> {
    let candidate =
        PathBuf::from(r"D:\viola-bridge\dist-live\viola-bridge-windows-x86_64\viola_bridge.dll");
    candidate.is_file().then_some(candidate)
}

/// The three external binaries the panel needs, after probing.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ResolvedPaths {
    pub orender_exe: Option<PathBuf>,
    pub ffplay_exe: Option<PathBuf>,
    pub bridge_dll: Option<PathBuf>,
}

/// Apply the probe order: an explicit setting wins, otherwise probe.
pub fn resolve_panel_paths(panel: &PanelSettings) -> ResolvedPaths {
    ResolvedPaths {
        orender_exe: panel.orender_exe.clone().or_else(probe_orender),
        ffplay_exe: panel.ffplay_exe.clone().or_else(probe_ffplay),
        bridge_dll: panel.bridge_dll.clone().or_else(probe_bridge_dll),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("viola-panel-config-test");
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    #[test]
    fn panel_dir_is_under_localappdata() {
        let dir = panel_dir().expect("LOCALAPPDATA should be set on Windows");
        assert!(dir.ends_with("viola-panel"));
        assert!(default_config_path().unwrap().ends_with("panel.yaml"));
    }

    #[test]
    fn defaults_describe_a_working_file_backend() {
        let config = PanelConfig::default();
        assert_eq!(config.panel.output_backend, "file");
        assert_eq!(config.panel.output_sample_rate, 48_000);
        assert_eq!(config.get_render("output_file"), Some(&Value::from("-")));
        assert_eq!(
            config.get_render("output_file_format"),
            Some(&Value::from("raw_f32"))
        );
    }

    #[test]
    fn round_trip_preserves_foreign_render_keys() {
        let path = scratch("round-trip.yaml");
        std::fs::write(
            &path,
            "panel:\n  output_backend: file\nrender:\n  binaural:\n    output_mode: binaural\n  master_gain: -3.0\n",
        )
        .unwrap();

        let config = PanelConfig::load(&path).unwrap();
        // A key the panel does not own must survive, nested and all.
        let binaural = config.get_render("binaural").expect("binaural kept");
        assert_eq!(binaural["output_mode"], Value::from("binaural"));

        config.save(&path).unwrap();
        let reloaded = PanelConfig::load(&path).unwrap();
        assert_eq!(reloaded, config);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn switching_backend_rewrites_only_the_owned_keys() {
        let mut config = PanelConfig::default();
        config.panel.output_backend = "asio".to_string();
        config.panel.output_device = Some("ASIO4ALL v2".to_string());
        config.sync_render_block();

        assert_eq!(config.get_render("output_file"), None);
        assert_eq!(config.get_render("output_file_format"), None);
        assert_eq!(
            config.get_render("output_device"),
            Some(&Value::from("ASIO4ALL v2"))
        );
    }

    #[test]
    fn a_missing_file_yields_defaults() {
        let path = scratch("does-not-exist.yaml");
        let _ = std::fs::remove_file(&path);
        assert_eq!(PanelConfig::load(&path).unwrap(), PanelConfig::default());
    }

    #[test]
    fn a_corrupt_file_is_an_error_not_a_silent_reset() {
        let path = scratch("corrupt.yaml");
        std::fs::write(&path, "panel: [this is not a mapping\n").unwrap();
        assert!(PanelConfig::load(&path).is_err());
        let _ = std::fs::remove_file(&path);
    }
}
