# Changelog

All notable changes to Flint are listed here. Versions follow
[Semantic Versioning](https://semver.org/).

## 0.1.3 — 2026-10-30

- The index commits less memory once files change after a start. Its
  columns grew by doubling, and after a start from the saved index every
  column is exactly full, so the first new file doubled them all at once:
  the Full indexing service then committed 383–392 MB of private memory
  against 310–312 MB after a first index, with the same 1.45 M entries.
  Columns now grow by an eighth. In a 1.45 M-entry test index the private
  memory after a start and a thousand new files went from 308 MB to
  189 MB; after a first index and one new file, from 252 MB to 184 MB.
  The memory actually in use (working set) was the same before and after.
  Adding entries one by one without reserving room first (a folder walk)
  takes about 1.5 times as long, about 10 ms for Basic mode's 151 k
  entries. Measured after the update on the same PC, the Full indexing
  service committed 243–246 MB after a first index and after a restart
  alike (see
  [everything-comparison.md](docs/benchmarks/everything-comparison.md)).

## 0.1.2 — 2026-10-30

- Full indexing leaves out the hidden app data folders too, for every
  account on the PC; the Start menu shortcuts kept there stay. On the
  measured disk (C:, read without administrator rights and built the way
  the service builds it) the index went from 4.13 M to 1.70 M entries
  and its memory from 434 MB to 178 MB (the whole process: 486 MB to
  227 MB). The service rebuilds its index once after the update.
- **Settings → Search app data folders** also works with full indexing:
  Flint then indexes your own app data folders beside the service. A
  folder added under **Extra folders** that lies inside an app data
  folder is indexed in both modes.
- **Settings → Indexing → Full indexing** turns full indexing off as
  well as on (one administrator prompt): the service and its index are
  removed and Flint goes on in Basic mode. Before, the only way back was
  to reinstall.

## 0.1.1 — 2026-10-30

- Basic mode leaves out the hidden app data folders (`AppData`): program
  caches and settings that made up 94% of one measured profile. On that
  profile the index went from 2.57 M to 152 k entries, memory from about
  350 MB to 28 MB, and the first index from about 45 s to 7 s. Start menu
  shortcuts stay; **Settings → Search app data folders** brings the rest
  back.

## 0.1.0 — 2026-10-30

First version (built, not published).

- Launcher: global hotkey (Alt+Space by default), results as you type,
  real Windows icons, a details pane, dark and light themes, English and
  Turkish.
- Search: file and folder names on every local drive; typo and
  Turkish/accent tolerant; files you open often and recently rank higher.
- Full indexing: a background service reads the NTFS file table and
  follows the change journal; without it, Flint indexes your own folders.
- Settings: hotkey, start with Windows, theme, language, excluded and
  extra folders, history, updates.
- Installer for Windows 10 22H2 and Windows 11 (x64), signed updates.
