# Changelog

All notable changes to Flint are listed here. Versions follow
[Semantic Versioning](https://semver.org/).

## 0.1.8 — 2026-10-31

- Updating no longer fails while an AI assistant is connected to Flint
  (or `flint-indexer query` runs). Windows does not let a running
  program be overwritten, so the update could not replace
  `flint-indexer.exe` and left the old one in place. The running copy is
  now moved aside and removed when Windows restarts, or by the next
  update; the assistant's connection keeps working until it ends.

## 0.1.7 — 2026-10-31

- Without the service, the index of your user folder now starts with
  Flint, not at the first search in its window. Before, after a restart
  AI assistants (and `flint-indexer query`) found nothing in the user
  folder until the window had been used once.

## 0.1.6 — 2026-10-31

- AI assistants can search through Flint: `flint-indexer mcp` is a
  Model Context Protocol server over standard input and output (no
  network) with one read-only tool, `search`, returning names, paths,
  kind and last change, never file contents. See "Use from AI
  assistants" in the README.
- New setting, **Hidden from AI assistants**: folders the assistants
  never see, while Flint itself still shows them. It starts with the
  folders in the user folder that hold keys and sign-in data (`.ssh`,
  `.gnupg`, `.aws`, `.azure`, `.kube`, `.docker`), which can be removed.
  Excluded folders are hidden from assistants too.
- `flint-indexer query` searches every running index, as the app does.
  Before, with Basic mode or the app's own indexers it reported that
  no indexer was running. It leaves out the folders hidden from
  assistants, no longer names the indexer that answered, and exits with
  4 when no index answers and 2 for any other error.
  Like the MCP server, it gives up on an index that does not answer
  within 2 seconds, where it waited without end.

## 0.1.5 — 2026-10-31

- Full indexing no longer leaves memory behind in Windows. To decide
  which folders an ordinary user may list, a first index opened each
  folder it checked for its permissions (432 k of them on the measured
  C:), and on the measured PC (Windows 11 build 26300) a file system
  filter of Windows itself (wcifs.sys, the Windows Container Isolation
  filter) kept a little kernel memory for each folder opened that way
  whose cached state Windows had dropped, until the PC restarts. Each
  first index of C: left about 200 k such allocations, over 1 GB, so a
  few reinstalls or rescans filled the memory. The service now reads
  each folder's security id from the MFT and the permissions from the
  volume's own table of them (`$Secure`), after asking Windows to write
  pending changes to disk, so a permission changed a moment before is
  seen. In the normal case no folder is opened: on C: (0.97 M folders,
  2.2 k distinct security ids) reading the ids, the permissions and
  checking them took 2.1–2.3 s, the kernel allocation count did not
  change, and on a sample of 965 folders the verdicts equal those from
  opening each. A folder whose permissions cannot be read that way (a
  record being written, an id without a readable entry) is still
  opened. Folders that new changes bring are checked the same way.
- Memory already held by that filter is given back only by a restart.

## 0.1.4 — 2026-10-30

- The index takes about a quarter less memory, with nothing left out.
  Two thirds of file names are already lowercase plain letters, so their
  search form (lowercase, Turkish letters and accents folded) is the
  name itself: it is now kept once instead of twice. The table that
  follows files by their NTFS file number keeps a 4-byte key instead of
  8; a file number shared by two files (a reused record whose delete
  was missed) goes to a small second table, so both are still told
  apart. On the measured disk (C:'s 1.70 M entries, names read without
  administrator rights and built the way the service builds it) the
  private memory after a first index went from 233 MB to 174 MB and
  after a start from 225 MB to 163 MB; the saved index from 172 MB to
  127 MB, and loading it from 0.17–0.22 s to 0.13–0.16 s. Searching
  takes about 3 % longer (median 3.47–3.66 ms before, 3.63–4.01 ms
  after, on the 2 M-entry test corpus with the privacy filter on).
- The index saved by an earlier version is read and converted on the
  first start after the update, so there is no new full scan.

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
