# viola-panel

Control panel for the **output side** of the [viola-bridge](https://github.com/uwuAitry/viola-bridge)
chain. It configures and supervises how `orender` (Omniphony's spatial audio
renderer) delivers its rendered bed to an output device, and can open the control
panel of that output ASIO driver.

`viola-panel` is a standalone process. It does not link, load, or copy any
viola-bridge code — it only spawns `orender.exe` and `ffplay.exe` and reads the
public `HKLM\SOFTWARE\ASIO` registry contract.

## Status

Early: M0 skeleton (single-instance guard, window, log file). See
[docs/design.md](docs/design.md) for the full design consensus and the M0–M6 plan.

## Build

Windows only. Building requires a Rust toolchain and a Windows host:

```
cargo build --release
```

CI builds it on `windows-latest` and publishes `viola-panel.exe` as an artifact.

## Licence

GPL-3.0-or-later. See [LICENSE](LICENSE).

ASIO is a trademark and software of Steinberg Media Technologies GmbH.
