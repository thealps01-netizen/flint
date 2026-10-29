import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { t, type Key, type Lang } from "./i18n";
import { actionFor } from "./keys";
import { detailsHtml, esc, rowHtml, sizeText, type Details, type Hit } from "./render";
import { IconLoader } from "./icons";
import { Sequencer, move } from "./state";

type SourceState =
  | { kind: "service" }
  | { kind: "userMode" }
  | { kind: "serviceLost"; reason: string }
  | { kind: "serviceStarting" }
  | { kind: "versionMismatch"; detail: string };
interface Answer {
  seq: number;
  hits: Hit[];
  total: number;
  elapsedUs: number;
  source: SourceState;
  opened: boolean[];
  iconKeys: string[];
}
interface Volume {
  drive: string;
  state: string;
  entries: number;
}
interface StatusInfo {
  entries: number;
  volumes: Volume[];
  state: SourceState;
}

const LIMIT = 50;
const PAGE = 8;
let lang: Lang = "en";
const $ = <T extends HTMLElement>(id: string) => document.getElementById(id) as T;
const input = $<HTMLInputElement>("q");
const win = $("win");
const list = $("list");
const statusEl = $("status");
const banner = $("banner");
const detailsEl = $("details");
const detailBody = $("detail-body");

interface Settings {
  theme: "system" | "dark" | "light";
  language: Lang;
  hotkey: string;
}
let hotkeyName = "Alt+Space";

/** Theme and language from Settings; the page redraws in the new language. */
function applySettings(s: Settings) {
  if (s.theme === "system") delete document.documentElement.dataset.theme;
  else document.documentElement.dataset.theme = s.theme;
  lang = s.language;
  hotkeyName = s.hotkey;
  document.documentElement.lang = lang;
  for (const el of document.querySelectorAll<HTMLElement>("[data-t]")) el.textContent = t(lang, el.dataset.t as Key);
  input.placeholder = t(lang, "searchLabel");
  input.setAttribute("aria-label", t(lang, "searchLabel"));
  list.setAttribute("aria-label", t(lang, "results"));
  nf = new Intl.NumberFormat(lang === "tr" ? "tr-TR" : "en-US");
  nf1 = new Intl.NumberFormat(lang === "tr" ? "tr-TR" : "en-US", { minimumFractionDigits: 1, maximumFractionDigits: 1 });
  if (input.value) void search();
  else {
    renderBanner(info?.state);
    renderEmpty();
    renderStatus();
  }
}

// innerHTML below only ever receives markup built here, with every file
// name, path and message passed through esc() (render.test.ts checks it).
const seq = new Sequencer(Date.now());
let hits: Hit[] = [];
let opened: boolean[] = [];
let iconKeys: string[] = [];
let sel = -1;
let last: Answer | null = null;
let info: StatusInfo | null = null;
let notice: string | null = null;
let flash: string | null = null;
let flashTimer = 0;
const detailCache = new Map<string, Details | null>();
/** Typing pause after which sizes and dates are asked for. */
const IDLE_MS = 150;
let detailTimer = 0;
let panelTimer = 0;
let lastRender = 0;
/** Results changed within the typing pause: new icons and sizes wait. */
const typing = () => performance.now() - lastRender < IDLE_MS;
let nf = new Intl.NumberFormat("en-US");
let nf1 = new Intl.NumberFormat("en-US", { minimumFractionDigits: 1, maximumFractionDigits: 1 });

// ---- status line and banners -------------------------------------------

function renderStatus() {
  if (flash) return void (statusEl.innerHTML = `<span class="ok">${esc(flash)}</span>`);
  const parts: string[] = notice ? [notice] : [];
  if (input.value && last) {
    parts.push(last.total === 1 ? t(lang, "oneMatch") : t(lang, "matches", nf.format(last.total)));
    parts.push(t(lang, "ms", nf1.format(last.elapsedUs / 1000)));
  } else if (info) {
    parts.push(t(lang, "indexed", nf.format(info.entries)));
    const scanning = info.volumes.filter((v) => v.state === "scanning").map((v) => v.drive);
    if (scanning.length) parts.push(`${scanning.join(" ")} ${t(lang, "scanning")}`);
  } else {
    parts.push(t(lang, "starting"));
  }
  statusEl.textContent = parts.join(" · ");
}

function showFlash(text: string, ms = 1600) {
  flash = text;
  renderStatus();
  clearTimeout(flashTimer);
  flashTimer = window.setTimeout(() => {
    flash = null;
    renderStatus();
  }, ms);
}

