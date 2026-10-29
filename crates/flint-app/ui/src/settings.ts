import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { open } from "@tauri-apps/plugin-dialog";
import { hotkeyName } from "./hotkey";
import { t, type Key, type Lang } from "./i18n";
import { esc } from "./render";

interface Settings {
  hotkey: string;
  launchAtStartup: boolean;
  theme: "system" | "dark" | "light";
  language: Lang;
  useWindowsRecent: boolean;
  extraFolders: string[];
  excludedFolders: string[];
  indexAppData: boolean;
  checkUpdates: boolean;
}
interface UpdateInfo {
  version: string;
  notes: string;
}
interface UpdateState {
  supported: boolean;
  version: string;
  found: UpdateInfo | null;
}
interface FieldError {
  field: string;
  message: string;
}
interface Volume {
  drive: string;
  state: string;
  entries: number;
}

const root = document.getElementById("root")!;
// root.innerHTML only receives markup built in this file; every setting,
// folder path, drive name and message goes through esc().
let s: Settings | null = null;
let volumes: Volume[] = [];
let errors: Record<string, string> = {};
let capturing = false;
let note = "";
/** Where the focus goes after the next render when its element is gone
 *  (a removed folder's button). */
let focusNext: string[] = [];
let upd: UpdateState | null = null;
/** The outcome of "Check now" / "Install", shown in the Updates card. */
let updNote = "";
let updBusy = false;

const L = (k: Key, ...a: string[]) => t(s?.language ?? "en", k, ...a);
const X = '<svg viewBox="0 0 16 16" aria-hidden="true"><path d="M4 4l8 8M12 4l-8 8"/></svg>';
const PLUS = '<svg viewBox="0 0 16 16" aria-hidden="true"><path d="M8 3v10M3 8h10"/></svg>';

const err = (field: string) => (errors[field] ? `<p class="err" role="alert">${esc(errors[field])}</p>` : "");

function sw(id: string, on: boolean, label: Key) {
  return `<button class="sw" id="${id}" role="switch" aria-checked="${on}" aria-label="${esc(L(label))}"></button>`;
}

function seg<T extends string>(id: string, label: Key, value: T, options: [T, string][]) {
  return (
    `<div class="seg" role="radiogroup" id="${id}" aria-label="${esc(L(label))}">` +
    options
      .map(([v, text]) => `<button role="radio" id="${id}-${v}" data-v="${v}" aria-checked="${v === value}" tabindex="${v === value ? 0 : -1}">${esc(text)}</button>`)
      .join("") +
    `</div>`
  );
}

function folders(id: string, list: string[]) {
  const items = list.length
    ? list.map((f, i) => `<li><span title="${esc(f)}">${esc(f)}</span><button class="x" id="rm-${id}-${i}" data-rm="${id}:${i}" aria-label="${esc(L("remove", f))}">${X}</button></li>`).join("")
    : `<li class="none">${esc(L("none"))}</li>`;
  return `<ul class="fl">${items}</ul><button class="btn" id="add-${id}" data-add="${id}">${PLUS}${esc(L("addFolder"))}</button>`;
}

function driveState(v: Volume) {
  const k: Key = v.state === "live" ? "driveLive" : v.state === "basic" ? "driveBasic" : "driveScanning";
  const nf = new Intl.NumberFormat(s?.language === "tr" ? "tr-TR" : "en-US");
  return `<div class="dv"><b>${esc(v.drive)}</b><span class="st ${v.state === "live" ? "live" : ""}">${esc(L(k))}</span><span class="n">${esc(L("driveEntries", nf.format(v.entries)))}</span></div>`;
}

function updatesCard() {
  if (!s || !upd) return "";
  const head =
    `<h2>${esc(L("secUpdates"))}</h2><div class="card">` +
    `<div class="row"><div class="lb"><b>${esc(L("version", upd.version))}</b></div></div>`;
  if (!upd.supported) return head + `<div class="row"><div class="lb"><small>${esc(L("updatesOff"))}</small></div></div></div>`;
  const status = updNote || (upd.found ? L("updateFound", upd.found.version) : "");
  const install = upd.found ? `<button class="btn" id="install-update"${updBusy ? " disabled" : ""}>${esc(L("installUpdate"))}</button>` : "";
  return (
    head +
    `<div class="row"><div class="lb"><b>${esc(L("autoUpdate"))}</b><small>${esc(L("autoUpdateHint"))}</small></div>${sw("updates", s.checkUpdates, "autoUpdate")}</div>` +
    `<div class="row"><div class="lb">${status ? `<span class="ok" role="status">${esc(status)}</span>` : ""}</div>` +
    `${install}<button class="btn" id="check-update"${updBusy ? " disabled" : ""}>${esc(L("checkNow"))}</button></div></div>`
  );
}

