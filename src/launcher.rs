// SPDX-License-Identifier: GPL-3.0-or-later
//! Launching `orender | ffplay` and killing exactly that process tree.
//!
//! The plumbing is one `cmd /S /C "<orender> | <ffplay>"` child: cmd.exe is what
//! wires one process's stdout into the next one's stdin on Windows. Both children
//! append to a single log file so the status bar can read its tail.
//!
//! The argv literal is built by [`build_command_line`] and pinned by unit tests,
//! so it cannot silently drift from orender's CLI (docs/design.md 4.1).

use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};

/// `CREATE_NO_WINDOW`: a GUI process must not flash a console for each child.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// The named pipe orender reads its spatial input from (hard-coded, design.md 1.6).
const INPUT_PIPE: &str = r"\\.\pipe\orender.input";

/// Everything needed to build the launch command line and spawn it.
pub struct LaunchSpec {
    /// `orender.exe` (path resolution happens before this type is built).
    pub orender_exe: PathBuf,
    /// `ffplay.exe`; a bare `ffplay` is also fine, since it resolves via PATH.
    pub ffplay_exe: PathBuf,
    /// `viola_bridge.dll`, required by orender's `--bridge-path`.
    pub bridge_dll: PathBuf,
    /// YAML speaker-layout file; `None` leaves orender's own config in charge.
    pub speaker_layout: Option<PathBuf>,
    /// VBAP on/off. There is no `--disable-vbap`, so "off" means "emit nothing".
    pub enable_vbap: bool,
    /// `"file"` (stdout -> ffplay) or `"asio"` (straight to a driver).
    pub output_backend: String,
    /// Exact ASIO driver name; only meaningful for the `asio` backend.
    pub output_device: Option<String>,
    /// Output sample rate, passed to both orender and ffplay.
    pub output_sample_rate: u32,
    /// `--latency-target-ms`; `None` leaves orender's own default (220). Emitted
    /// only when set, so the default argv stays exactly what design.md 4.1 pins.
    pub latency_target_ms: Option<u32>,
    /// `--config` for orender, and the panel's own settings file.
    pub config_path: PathBuf,
    /// stdout+stderr of the whole pipeline are appended here.
    pub log_path: PathBuf,
}