const LOGS = "%LOCALAPPDATA%\\Flint\\logs";
const SHIELD = '<svg class="i" aria-hidden="true"><use href="#i-shield"/></svg>';

function renderBanner(state: SourceState | undefined) {
  let html = "";
  let warn = false;
  switch (state?.kind) {
    case "userMode":
      html = `<span class="msg">${esc(t(lang, "basic"))}</span><button type="button" id="enable-full">${esc(t(lang, "enableFull"))}</button>`;
      break;
    case "serviceLost":
      warn = true;
      html = `<span class="msg">${esc(t(lang, "lost"))}<span class="sub">${esc(t(lang, "lostRecovery", LOGS))}</span></span>`;
      break;
    case "serviceStarting":
      html = `<span class="msg">${esc(t(lang, "startingService"))}</span>`;
      break;
    case "versionMismatch":
      warn = true;
      html = `<span class="msg">${esc(t(lang, "restart"))}</span>`;
      break;
  }
  banner.hidden = !html;
  banner.classList.toggle("warn", warn);
  banner.innerHTML = html ? SHIELD + html : "";
}

// ---- list ---------------------------------------------------------------

const icons = new IconLoader();

function fillRow(i: number, d: Details | null) {
  const row = document.getElementById(`opt-${i}`);
  const cell = row?.querySelector<HTMLElement>("[data-size]");
  if (cell && d) cell.textContent = sizeText(d, lang);
}

async function detailsFor(h: Hit): Promise<Details | null> {
  if (detailCache.has(h.path)) return detailCache.get(h.path)!;
  const d = await invoke<Details | null>("details", { path: h.path }).catch(() => null);
  detailCache.set(h.path, d);
  return d;
}

function renderDetails() {
  const h = hits[sel];
  detailsEl.hidden = !h;
  list.classList.toggle("full", !h);
  if (!h) return;
  const show = (d: Details | null) => {
    detailBody.innerHTML = detailsHtml(h, d, opened[sel] ?? false, lang, iconKeys[sel]);
    icons.load(detailBody, !typing());
  };
  show(detailCache.get(h.path) ?? null);
  const want = h.path;
  clearTimeout(panelTimer);
  panelTimer = window.setTimeout(() => {
    if (hits[sel]?.path !== want) return;
    icons.load(detailBody, true);
    if (detailCache.has(want)) return;
    void detailsFor(h).then((d) => {
      if (hits[sel]?.path === want) show(d);
    });
  }, IDLE_MS);
}

function select(i: number, scroll = true) {
  const prev = document.getElementById(`opt-${sel}`);
  prev?.classList.remove("s");
  prev?.setAttribute("aria-selected", "false");
  sel = i;
  const row = document.getElementById(`opt-${sel}`);
  row?.classList.add("s");
  row?.setAttribute("aria-selected", "true");
  if (row) input.setAttribute("aria-activedescendant", row.id);
  else input.removeAttribute("aria-activedescendant");
  if (scroll) row?.scrollIntoView({ block: "nearest" });
  renderDetails();
}

function renderEmpty() {
  hits = [];
  sel = -1;
  input.setAttribute("aria-expanded", "false");
  input.removeAttribute("aria-activedescendant");
  detailsEl.hidden = true;
  list.classList.add("full");
  const welcomed = localStorage.getItem("flint.welcomed") === "1";
  const scanning = info?.volumes.filter((v) => v.state === "scanning") ?? [];
  // the first-run panel: later starts show progress in the status line only
  if (!welcomed) {
    const drives = (info?.volumes ?? [])
      .map(
        (v) =>
          `<li class="${v.state === "scanning" ? "" : "done"}"><span>${esc(v.drive)}</span><span class="bar"><i></i></span><span>${esc(
            v.state === "scanning" ? t(lang, "scanning") : v.state === "basic" ? t(lang, "basicState") : t(lang, "live"),
          )} · ${nf.format(v.entries)}</span></li>`,
      )
      .join("");
    list.innerHTML = `<div class="msgs"><h2>${esc(t(lang, "welcome"))}</h2><p>${esc(
      t(lang, "welcomeHotkey", hotkeyName),
    )}</p>${scanning.length ? `<p>${esc(t(lang, "welcomeIndexing"))}</p>` : ""}<ul class="drives">${drives}</ul></div>`;
    if (!scanning.length && info && document.hasFocus()) localStorage.setItem("flint.welcomed", "1");
  } else {
    list.innerHTML = `<div class="hint">${esc(t(lang, "emptyHint"))}</div>`;
  }
}

