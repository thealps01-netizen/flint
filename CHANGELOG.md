# Changelog

All notable changes to Flint are listed here. Versions follow
[Semantic Versioning](https://semver.org/).

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
