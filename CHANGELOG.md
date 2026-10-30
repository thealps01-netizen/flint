# Changelog

All notable changes to Flint are listed here. Versions follow
[Semantic Versioning](https://semver.org/).

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
