/* DEV ONLY — never shipped, never referenced by index.html.
 *
 * Regression check for the unsaved-edit lock in ui/app.js.
 *
 * The host pushes a full state snapshot every 500 ms (design.md §9.3). If the
 * page writes every control back from it, a choice the user has made but not
 * yet submitted is silently reverted within half a second — picking `asio`
 * snapped back to `file` before Start/Apply could be pressed, which is what
 * "changes do not stick / cannot switch to asio" looked like.
 *
 * Loads the preview copy built by tools/preview.ps1 (so it exercises exactly
 * the shipped app.js, plus dev-bridge.js as the host stand-in) and asserts:
 *   1. a push still applies when nothing is being edited
 *   2. an unsubmitted backend choice survives a stale push
 *   3. an unsubmitted sample rate survives a stale push
 *   4. after a successful submit the host value is authoritative again
 *   5. a REFUSED submit keeps the lock (otherwise the refusal reason is shown
 *      next to a form that has already been reset)
 *
 * Needs jsdom. Run:
 *   pwsh -File tools/preview.ps1
 *   bun add jsdom            # or: npm i jsdom
 *   bun tools/check-ui.mjs
 */
import { JSDOM } from "jsdom";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = dirname(dirname(fileURLToPath(import.meta.url)));
const preview = join(root, "ui", ".preview");

const read = (name) => readFileSync(join(preview, name), "utf8");

// jsdom will not fetch from the fake app.localhost origin the panel serves, so
// the scripts are evaluated directly — the same bytes the wry host embeds.
const html = read("index.html").replace(/<script src="[^"]+"><\/script>/g, "");
const dom = new JSDOM(html, {
  url: "http://app.localhost/index.html",
  runScripts: "dangerously",
  pretendToBeVisual: true,
});
const { window } = dom;
const doc = window.document;

window.eval(read("dev-bridge.js"));
window.eval(read("app.js"));

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
await sleep(200); // let the stub boot and push once

function push(backend, rate) {
  window.violaPanel.applyState(
    JSON.stringify({
      settings: {
        speaker_layout: null,
        enable_vbap: true,
        output_backend: backend,
        output_device: null,
        output_sample_rate: rate,
        latency_target_ms: null,
      },
      devices: [{ key_name: "ASIO4ALL v2", description: "ASIO4ALL v2", clsid: "{B1}" }],
      engine_running: false,
      engine_pid: null,
      engine_error: null,
      driver: null,
      driver_error: null,
      default_endpoint: "扬声器 (TANCHJIM BUNNY DSP)",
      default_endpoint_error: null,
      config_path: "C:/x/panel.yaml",
    })
  );
}

const change = (el) => el.dispatchEvent(new window.Event("change", { bubbles: true }));

const results = [];
function check(name, actual, expected) {
  const ok = actual === expected;
  results.push(ok);
  console.log(`${ok ? "PASS" : "FAIL"}  ${name}: got ${JSON.stringify(actual)}, want ${JSON.stringify(expected)}`);
}

const backend = doc.getElementById("backend");
const rate = doc.getElementById("rate");

push("file", 48000);
check("idle push applies the host value", backend.value, "file");

backend.value = "asio";
change(backend);
push("file", 48000);
check("unsubmitted choice survives a stale push", backend.value, "asio");

rate.value = "96000";
change(rate);
push("file", 48000);
check("unsubmitted rate survives a stale push", rate.value, "96000");

// The stub auto-replies `ok` to every command; drop it so the host's answer is
// the only one these two checks see.
window.__violaStub = function () {};
const start = doc.getElementById("start");

start.dispatchEvent(new window.Event("click", { bubbles: true }));
window.violaPanel.applyResult(JSON.stringify({ ok: true, message: "Engine started" }));
push("asio", 96000);
check("after a successful submit the host value wins", backend.value, "asio");

backend.value = "file";
change(backend);
start.dispatchEvent(new window.Event("click", { bubbles: true }));
window.violaPanel.applyResult(JSON.stringify({ ok: false, error: "nope" }));
push("asio", 96000);
check("a refused submit keeps the edit lock", backend.value, "file");

console.log(results.every(Boolean) ? "\nALL PASS" : "\nSOME FAILED");
process.exit(results.every(Boolean) ? 0 : 1);
