// The app's end-to-end gates, measured on the running app.
//
//   node crates/flint-app/tests/gate.mjs [port] [presses]
//
// Start Flint first with a DevTools port:
//   WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS=--remote-debugging-port=9333 flint.exe
// and wait until its index has entries. The script:
//   1. hotkey -> visible: presses Alt+Space through Windows (SendInput, by
//      gate-keys.ps1 next to this file) `presses` times (default 50); for
//      each, the time from the app's hotkey handler to the first frame
//      after the search field has focus (both wall-clock ms);
//   2. keystroke -> results painted: types each of the 13 gate queries one
//      character at a time; for each character, from the `input` event to
//      the frame after the results are drawn.
// It prints p50/p95 for both and exits 1 when a budget is missed:
// hotkey <= 50 ms p95, keystroke <= 16 ms + one frame (16.7 ms) p95.
// Nothing on disk is changed; the launcher is left hidden.

import { execFileSync } from "node:child_process";
import path from "node:path";
import { fileURLToPath } from "node:url";

const port = process.argv[2] ?? "9333";
const presses = Number(process.argv[3] ?? 50);
const here = path.dirname(fileURLToPath(import.meta.url));
const QUERIES = ["b", "bu", "ra", "butce", "rapor", "isik", "finans butce", "node", "rapro", "rabor taslka", "ext:pdf rapor", "d: sunum", "zzqxw"];
const BUDGET = { hotkey: 50, paint: 16 + 1000 / 60 };

const targets = await (await fetch(`http://127.0.0.1:${port}/json`)).json();
const page = targets.find((t) => t.type === "page" && t.url === "http://tauri.localhost/");
if (!page) {
  console.error("the launcher page was not found on port " + port);
  process.exit(2);
}
const ws = new WebSocket(page.webSocketDebuggerUrl);
await new Promise((ok, fail) => ((ws.onopen = ok), (ws.onerror = fail)));
let id = 0;
const waiting = new Map();
ws.onmessage = (m) => {
  const msg = JSON.parse(m.data);
  if (waiting.has(msg.id)) {
    waiting.get(msg.id)(msg);
    waiting.delete(msg.id);
  }
};
async function js(expression) {
  const n = ++id;
  ws.send(JSON.stringify({ id: n, method: "Runtime.evaluate", params: { expression, awaitPromise: true, returnByValue: true } }));
  const r = await new Promise((ok) => waiting.set(n, ok));
  if (r.result?.exceptionDetails) throw new Error(JSON.stringify(r.result.exceptionDetails));
  return r.result?.result?.value;
}
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const pct = (xs, k) => {
  const s = [...xs].sort((a, b) => a - b);
  return s[Math.min(s.length - 1, Math.ceil(k * s.length) - 1)];
};
const I = "window.__TAURI_INTERNALS__.invoke";

await js("window.__flintGate = { shown: [], paints: [], parts: [] }, 1");
const hide = () => js("document.dispatchEvent(new KeyboardEvent('keydown',{key:'Escape'})), 1");
const visible = () => js("document.hasFocus() && document.visibilityState === 'visible'");

// 1. hotkey -> visible with the field focused
const hot = [];
await hide();
await sleep(300);
for (let i = 0; i < presses; i++) {
  const before = await js("window.__flintGate.shown.length");
  // no console window: one would take the focus the gate is measuring
  execFileSync("powershell", ["-NoProfile", "-ExecutionPolicy", "Bypass", "-File", path.join(here, "gate-keys.ps1")], { windowsHide: true });
  let shownAt = null;
  for (let w = 0; w < 100 && shownAt === null; w++) {
    shownAt = await js(`window.__flintGate.shown.length > ${before} ? window.__flintGate.shown.at(-1) : null`);
    if (shownAt === null) await sleep(10);
  }
  const pressedAt = await js(`${I}('gate_hotkey_ms')`);
  if (shownAt === null) console.error(`press ${i + 1}: the launcher did not show`);
  else hot.push(shownAt - pressedAt);
  if (!(await visible())) console.error(`press ${i + 1}: shown but not focused`);
  await hide();
  await sleep(250);
}

// 2. keystroke -> results painted, the gate queries typed one character at a time
const paints = [];
const typed = [];
for (const q of QUERIES) {
  for (let n = 1; n <= q.length; n++) {
    const before = await js("window.__flintGate.paints.length");
    await js(`(() => { const i = document.getElementById('q'); i.value = ${JSON.stringify(q.slice(0, n))}; i.dispatchEvent(new Event('input')); return 1 })()`);
    let v = null;
    for (let w = 0; w < 200 && v === null; w++) {
      v = await js(`window.__flintGate.paints.length > ${before} ? window.__flintGate.paints.at(-1) : null`);
      if (v === null) await sleep(5);
    }
    if (v === null) console.error(`"${q.slice(0, n)}": no paint`);
    else {
      paints.push(v);
      typed.push(q.slice(0, n));
    }
  }
}
const parts = await js("window.__flintGate.parts");
await js("(() => { const i = document.getElementById('q'); i.value = ''; i.dispatchEvent(new Event('input')); delete window.__flintGate; return 1 })()");
await hide();

const source = await js(`${I}('status').then(s => s.state.kind)`);
const row = (name, xs, budget) => {
  const p95 = pct(xs, 0.95);
  const ok = xs.length > 0 && p95 <= budget;
  console.log(`${name}: n=${xs.length} p50=${pct(xs, 0.5)?.toFixed(1)} ms p95=${p95?.toFixed(1)} ms max=${Math.max(...xs).toFixed(1)} ms (budget p95 <= ${budget.toFixed(1)} ms) ${ok ? "PASS" : "MISS"}`);
  return ok;
};
console.log(`source: ${source}`);
const a = row("hotkey -> visible", hot, BUDGET.hotkey);
const b = row("keystroke -> painted", paints, BUDGET.paint);
// where a keystroke's time goes: the answer, drawing it, then the frame
const col = (k) => parts.map((p) => p[k]);
const slow = paints
  .map((ms, i) => [ms, typed[i], parts[i]])
  .sort((x, y) => y[0] - x[0])
  .slice(0, 5)
  .map(([ms, q, p]) => `"${q}" ${ms.toFixed(1)} (answer ${p[0].toFixed(1)}, engine ${p[2].toFixed(1)})`);
console.log(`  slowest: ${slow.join("; ")}`);
console.log(`  answer arrived p50=${pct(col(0), 0.5).toFixed(1)} p95=${pct(col(0), 0.95).toFixed(1)} ms (engine p95 ${pct(col(2), 0.95).toFixed(1)} ms); drawn p50=${pct(col(1), 0.5).toFixed(1)} p95=${pct(col(1), 0.95).toFixed(1)} ms`);
ws.close();
process.exit(a && b ? 0 : 1);
