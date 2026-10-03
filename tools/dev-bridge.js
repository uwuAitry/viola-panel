/* DEV ONLY — never shipped, never referenced by index.html.
 *
 * Supplies the same RPC surface app.js expects from the wry host, so the exact
 * production frontend can be reviewed in a plain browser. Loaded only by
 * tools/preview.ps1 into a generated copy.
 *
 * Deliberately uses the tricky data: a Chinese endpoint name (the string that
 * rendered as tofu under egui), a device that is NOT in the registry (to
 * exercise the "missing device" option) and a live PID.
 */
(function () {
  "use strict";

  var state = {
    settings: {
      speaker_layout: null,
      enable_vbap: true,
      output_backend: "asio",
      output_device: "ASIO4ALL v2",          // not in the registry list below
      output_sample_rate: 48000,
      latency_target_ms: 220
    },
    devices: [
      { key_name: "ASIO4ALL v2", description: "ASIO4ALL v2", clsid: "{B1}" },
      { key_name: "Realtek ASIO", description: "Realtek ASIO", clsid: "{B2}" }
    ],
    engine_running: true,
    engine_pid: 16652,
    engine_error: null,
    driver: {
      driver_name: "ASIO4ALL v2",
      driver_version: 2,
      input_channels: 2,
      output_channels: 8,
      current_sample_rate: 48000.0,
      buffer_min: 256,
      buffer_max: 2048,
      buffer_preferred: 512,
      buffer_granularity: 8,
      input_latency: [0, 256],
      output_latency: [0, 256]
    },
    driver_error: null,
    // A real Chinese endpoint name. Under egui this rendered as blank tofu
    // boxes; under WebView2 it should render as text.
    default_endpoint: "扬声器 (TANCHJIM BUNNY DSP)",
    default_endpoint_error: null,
    config_path: "C:\\Users\\aitry\\AppData\\Local\\viola-panel\\panel.yaml"
  };

  // Declared at module scope: the DOMContentLoaded hook below needs it.
  function push() { window.violaPanel.applyState(JSON.stringify(state)); }

  window.__violaStub = function (envelope) {
    var msg = JSON.parse(envelope);


    switch (msg.cmd) {
      case "get_state":
        push();
        break;
      case "start":
        state.engine_running = true;
        state.engine_pid = 20000 + Math.floor(Math.random() * 5000);
        push();
        window.violaPanel.applyResult(JSON.stringify({ ok: true, message: "Engine started" }));
        break;
      case "stop":
        state.engine_running = false;
        state.engine_pid = null;
        push();
        window.violaPanel.applyResult(JSON.stringify({ ok: true, message: "Engine stopped" }));
        break;
      case "apply":
        state.engine_running = true;
        push();
        window.violaPanel.applyResult(JSON.stringify({ ok: true, message: "Saved; engine restarted" }));
        break;
      case "probe_driver":
        push();
        window.violaPanel.applyResult(JSON.stringify({ ok: true, message: "Driver read" }));
        break;
      case "open_driver_panel":
        window.violaPanel.applyResult(JSON.stringify({ ok: false, error: "Driver refused to open", detail: "0x80070005 (access denied)" }));
        break;
      case "read_engine_log":
        window.violaPanel.applyLog("orender: 48000 Hz, 12 ch, file backend\nffplay: buffer 512 kB\n");
        break;
    }
  };

  // Push once at load; app.js's own get_state also triggers the stub.
  document.addEventListener("DOMContentLoaded", push);
})();