function render() {
  if (!s) return;
  const focused = document.activeElement?.id;
  document.documentElement.lang = s.language;
  if (s.theme === "system") delete document.documentElement.dataset.theme;
  else document.documentElement.dataset.theme = s.theme;
  document.title = L("settingsTitle");
  const keys = capturing
    ? esc(L("hotkeyPress"))
    : s.hotkey
        .split("+")
        .map((k) => `<span class="k">${esc(k)}</span>`)
        .join("");
  root.innerHTML =
    `<h1>${esc(L("settingsTitle"))}</h1>` +
    `<h2>${esc(L("secGeneral"))}</h2><div class="card">` +
    `<div class="row"><div class="lb"><b id="hk-l">${esc(L("hotkey"))}</b><small>${esc(L("hotkeyHint"))}</small>${err("hotkey")}</div>` +
    `<button class="hk${capturing ? " cap" : ""}" id="hk" aria-labelledby="hk-l" aria-describedby="hk-v"><span id="hk-v">${keys}</span></button></div>` +
    `<div class="row"><div class="lb"><b>${esc(L("launchAtStartup"))}</b><small>${esc(L("launchHint"))}</small>${err("launchAtStartup")}</div>${sw("startup", s.launchAtStartup, "launchAtStartup")}</div>` +
    `<div class="row"><div class="lb"><b>${esc(L("theme"))}</b></div>${seg("theme", "theme", s.theme, [
      ["system", L("themeSystem")],
      ["dark", L("themeDark")],
      ["light", L("themeLight")],
    ])}</div>` +
    `<div class="row"><div class="lb"><b>${esc(L("language"))}</b></div>${seg("language", "language", s.language, [
      ["en", "English"],
      ["tr", "Türkçe"],
    ])}</div></div>` +
    `<h2>${esc(L("secIndex"))}</h2><div class="card">` +
    `<div class="row col"><div class="lb"><b>${esc(L("drives"))}</b></div><div>${volumes.map(driveState).join("") || esc(L("none"))}</div></div>` +
    `<div class="row col"><div class="lb"><b>${esc(L("excluded"))}</b><small>${esc(L("excludedHint"))}</small></div>${folders("excludedFolders", s.excludedFolders)}</div>` +
    `<div class="row col"><div class="lb"><b>${esc(L("extraFolders"))}</b><small>${esc(L("extraHint"))}</small></div>${folders("extraFolders", s.extraFolders)}</div>` +
    `<div class="row"><div class="lb"><b>${esc(L("appData"))}</b><small>${esc(L("appDataHint"))}</small></div>${sw("appdata", s.indexAppData, "appData")}</div></div>` +
    `<h2>${esc(L("secHistory"))}</h2><div class="card">` +
    `<div class="row"><div class="lb"><b>${esc(L("useRecent"))}</b><small>${esc(L("useRecentHint"))}</small></div>${sw("recent", s.useWindowsRecent, "useRecent")}</div>` +
    `<div class="row"><div class="lb">${note ? `<span class="ok" role="status">${esc(note)}</span>` : ""}</div><button class="btn" id="clear">${esc(L("clearHistory"))}</button></div></div>` +
    updatesCard() +
    err("save");
  // the focus stays where it was (every control has an id); a removed
  // folder's button hands it to its neighbour
  const target = [focused, ...focusNext].map((id) => id && document.getElementById(id)).find((el) => el);
  focusNext = [];
  if (target) target.focus();
}

/** Saves run one after another, each on the settings the one before it
 *  left: two quick changes both stay. */
let saving: Promise<void> = Promise.resolve();
function save(patch: Partial<Settings>) {
  saving = saving.then(() => saveNow(patch));
  return saving;
}

async function saveNow(patch: Partial<Settings>) {
  if (!s) return;
  errors = {};
  note = "";
  try {
    s = await invoke<Settings>("set_settings", { new: { ...s, ...patch } });
  } catch (e) {
    if (typeof e === "object" && e && "field" in e) {
      const fe = e as FieldError;
      errors[fe.field] = fe.field === "save" ? L("saveFailed", fe.message) : fe.message;
    } else {
      errors.save = L("saveFailed", String(e));
    }
    s = await invoke<Settings>("get_settings");
  }
  render();
}

