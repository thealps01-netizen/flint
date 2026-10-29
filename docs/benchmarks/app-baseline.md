# App latency baseline

What this file is: the recorded result of the launcher's two end-to-end
gates, measured on the running app. Later plans compare against it; a
change that pushes either p95 over its budget fails the gate
(`gate.mjs` exits 1).

| Gate | What is timed | Budget (p95) |
|---|---|---|
| Hotkey → visible | from the app's hotkey handler to the first frame after the search field has focus | ≤ 50 ms |
| Keystroke → painted | from the field's `input` event to the frame after the results are drawn | ≤ 16 ms + one frame (32.7 ms) |

## How to reproduce

1. Build: `npm --prefix crates/flint-app/ui run build` and
   `cargo build -p flint-app --release`.
2. Start the app with a DevTools port (and, on a machine whose other
   drives must not be walked, `FLINT_OTHER_DRIVES=""`):
   `WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS=--remote-debugging-port=9333 target/release/flint.exe`
3. Wait until indexing is done (`status` shows no drive "scanning").
4. `node crates/flint-app/tests/gate.mjs 9333 50`

The script presses Alt+Space 50 times through Windows (`gate-keys.ps1`,
started without a console window: a visible console takes the focus the
gate measures), then types the 13 gate queries of the core benchmark one
character at a time (78 keystrokes) and waits for each paint. It also
prints where a keystroke's time goes (answer arrived, drawn, engine) and
the five slowest keystrokes. Run it on an otherwise idle machine, without
using the mouse or keyboard meanwhile.

The plan named a Rust test (`tests/gate.rs`); the gate is a Node script
instead because it drives the WebView over DevTools, which Node 22 does
with no extra dependency.

## Machine

- CPU: AMD Ryzen 9 9900X, 12 cores / 24 threads
- RAM: 31.1 GB
- OS: Windows 11 Pro, build 26300; C: NTFS, D: ReFS (not indexed in these runs)
- History empty (no boosts), Windows recent-file tracking off

## Result (2026-10-29, the commit that adds this file, 3 consecutive runs per mode, all PASS)

### Basic mode (no service; the user folder, 2.54 M entries, indexed in the app)

| Run | Hotkey p50 / p95 / max | Keystroke p50 / p95 / max |
|---|---|---|
| 1 | 22 / 33 / 38 ms | 8.7 / 13.5 / 85.5 ms |
| 2 | 21 / 24 / 26 ms | 8.2 / 12.2 / 69.8 ms |
| 3 | 21 / 33 / 37 ms | 7.8 / 11.4 / 70.9 ms |

Run 3 breakdown at p95: answer arrived 6.4 ms (engine 5.4 ms), drawn
11.0 ms. App working set about 300 MB (it holds the index).

### Full indexing (FlintIndexer service, C:, 3.42 M entries)

| Run | Hotkey p50 / p95 / max | Keystroke p50 / p95 / max |
|---|---|---|
| 1 | 22 / 39 / 40 ms | 10.3 / 19.4 / 29.0 ms |
| 2 | 21 / 24 / 27 ms | 9.2 / 20.1 / 29.0 ms |
| 3 | 22 / 32 / 37 ms | 10.1 / 17.6 / 25.0 ms |

Run 3 breakdown at p95: answer arrived 11.2 ms (engine 9.6 ms), drawn
16.1 ms. App working set 34–48 MB; service working set 415 MB right
after its first scan.

The service was installed through the app's own "Enable full indexing"
button (one UAC prompt, accepted by the owner) and removed with
`flint-indexer uninstall-service` after the runs.

In these six runs, all 300 presses showed the launcher with the field
focused.

## What was changed to pass, and why

- **Sizes and dates wait for a typing pause (150 ms).** Fetching them for
  14 rows right after every render queued ~14 IPC calls in front of the
  next keystroke's search: Basic mode p95 was 53–74 ms before, 24–28 ms
  after.
- **Icons are asked for once per icon, not once per row.** The app sends
  each hit's icon key (type, own path for .exe/.lnk/.ico/.url, or folder);
  a key seen before is shown at once from the address that loaded it (the
  page's image cache answers), a new key is asked for after the typing
  pause. With every row asking for its own icon, the icon traffic delayed
  answers by up to 95 ms in service mode (answer 99 ms, engine 4 ms); the
  same run with icons switched off passed at 20.6 ms p95. Service p95 was
  37.9–57.0 ms before, 17.6–20.1 ms after; Basic mode p95 went from
  24–28 ms to 11.4–13.5 ms.

## Misses and open points

- **First one-letter query of a run.** "b" takes 45–58 ms in the engine
  the first time (85.5 ms painted at worst); later one-letter queries are
  fast. It does not move p95; it is the first query to look at if the cold
  first keystroke matters.
- **Carried over from indexer-baseline.md, still open:** the access check costs +86 %
  on top of search (budget +20 %), and the service's memory target of
  200 MB is missed (415 MB working set here; indexer-baseline.md recorded 662 MB at its
  peak).
- **A press can show the launcher without focus.** One earlier service run
  had 3 presses that did not show the launcher and 6 that showed it
  without focus (old icon code, the owner possibly at the machine); the
  300 presses above had none; the 150 presses after the review fixes
  (Basic mode, hotkey p95 27–45 ms, keystroke p95 12.3–13.0 ms, all
  PASS) had one (press 44 of the third run). The cause is not known:
  Windows may refuse to bring the window to the front. While it lasts,
  Esc and click-away do nothing until the hotkey is pressed again (which
  shows and focuses it). If it is seen again, the window's focus handling
  (`window::show`, `set_focus`) is the place to look.