/** A shorter query to offer when nothing matched: the last word dropped. */
function shorter(q: string): string | null {
  const words = q.trim().split(/\s+/);
  return words.length > 1 ? words.slice(0, -1).join(" ") : null;
}

function renderNoResults(q: string) {
  hits = [];
  sel = -1;
  detailsEl.hidden = true;
  list.classList.add("full");
  input.setAttribute("aria-expanded", "false");
  input.removeAttribute("aria-activedescendant");
  const s = shorter(q);
  const scope = (info?.volumes ?? []).map((v) => v.drive).join(", ");
  list.innerHTML =
    `<div class="msgs"><h2>${esc(t(lang, "noResults", q))}</h2>` +
    (scope ? `<p>${esc(t(lang, "searched", scope))}</p>` : "") +
    (s ? `<button class="try" type="button" data-try="${esc(s)}"><svg class="i"><use href="#i-search"/></svg>${esc(t(lang, "tryShorter", s))}</button>` : "") +
    `</div>`;
}

function renderHits(a: Answer) {
  const keep = hits[sel]?.path;
  hits = a.hits;
  opened = a.opened ?? [];
  iconKeys = a.iconKeys ?? [];
  input.setAttribute("aria-expanded", String(hits.length > 0));
  list.innerHTML = hits.map((h, i) => rowHtml(h, i, false, opened[i] ?? false, lang, iconKeys[i])).join("");
  lastRender = performance.now();
  // icons seen before show at once; new ones wait for a typing pause
  icons.load(list, false);
  // the selection follows the file it was on when it is still listed
  const again = keep ? hits.findIndex((h) => h.path === keep) : -1;
  sel = -1;
  select(again >= 0 ? again : 0, again < 0);
  if (again < 0) list.scrollTop = 0;
  // sizes for the visible rows once typing pauses, so they never queue
  // in front of the next keystroke's search
  clearTimeout(detailTimer);
  const shown = hits;
  detailTimer = window.setTimeout(() => {
    if (hits === shown) icons.load(list, true);
    shown.slice(0, 14).forEach((h, i) => void detailsFor(h).then((d) => hits[i] === h && fillRow(i, d)));
  }, IDLE_MS);
}

// ---- search -------------------------------------------------------------

/** The newest search still running; actions wait for it. */
let pending: Promise<void> = Promise.resolve();

function search(): Promise<void> {
  pending = runSearch();
  return pending;
}

async function runSearch() {
  const q = input.value;
  const n = seq.next();
  if (!q.trim()) {
    seq.clear();
    last = null;
    renderEmpty();
    renderStatus();
    return;
  }
  let a: Answer | null;
  try {
    a = await invoke<Answer | null>("search", { seq: n, text: q, limit: LIMIT });
  } catch {
    a = null; // still starting: the next keystroke or status tick retries
    if (seq.accept(n)) {
      statusEl.textContent = t(lang, "starting");
    }
    return;
  }
  // an older answer arriving late, or one for text no longer in the field
  if (!a || input.value !== q || !seq.accept(n)) return;
  const answered = performance.now();
  last = a;
  renderBanner(a.source);
  if (a.hits.length) renderHits(a);
  else renderNoResults(q);
  renderStatus();
  markPaint(answered, a.elapsedUs / 1000);
}

async function refreshStatus() {
  info = await invoke<StatusInfo>("status").catch(() => null);
  if (!input.value) {
    renderBanner(info?.state);
    renderEmpty();
  }
  renderStatus();
}

// ---- actions ------------------------------------------------------------

let acting = false;

async function act(cmd: "open" | "reveal") {
  if (acting) return; // a double Enter or click opens once
  acting = true;
  try {
    await pending;
    const h = hits[sel];
    if (!h) return;
    try {
      await invoke(cmd, { path: h.path });
      void getCurrentWindow().hide();
    } catch (e) {
      statusEl.textContent = String(e);
      detailCache.delete(h.path);
    }
  } finally {
    acting = false;
  }
}

async function copy() {
  const h = hits[sel];
  if (!h) return;
  try {
    await invoke("copy_path", { path: h.path });
    showFlash(t(lang, "copied"));
  } catch (e) {
    statusEl.textContent = String(e);
  }
}

input.addEventListener("input", () => {
  if (gate()) typedAt = performance.now();
  void search();
});

