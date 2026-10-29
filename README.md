# Flint

Flint finds a file on your Windows PC by part of its name and opens it.
Press **Alt+Space**, type a few letters, press **Enter**.

- **Ranks what you mean first.** Files you open often and recently rise
  to the top; Windows' own list of recent files gives it a head start.
- **Forgives typing.** `rapro` finds `rapor`, `butce` finds `Bütçe`
  (Turkish and accented letters, and one typo in a word of four letters
  or more).
- **Searches every local drive.** With full indexing, Flint reads the
  NTFS file table and follows its change journal, so new, renamed and
  deleted files show up within moments.
- **Stays out of the way.** The window appears when you press the hotkey
  and hides when you press Esc or click elsewhere.

Flint searches file and folder **names**, not the text inside files.

## Install

1. Download `Flint_<version>_x64-setup.exe` from the
   [latest release](../../releases/latest).
2. Run it. Windows asks for administrator permission once: Flint installs
   for all users of the PC.
3. Windows SmartScreen may say "Windows protected your PC", because this
   first release is not code-signed. Choose **More info → Run anyway**.
4. The installer asks whether to turn on **full indexing**:
   - **Yes (recommended):** a small background service (`FlintIndexer`)
     indexes every NTFS drive and keeps the index up to date.
   - **No:** Flint indexes only your own folders (basic mode). You can
     turn full indexing on later from Flint.

Needs Windows 10 version 22H2 or Windows 11, 64-bit. The installer adds
Microsoft's WebView2 runtime if it is missing (it comes with Windows 11).

Uninstall from **Settings → Apps**. The service and its index are
removed; tick "Delete the application data" to also remove your Flint
settings and history.

## Use

| Key | Does |
|---|---|
| Alt+Space | Show or hide Flint (change it in Settings) |
| ↑ ↓, Page Up/Down | Move through the results |
| Enter | Open |
| Ctrl+Enter | Show in folder |
| Ctrl+C | Copy the path |
| Ctrl+, | Settings |
| Esc | Hide |

Separate words narrow the search, and each word may match a folder in the
path: `finans butce` finds `…\Finans\…\Bütçe_2026.xlsx`.

## Privacy

- The index and your open history stay on your PC. Flint sends nothing
  about your files anywhere.
- The service indexes every NTFS drive, but never shows a signed-in
  user another user's profile folder or recycle bin, system-only folders
  such as `System Volume Information`, or folders an ordinary user may
  not open. A single file with its own stricter permissions inside a
  folder you can open is not checked one by one in this version.
- Drives that are not NTFS (FAT32, exFAT, ReFS) are walked by Flint
  itself, as you, so Windows' own permissions apply.
- **Update check:** a minute after Flint starts, and then once a day,
  Flint asks this project's release page whether a newer version exists. Turn it off in **Settings → Updates**.
  Updates are signed; Flint installs only an installer signed with the
  project's key, and only for the version it announces.

## Measured speed

All numbers below were measured on one machine (AMD Ryzen 9 9900X,
31 GiB RAM, NVMe SSD, Windows 11 Pro) and are documented with their
method, so you can repeat them:

| What | Result (p95) | Budget | Details |
|---|---|---|---|
| Hotkey to visible window | 24–39 ms | ≤ 50 ms | [app-baseline.md](docs/benchmarks/app-baseline.md) |
| Keystroke to results on screen | 11.4–20.1 ms | ≤ 32.7 ms | [app-baseline.md](docs/benchmarks/app-baseline.md) |
| Search on a 2 M-name test corpus | 6.9–8.0 ms | ≤ 8 ms | [core-baseline.md](docs/benchmarks/core-baseline.md) |
| Search answered by the service, 3.5 M entries on C: | 12.7 ms | ≤ 16 ms | [indexer-baseline.md](docs/benchmarks/indexer-baseline.md) |

The keystroke budget is 16 ms of Flint's own work plus one 60 Hz frame
for the screen to show it (see app-baseline.md).

Two targets are **not** met yet and are listed with their causes in
[indexer-baseline.md](docs/benchmarks/indexer-baseline.md): the service's
memory (about 660 MB private for 3.5 M entries; target 200 MB) and the
cost of folder permission checks during the first index.

No comparison with other search tools is published yet. One will appear
here only together with its measurement.

## Build from source

Needs Rust (stable, from `rust-toolchain.toml`), Node.js 22.12 or later
(or 20.19) and Windows.

```
npm --prefix crates/flint-app/ui ci
cargo test --workspace --release
npm --prefix crates/flint-app/ui test
npm --prefix crates/flint-app/ui run release
```

The last command builds the installer
`target/release/bundle/nsis/Flint_<version>_x64-setup.exe`. A build made
this way has no update address and never checks for updates.

| Folder | What it is |
|---|---|
| `crates/flint-core` | Name matching and ranking |
| `crates/flint-indexer` | The index: NTFS reader, change journal, folder walk, the service |
| `crates/flint-app` | The launcher (Tauri) and its installer |
| `crates/flint-bench` | The search benchmark |
| `docs/benchmarks` | Measured results and how to repeat them |

## Licence

MIT, see [LICENSE](LICENSE).
