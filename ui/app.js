/* viola-panel frontend.
 *
 * ONE code path. Under the wry host, `window.ipc.postMessage` is the real
 * channel; in a plain browser (local design review, no Rust toolchain needed)
 * `installBridgeStub()` supplies the same RPC surface backed by a fake state.
 * There is never a second UI.
 *
 * Rust -> JS: the host calls `window.violaPanel.applyState(json)`,
 * `window.violaPanel.applyResult(json)` and `window.violaPanel.applyLog(text)`.
 * JS -> Rust: one JSON envelope per message, `{ "cmd": ..., "args": ... }`.
 */
(function () {
  "use strict";

  // ── RPC ──────────────────────────────────────────────────────────────────
  // Under wry, `window.ipc.postMessage` exists before this script runs.
  var hasHost = typeof window.ipc !== "undefined" &&
                typeof window.ipc.postMessage === "function";

  function send(cmd, args) {
    var envelope = JSON.stringify({ cmd: cmd, args: args === undefined ? null : args });
    if (hasHost) {
      window.ipc.postMessage(envelope);
    } else if (window.__violaStub) {
      window.__violaStub(envelope);
    } else {
      console.warn("viola-panel: no bridge; dropping", cmd);
    }
  }

  // ── DOM helpers ──────────────────────────────────────────────────────────
  function $(id) { return document.getElementById(id); }

  // ── State ────────────────────────────────────────────────────────────────
  // `last` is the last snapshot the host pushed; it is the source of truth for
  // every render, so a push only ever re-renders from one object.
  var last = null;
  // Unsaved edits. The host pushes a full snapshot every 500 ms, and `render`
  // writes every control back from it — which silently reverted a choice the
  // user had made but not yet submitted (picking `asio` snapped back to `file`
  // before Start/Apply could be pressed). While this is set, a push still
  // refreshes the derived readouts but never touches the settings controls.
  var edits = false;
  // True only for the commands that carry the form; a driver readout result
  // must not be mistaken for the host accepting the settings.
  var submitted = false;
  // Sample-rate candidates. The panel's own setting is authoritative here; if
  // the driver probe reported a rate, that value is offered first.
  var RATES = [44100, 48000, 88200, 96000, 176400, 192000];

  // ── Status lamp / text ───────────────────────────────────────────────────
  function renderStatus(s) {
    var lamp = $("status-lamp");
    var text = $("status-text");

    if (s.engine_error) {
      lamp.dataset.state = "broken";
      text.dataset.error = "true";
      text.textContent = s.engine_error;
    } else if (s.engine_running) {
      lamp.dataset.state = "flow";
      text.dataset.error = "false";
      text.textContent = "Engine running" + (s.engine_pid ? " · PID " + s.engine_pid : "");
    } else {
      lamp.dataset.state = "idle";
      text.dataset.error = "false";
      text.textContent = "Engine stopped";
    }
  }

  // ── Signal chain ─────────────────────────────────────────────────────────
  // The chain reflects measured facts only. `host`, `driver` and `pipe` are
  // upstream of this process and are not observable from here, so they stay
  // dim rather than claiming a state this panel cannot know.
  function setNode(name, state, meta) {
    var node = document.querySelector('.chain__node[data-node="' + name + '"]');
    if (!node) return;
    node.dataset.state = state;
    node.querySelector(".lamp").dataset.state = state;
    if (meta !== undefined) {
      var metaEl = node.querySelector(".chain__meta");
      if (metaEl) metaEl.textContent = meta;
    }
  }

  function renderChain(s) {
    // The break in the chain is the whole point of the diagram: everything
    // downstream of orender only flows while orender is alive.
    if (s.engine_running) {
      setNode("orender", "flow", "PID " + (s.engine_pid || "?"));
      setNode("ffplay", "flow", "sink");
      // The endpoint's own label is its name line; its meta stays "Windows
      // default" so the same long string is not printed twice in one node.
      setNode("endpoint", "flow");
    } else {
      setNode("orender", "idle", "stopped");
      setNode("ffplay", "idle", "sink");
      setNode("endpoint", "idle");
    }

    // The pipe is only reachable while orender owns the server end.
    setNode("pipe", s.engine_running ? "flow" : "idle", "orender.input");

    var endpointName = $("chain-endpoint");
    endpointName.textContent = s.default_endpoint || "output";
    endpointName.title = s.default_endpoint || "";
  }

  // ── Output section ───────────────────────────────────────────────────────
  function fillSelect(sel, values, current) {
    sel.textContent = "";
    values.forEach(function (v) {
      var opt = document.createElement("option");
      opt.value = String(v);
      opt.textContent = String(v);
      if (String(v) === String(current)) opt.selected = true;
      sel.appendChild(opt);
    });
  }

  function renderOutput(s) {
    if (edits) {
      renderReadout(s);
      renderEndpoint(s);
      applyBackendVisibility($("backend").value);
      return;
    }
    $("backend").value = s.settings.output_backend || "file";

    // Device list comes from the registry enumeration the host already did;
    // this is a read-only list (design.md §2 #25).
    var deviceSel = $("device");
    var devices = s.devices || [];
    deviceSel.textContent = "";
    if (devices.length === 0) {
      var empty = document.createElement("option");
      empty.textContent = "(no ASIO devices in the registry)";
      empty.value = "";
      deviceSel.appendChild(empty);
    } else {
      devices.forEach(function (d) {
        var opt = document.createElement("option");
        opt.value = d.description;       // --output-device matches this
        opt.textContent = d.description;
        opt.title = d.key_name;
        deviceSel.appendChild(opt);
      });
      deviceSel.value = s.settings.output_device || "";
      // A configured device that no longer exists must not silently look
      // selected as the first entry; surface it as its own option.
      if (deviceSel.value !== (s.settings.output_device || "")) {
        var missing = document.createElement("option");
        missing.value = s.settings.output_device;
        missing.textContent = s.settings.output_device + "  (not in registry)";
        deviceSel.appendChild(missing);
        deviceSel.value = s.settings.output_device;
      }
    }

    // Sample rates: offer the driver's reported rate first when we have one.
    var rates = RATES.slice();
    if (s.driver && s.driver.current_sample_rate) {
      var reported = Math.round(s.driver.current_sample_rate);
      if (rates.indexOf(reported) === -1) rates.unshift(reported);
    }
    fillSelect($("rate"), rates, s.settings.output_sample_rate);

    // Latency: the checkbox is the "use orender's default" escape hatch, so
    // an absent value disables the slider rather than showing a guessed one.
    var lat = s.settings.latency_target_ms;
    var hasLat = lat !== null && lat !== undefined;
    $("latency-on").checked = hasLat;
    $("latency").disabled = !hasLat;
    $("latency").value = hasLat ? lat : 220;
    $("latency-out").textContent = hasLat ? lat + " ms" : "orender default";

    renderReadout(s);
    renderEndpoint(s);
    applyBackendVisibility(s.settings.output_backend);
  }

  function applyBackendVisibility(backend) {
    // design.md §2 #15: the whole device area is hidden under `file`.
    var asio = backend === "asio";
    $("row-device").hidden = !asio;
    $("file-note").hidden = asio;

    // Driver probing is only meaningful for the ASIO backend, and only while
    // the engine is stopped (design.md §2 #8/#26).
    var stopped = !(last && last.engine_running);
    $("probe").disabled = !asio || !stopped;
    $("driver-panel").disabled = !asio || !stopped || !$("device").value;
  }

  function renderReadout(s) {
    var el = $("driver-readout");
    if (s.driver_error) {
      el.textContent = s.driver_error;
      return;
    }
    var d = s.driver;
    if (!d) {
      el.textContent = "";
      return;
    }
    var lines = [
      d.driver_name + "  v" + d.driver_version,
      "channels  in " + d.input_channels + " / out " + d.output_channels,
      "rate      " + d.current_sample_rate + " Hz",
      "buffer    " + d.buffer_min + ".." + d.buffer_max +
        "  (pref " + d.buffer_preferred + ", gran " + d.buffer_granularity + ")",
      "latency   in " + d.input_latency[0] + "/" + d.input_latency[1] +
        "  out " + d.output_latency[0] + "/" + d.output_latency[1]
    ];
    el.textContent = lines.join("\n");
  }

  function renderEndpoint(s) {
    var el = $("endpoint");
    if (s.default_endpoint_error) {
      el.textContent = s.default_endpoint_error;
      el.dataset.error = "true";
    } else {
      // This is the string that used to render as tofu boxes under egui.
      el.textContent = s.default_endpoint || "—";
      el.dataset.error = "false";
    }
    $("chain-endpoint").textContent = s.default_endpoint || "output";
  }

  // ── Render section ───────────────────────────────────────────────────────
  function renderRender(s) {
    if (!edits) {
      $("vbap").checked = !!s.settings.enable_vbap;
      $("layout").value = s.settings.speaker_layout || "";
    }
    $("fact-engine").textContent = s.engine_running ? "running" : "stopped";
    $("fact-config").textContent = s.config_path || "—";
  }

  // ── Whole-state render ───────────────────────────────────────────────────
  function render(s) {
    last = s;
    renderChain(s);
    renderStatus(s);
    renderOutput(s);
    renderRender(s);
    // Save/Start/Stop stay enabled; Apply is only useful with a saved config.
    $("start").disabled = false;
    $("stop").disabled = !s.engine_running;
    $("apply").disabled = false;
  }

  // ── Toasts ───────────────────────────────────────────────────────────────
  // Exit is 250 ms and entrance 350 ms (transitions.dev's open-slow /
  // close-fast asymmetry); the delay lives on the show rule only.
  function toast(message, kind) {
    var host = $("toast-host");
    var el = document.createElement("div");
    el.className = "toast";
    el.dataset.kind = kind || "ok";
    el.dataset.show = "false";
    el.textContent = message;
    host.appendChild(el);

    requestAnimationFrame(function () {
      requestAnimationFrame(function () { el.dataset.show = "true"; });
    });

    setTimeout(function () {
      el.dataset.show = "false";
      setTimeout(function () {
        if (el.parentNode) el.parentNode.removeChild(el);
      }, 300);
    }, kind === "error" ? 6000 : 3200);
  }

  // ── Host -> JS entry points ──────────────────────────────────────────────
  window.violaPanel = {
    applyState: function (json) {
      var s;
      try {
        s = typeof json === "string" ? JSON.parse(json) : json;
      } catch (e) {
        toast("Bad state from host: " + e.message, "error");
        return;
      }
      render(s);
    },

    applyResult: function (json) {
      var r = typeof json === "string" ? JSON.parse(json) : json;
      if (r.ok && submitted) {
        // The host took the settings, so the next snapshot is the truth again.
        edits = false;
        if (r.message) toast(r.message, "ok");
      } else {
        toast(r.error || "Operation failed", "error");
        if (r.detail) toast(r.detail, "error");
      }
      submitted = false;
      if (r.refresh) send("get_state");
    },

    applyLog: function (text) {
      $("log").textContent = text || "(empty)";
    },
    toast: toast
  };

  // ── Wiring ───────────────────────────────────────────────────────────────
  function settingsFromForm() {
    var latencyOn = $("latency-on").checked;
    return {
      speaker_layout: $("layout").value.trim() || null,
      enable_vbap: $("vbap").checked,
      output_backend: $("backend").value,
      output_device: $("device").value || null,
      output_sample_rate: parseInt($("rate").value, 10) || 48000,
      latency_target_ms: latencyOn ? parseInt($("latency").value, 10) : null
    };
  }

  function bind() {
    // Any change to a settings control counts as an unsaved edit until the
    // host confirms it, so a state push cannot overwrite it.
    ["backend", "device", "rate", "latency-on", "latency", "vbap", "layout"].forEach(function (id) {
      $(id).addEventListener("change", function () { edits = true; });
      $(id).addEventListener("input", function () { edits = true; });
    });

    $("backend").addEventListener("change", function () {
      applyBackendVisibility($("backend").value);
    });

    $("device").addEventListener("change", function () {
      applyBackendVisibility($("backend").value);
    });

    $("latency-on").addEventListener("change", function () {
      var on = $("latency-on").checked;
      $("latency").disabled = !on;
      $("latency-out").textContent = on ? $("latency").value + " ms" : "orender default";
    });

    $("latency").addEventListener("input", function () {
      $("latency-out").textContent = $("latency").value + " ms";
    });

    $("render-toggle").addEventListener("click", function () {
      var rack = $("rack-render");
      var open = rack.dataset.open !== "true";
      rack.dataset.open = open ? "true" : "false";
      this.setAttribute("aria-expanded", open ? "true" : "false");
    });

    $("probe").addEventListener("click", function () {
      send("probe_driver");
      toast("Reading driver…", "warn");
    });

    $("driver-panel").addEventListener("click", function () {
      send("open_driver_panel");
      toast("Opening the driver's own window…", "warn");
    });

    $("start").addEventListener("click", function () { submitted = true; send("start", settingsFromForm()); });
    $("stop").addEventListener("click", function () { send("stop"); });
    $("apply").addEventListener("click", function () { submitted = true; send("apply", settingsFromForm()); });

    $("log-toggle").addEventListener("click", function () {
      var log = $("log");
      var show = log.hidden;
      log.hidden = !show;
      this.setAttribute("aria-expanded", show ? "true" : "false");
      this.textContent = show ? "Hide details" : "Details";
      if (show) send("read_engine_log");
    });
  }

  // ── Boot ─────────────────────────────────────────────────────────────────
  bind();
  send("get_state");
  // 500 ms poll, matching the host's repaint cadence.
  setInterval(function () { send("get_state"); }, 500);
})();