root.addEventListener("click", async (e) => {
  if (!s) return;
  const el = (e.target as HTMLElement).closest<HTMLElement>("button");
  if (!el) return;
  if (el.id === "hk") {
    capturing = !capturing;
    errors = {};
    render();
  } else if (el.id === "startup") void save({ launchAtStartup: !s.launchAtStartup });
  else if (el.id === "recent") void save({ useWindowsRecent: !s.useWindowsRecent });
  else if (el.id === "appdata") void save({ indexAppData: !s.indexAppData });
  else if (el.id === "updates") void save({ checkUpdates: !s.checkUpdates });
  else if (el.id === "check-update" && upd) {
    updBusy = true;
    updNote = L("checking");
    render();
    try {
      upd.found = await invoke<UpdateInfo | null>("check_update");
      updNote = upd.found ? L("updateFound", upd.found.version) : L("upToDate");
    } catch (m) {
      updNote = L("updateFailed", String(m));
    }
    updBusy = false;
    render();
  } else if (el.id === "install-update") {
    updBusy = true;
    updNote = L("installing");
    render();
    try {
      await invoke("install_update"); // Flint exits while the installer runs
    } catch (m) {
      updNote = String(m);
      updBusy = false;
      render();
    }
  }
  else if (el.id === "clear") {
    await invoke("clear_history").then(
      () => (note = L("cleared")),
      (m) => (errors = { save: String(m) }),
    );
    render();
  } else if (el.dataset.v && el.parentElement?.id === "theme") void save({ theme: el.dataset.v as Settings["theme"] });
  else if (el.dataset.v && el.parentElement?.id === "language") void save({ language: el.dataset.v as Lang });
  else if (el.dataset.add) {
    const field = el.dataset.add as "extraFolders" | "excludedFolders";
    const picked = await open({ directory: true, multiple: false });
    if (typeof picked === "string" && !s[field].some((f) => f.toLowerCase() === picked.toLowerCase())) {
      void save({ [field]: [...s[field], picked] });
    }
  } else if (el.dataset.rm) {
    const [field, i] = el.dataset.rm.split(":") as ["extraFolders" | "excludedFolders", string];
    const n = Number(i);
    focusNext = [`rm-${field}-${n}`, `rm-${field}-${n - 1}`, `add-${field}`];
    void save({ [field]: s[field].filter((_, j) => j !== n) });
  }
});

document.addEventListener("keydown", (e) => {
  if (capturing) {
    // Tab leaves the capture and moves on, as anywhere else
    if (e.key === "Tab") {
      capturing = false;
      render();
      return;
    }
    e.preventDefault();
    if (e.key === "Escape") {
      capturing = false;
      render();
      return;
    }
    const name = hotkeyName(e);
    if (name === null) return;
    capturing = false;
    if (name === "") {
      errors = { hotkey: L("hotkeyBad") };
      render();
    } else void save({ hotkey: name });
    return;
  }
  // arrow keys move within a segmented control, like radio buttons
  const btn = e.target as HTMLElement;
  const group = btn.closest<HTMLElement>(".seg");
  if (group && (e.key === "ArrowLeft" || e.key === "ArrowRight")) {
    e.preventDefault();
    const opts = [...group.querySelectorAll<HTMLElement>("button")];
    const next = opts[(opts.indexOf(btn) + (e.key === "ArrowRight" ? 1 : -1) + opts.length) % opts.length];
    next.focus(); // the focus follows the choice through the re-render
    next.click();
  }
});

async function load() {
  s = await invoke<Settings>("get_settings");
  volumes = (await invoke<{ volumes: Volume[] }>("status").catch(() => null))?.volumes ?? [];
  upd = await invoke<UpdateState>("update_state").catch(() => null);
  render();
}
listen<Settings>("flint://settings", (e) => {
  s = e.payload;
  render();
});
// drive progress stays current while the window is open
window.setInterval(async () => {
  const st = await invoke<{ volumes: Volume[] }>("status").catch(() => null);
  if (st && !capturing && JSON.stringify(st.volumes) !== JSON.stringify(volumes)) {
    volumes = st.volumes;
    render();
  }
}, 3000);
void load();