document.addEventListener("keydown", (e) => {
  const textSelected = input.selectionStart !== input.selectionEnd;
  const a = actionFor(e, textSelected);
  if (!a) return;
  e.preventDefault();
  if (e.repeat && (a === "open" || a === "reveal" || a === "copy")) return; // a held key acts once
  switch (a) {
    case "hide":
      void getCurrentWindow().hide();
      break;
    case "down":
    case "up":
    case "pageDown":
    case "pageUp": {
      const d = { down: 1, up: -1, pageDown: PAGE, pageUp: -PAGE }[a];
      if (hits.length) select(move(sel, d, hits.length));
      break;
    }
    case "open":
    case "reveal":
      void act(a);
      break;
    case "copy":
      void copy();
      break;
    case "settings":
      void invoke("open_settings");
      break;
    case "block":
      break;
  }
});
// the WebView's menu offers Reload and Print; only the text field keeps its menu
document.addEventListener("contextmenu", (e) => {
  if (e.target !== input) e.preventDefault();
});

list.addEventListener("mousedown", (e) => e.preventDefault()); // keep the caret in the field
list.addEventListener("click", (e) => {
  const target = e.target as HTMLElement;
  const tryBtn = target.closest<HTMLElement>("[data-try]");
  if (tryBtn) {
    input.value = tryBtn.dataset.try!;
    void search();
    return;
  }
  const row = target.closest<HTMLElement>(".r");
  if (row) select(Number(row.dataset.i), false);
});
list.addEventListener("dblclick", (e) => {
  if ((e.target as HTMLElement).closest(".r")) void act("open");
});
$("act-open").addEventListener("click", () => void act("open"));
$("act-reveal").addEventListener("click", () => void act("reveal"));
$("act-copy").addEventListener("click", () => void copy());
for (const b of detailsEl.querySelectorAll("button")) b.addEventListener("mousedown", (e) => e.preventDefault());

// "Enable full indexing": one administrator prompt; the window hides while
// Windows asks, and shows the outcome when it comes back
banner.addEventListener("click", async (e) => {
  const b = (e.target as HTMLElement).closest<HTMLButtonElement>("#enable-full");
  if (!b) return;
  b.disabled = true;
  try {
    const r = await invoke<"done" | "declined">("enable_full_indexing");
    showFlash(t(lang, r === "done" ? "fullEnabled" : "notEnabled"), 4000);
    void refreshStatus();
  } catch (err) {
    showFlash(String(err), 8000);
  } finally {
    b.disabled = false;
  }
});

// ---- gate timing (tests/gate.mjs sets window.__flintGate) -------------

interface Gate {
  shown: number[];
  paints: number[];
  /** Per paint: [answer arrived, drawn, engine] ms after the keystroke. */
  parts: number[][];
}
const gate = () => (window as unknown as { __flintGate?: Gate }).__flintGate;
let typedAt = 0;
/** After a render: the next frame is when the user sees it. */
function markPaint(answered: number, engineMs: number) {
  const g = gate();
  if (!g || !typedAt) return;
  const t0 = typedAt;
  const drawn = performance.now();
  typedAt = 0;
  requestAnimationFrame(() => {
    g.parts.push([answered - t0, drawn - t0, engineMs]);
    g.paints.push(performance.now() - t0);
  });
}

// ---- window -------------------------------------------------------------

let ticker = 0;
listen<boolean>("flint://shown", (e) => {
  win.classList.toggle("opaque", !e.payload);
  win.classList.remove("enter");
  void win.offsetWidth; // restart the entrance animation
  win.classList.add("enter");
  input.focus();
  input.select();
  const g = gate();
  if (g) requestAnimationFrame(() => g.shown.push(Date.now()));
  detailCache.clear(); // sizes and dates may have changed while hidden
  void refreshStatus();
  if (input.value) void search();
  clearInterval(ticker);
  // while visible, the indexing progress and source state stay current
  ticker = window.setInterval(() => {
    if (document.hasFocus()) void refreshStatus();
    else clearInterval(ticker);
  }, 2000);
});

const showNotice = (text: string | null) => {
  notice = text;
  renderStatus();
};
listen<string | null>("flint://notice", (e) => showNotice(e.payload));
// a newer version: said in the status line unless a warning is there
listen<{ version: string }>("flint://update", (e) => {
  if (!notice) showNotice(t(lang, "updateAvailable", e.payload.version));
});
invoke<string | null>("launcher_notice").then(showNotice);
listen<Settings>("flint://settings", (e) => applySettings(e.payload));
renderEmpty();
void invoke<Settings>("get_settings").then(applySettings);
void refreshStatus();