/// Wrap `s` in double quotes, doubling any embedded `"`.
///
/// Limits, deliberately: this is not cmd.exe's full quoting grammar. It handles
/// spaces and literal quotes (the two things Windows paths actually contain) and
/// assumes the value has no metacharacters (`& | < > ^ %`) and does not end in a
/// backslash before the closing quote -- a trailing `\` would start an escape
/// sequence. None of the values passed here are attacker-controlled.
fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        if c == '"' {
            out.push('"');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// Quote an executable path only when it needs it.
///
/// The real `orender.exe` lives under `...\Programs\Omniphony Studio\`, so quoting
/// a space-containing path is mandatory for it to run at all. A space-free path is
/// emitted bare, which keeps the line starting with the executable path (pinned by
/// the unit tests, and what design.md 4.1 shows).
fn quote_exe(p: &Path) -> String {
    let s = p.to_string_lossy();
    if s.contains(' ') || s.contains('"') {
        quote(&s)
    } else {
        s.into_owned()
    }
}

/// Build the full command line, without any leading `cmd /S /C ` (added by [`start`]).
///
/// The shape is exactly docs/design.md 4.1, itself isomorphic to viola-bridge's
/// known-good `scripts/live-playout.ps1`:
///
/// ```text
/// <orender.exe> render "\\.\pipe\orender.input" --continuous --bridge-path "<dll>"
///   [--enable-vbap] [--speaker-layout "<layout.yaml>"]
///   --output-backend <file|asio> [--output-device "<name>"] --output-sample-rate <rate>
///   [--output-file - --output-file-format raw-f32]        (file backend only)
///   --config "<panel.yaml>" --loglevel info
///   | <ffplay.exe> -nodisp -hide_banner -loglevel info -f f32le -ar <rate> -ch_layout stereo -i -
/// ```
pub fn build_command_line(spec: &LaunchSpec) -> String {
    let mut line = String::new();

    line.push_str(&quote_exe(&spec.orender_exe));
    line.push_str(" render ");
    line.push_str(&quote(INPUT_PIPE));
    line.push_str(" --continuous --bridge-path ");
    line.push_str(&quote(&spec.bridge_dll.to_string_lossy()));

    if spec.enable_vbap {
        line.push_str(" --enable-vbap");
    }
    if let Some(layout) = &spec.speaker_layout {
        line.push_str(" --speaker-layout ");
        line.push_str(&quote(&layout.to_string_lossy()));
    }

    line.push_str(" --output-backend ");
    line.push_str(&spec.output_backend);
    if let Some(device) = &spec.output_device {
        line.push_str(" --output-device ");
        line.push_str(&quote(device));
    }
    line.push_str(&format!(" --output-sample-rate {}", spec.output_sample_rate));

    // `--latency-target-ms` is a CLI-only argument (the config file has no key for
    // it), so it is emitted here or not at all.
    if let Some(latency) = spec.latency_target_ms {
        line.push_str(&format!(" --latency-target-ms {latency}"));
    }

    // Raw f32 on stdout is what the `file` backend means; the `asio` backend
    // writes to the driver instead and takes neither flag.
    if spec.output_backend == "file" {
        line.push_str(" --output-file - --output-file-format raw-f32");
    }

    line.push_str(" --config ");
    line.push_str(&quote(&spec.config_path.to_string_lossy()));
    line.push_str(" --loglevel info | ");
    line.push_str(&quote_exe(&spec.ffplay_exe));
    line.push_str(&format!(
        " -nodisp -hide_banner -loglevel info -f f32le -ar {} -ch_layout stereo -i -",
        spec.output_sample_rate
    ));

    line
}

/// A live pipeline: the `cmd.exe` shell holding orender and ffplay.
pub struct Engine {
    child: Child,
}

impl Engine {
    /// PID of the shell; `taskkill /T` on it reaches orender and ffplay too.
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Non-blocking poll. `Ok(None)` means still running.
    pub fn try_exit(&mut self) -> std::io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }

    /// Kill this process tree (cmd + orender + ffplay). Best effort, never panics.
    pub fn kill(&mut self) {
        let pid = self.child.id().to_string();

        // `/T` takes the whole tree, `/F` forces it. Run through cmd rather than
        // relying on CreateProcess to find taskkill.exe.
        if let Ok(mut killer) = hidden_cmd()
            .args(["/C", "taskkill", "/PID"])
            .arg(&pid)
            .args(["/T", "/F"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            let _ = killer.wait();
        }

        // Belt and braces: if taskkill is unavailable, at least reap the shell.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Spawn `cmd /S /C "<line>"` with no console window; stdout+stderr appended to
/// `spec.log_path`; stdin null. Creates the log's parent directory if needed.
pub fn start(spec: &LaunchSpec) -> std::io::Result<Engine> {
    if let Some(parent) = spec.log_path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&spec.log_path)?;
    let log_err = log.try_clone()?;

    let line = build_command_line(spec);
    let mut cmd = hidden_cmd();
    cmd_line(&mut cmd, &line);
    let child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err))
        .spawn()?;

    Ok(Engine { child })
}

/// `cmd.exe` with no console window. The only place `CommandExt` is needed.
fn hidden_cmd() -> Command {
    let mut cmd = Command::new("cmd");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

/// Append cmd.exe's `/S /C "<line>"` form to `cmd`.
///
/// The line must reach cmd.exe through [`CommandExt::raw_arg`]: `args` escapes
/// the line's own quotes as `\"`, which cmd.exe does not undo, so a quoted
/// `orender.exe` path was chopped at its first space and the engine died with
/// exit 255. `/S` also strips the outer quotes, so the line needs its own pair.
fn cmd_line(cmd: &mut Command, line: &str) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.raw_arg("/S").raw_arg("/C").raw_arg(format!("\"{line}\""));
    }
    #[cfg(not(windows))]
    {
        cmd.args(["/S", "/C", line]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(backend: &str, enable_vbap: bool, layout: Option<&str>) -> LaunchSpec {
        LaunchSpec {
            orender_exe: PathBuf::from(r"C:\orender\orender.exe"),
            ffplay_exe: PathBuf::from(r"C:\ffmpeg\ffplay.exe"),
            bridge_dll: PathBuf::from(r"C:\bridge\viola_bridge.dll"),
            speaker_layout: layout.map(PathBuf::from),
            enable_vbap,
            output_backend: backend.to_string(),
            output_device: Some("ASIO4ALL v2".to_string()),
            output_sample_rate: 48_000,
            latency_target_ms: None,
            config_path: PathBuf::from(r"C:\cfg\panel.yaml"),
            log_path: PathBuf::from(r"C:\logs\panel.log"),
        }
    }

    /// The literal that must not drift: file backend, VBAP on, layout given.
    #[test]
    fn file_backend_line_pins_the_argv() {
        let line = build_command_line(&spec("file", true, Some(r"C:\cfg\layout.yaml")));

        assert!(
            line.starts_with(r"C:\orender\orender.exe"),
            "line must start with the orender path: {line}"
        );
        for needle in [
            r#"render "\\.\pipe\orender.input" --continuous"#,
            r#"--bridge-path "C:\bridge\viola_bridge.dll""#,
            "--enable-vbap",
            r#"--speaker-layout "C:\cfg\layout.yaml""#,
            "--output-backend file",
            "--output-sample-rate 48000",
            "--output-file -",
            "--output-file-format raw-f32",
            r#"--config "C:\cfg\panel.yaml""#,
            "--loglevel info",
            " | ",
        ] {
            assert!(line.contains(needle), "missing {needle:?} in {line}");
        }

        let ffplay = line.split_once(" | ").expect("pipe separator").1;
        assert!(
            ffplay.starts_with(r"C:\ffmpeg\ffplay.exe "),
            "ffplay segment must start with the exe: {ffplay}"
        );
        assert!(
            ffplay.contains(
                "-nodisp -hide_banner -loglevel info -f f32le -ar 48000 -ch_layout stereo -i -"
            ),
            "{ffplay}"
        );
    }

    /// The `asio` backend names a device and writes to it, so the stdout sink and
    /// its format flag must not appear at all.
    #[test]
    fn asio_backend_uses_a_device_and_drops_the_file_sink() {
        let line = build_command_line(&spec("asio", true, None));

        assert!(line.contains(r#"--output-device "ASIO4ALL v2""#), "{line}");
        assert!(line.contains("--output-backend asio"), "{line}");
        assert!(!line.contains("--output-file"), "{line}");
        assert!(line.contains(" | "), "{line}");
    }

    /// Off means "no flag", not a made-up `--disable-vbap` orender would reject.
    #[test]
    fn vbap_flag_is_absent_when_disabled() {
        let line = build_command_line(&spec("file", false, None));

        assert!(!line.contains("vbap"), "{line}");
        assert!(!line.contains("--speaker-layout"), "{line}");
    }

    /// Quoting rules the two helpers above document, pinned so they cannot regress.
    #[test]
    fn quoting_handles_spaces_and_embedded_quotes() {
        assert_eq!(quote("plain"), "\"plain\"");
        assert_eq!(quote("a\"b"), "\"a\"\"b\"");
        assert_eq!(quote_exe(Path::new(r"C:\orender.exe")), r"C:\orender.exe");
        assert_eq!(
            quote_exe(Path::new(r"C:\Omniphony Studio\orender.exe")),
            "\"C:\\Omniphony Studio\\orender.exe\""
        );
    }

    /// The latency flag is CLI-only and must appear only when the user sets it;
    /// leaving it out is what keeps the default line identical to design.md 4.1.
    #[test]
    fn latency_flag_is_emitted_only_when_set() {
        let mut s = spec("asio", true, None);
        assert!(!build_command_line(&s).contains("--latency-target-ms"));

        s.latency_target_ms = Some(220);
        assert!(build_command_line(&s).contains("--latency-target-ms 220"));
    }

    /// The defect this pins: `Command::args` escapes a line's own quotes as
    /// `\"`, which cmd.exe does not undo, so a quoted `orender.exe` path was
    /// chopped at its first space and the engine died with exit 255. Spawns a
    /// real cmd.exe and asserts the quotes arrive intact and unescaped.
    #[test]
    #[cfg(windows)]
    fn cmd_receives_the_line_with_quotes_intact() {
        let mut cmd = hidden_cmd();
        cmd_line(&mut cmd, r#"echo "omni phony""#);
        let out = cmd
            .stdout(Stdio::piped())
            .output()
            .expect("spawn cmd");

        assert!(out.status.success(), "cmd failed: {out:?}");
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            r#""omni phony""#,
            "cmd mangled the quoting path"
        );
    }
}
