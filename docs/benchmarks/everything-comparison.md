# Flint and Everything, side by side

What this file is: one measured comparison of Flint 0.1.2 with
[Everything](https://www.voidtools.com/) 1.4.1.1032 on the same machine,
the same day, in each of Flint's two modes. It records what was
measured, how, and where the two are not alike. It is not a general
verdict: one PC, one disk, one set of searches.

## Machine

- CPU: AMD Ryzen 9 9900X, 12 cores / 24 threads; RAM 31 GiB
- Windows 11 Pro, build 26300; C: NTFS on an NVMe SSD (D: ReFS, left out)
- Measured 2026-10-30, with the released Flint 0.1.2 installer
- Flint search history not used (no boosts); Everything with its default
  settings unless noted

## Setups

| Name | Program | What it indexes |
|---|---|---|
| Flint Basic | `flint-indexer serve --user-mode` (what the app runs in Basic mode, no administrator rights) | the user's folder without app data, Start menu kept |
| Everything, same folders | an Everything folder index | the same folders: the profile with its `AppData\Local`, `Roaming` and `LocalLow` excluded, plus the Start menu folder |
| Flint Full | the FlintIndexer service (full indexing) | all of C: from the NTFS master file table; every profile's app data contents left out; what the signed-in user cannot open is hidden |
| Everything, C: | Everything's NTFS index (its default) | all of C: |
| Everything, C: without app data | the same, with every profile's `AppData\Local`, `Roaming` and `LocalLow` in its exclude list | C: as close as Everything's settings get to what Flint Full keeps |

## Start-up and memory

Cold means no saved index; warm means starting with the index each saved
last time. Each Everything and Flint Basic setup ran cold and warm, twice,
one program at a time. "Ready" is the first moment the entry count
reached 99.9 % of its count at the end of the 150 s run, the same rule for
both (Everything gives no "done" signal to a script). Memory is the
process's working set / private bytes 5 s after the run.

| Setup | Entries | Ready, cold | Ready, warm | Memory (working set / private) |
|---|---|---|---|---|
| Flint Basic | 151,618 | 3.0–5.1 s | 0.12 s | 25 / 22–26 MB |
| Everything, same folders | 153,523 | 4.3–4.4 s | 0.10–0.12 s | 29–31 / 16–18 MB |
| Flint Full | 1,450,240–1,451,815 | 26.7–39.5 s | 0.33 s | 231–239 / 310–392 MB |
| Everything, C: | 4,097,124 | 18.1–20.0 s | 1.15–1.16 s | 370–377 / 359–366 MB |
| Everything, C: without app data | 1,750,283 | 10.6–12.5 s | 0.46–2.16 s | 182–187 / 170–176 MB |

How to read it:

- **Same folders:** the two are about even. Flint's warm start answers
  from its saved index at 0.12 s and then checks the folders again in the
  background (done after 2.3–3.0 s).
- **Whole C:, first index:** Everything builds it in less time and uses
  less memory per entry: 109–112 bytes per entry without app data (96
  with it) against Flint Full's 167–173 bytes of working set; in private
  bytes 102–105 against 224–283. Flint Full's first index includes
  Flint's folder permission checks (see
  [indexer-baseline.md](indexer-baseline.md)), which Everything does not
  make.
- **Whole C:, restart:** Flint Full answers 0.33 s after the service
  starts with its saved index, in all three runs; Everything without app
  data took 0.46–2.16 s with its saved database (1.15 s with all of C:).
- **Flint Full's private memory is higher after a restart** (383–392 MB)
  than after a first index (310–312 MB), with the same entries and the
  same working set. Noted as a finding; its cause has not been looked
  into.
- **Entries differ on C:** Everything lists every name on the volume to
  every user; Flint Full hides what the signed-in user cannot open (other
  profiles, restricted system folders). Everything's exclude list also
  does not quite match Flint's rules (for example the system profiles'
  app data). So Flint Full holds 0.30 M fewer entries than the closest
  Everything setup.

### How Flint Full was measured

Three first-index runs and three restarts:

- Run 1, without administrator rights: full indexing was turned off and
  on again in Flint's Settings while a script polled. Timed from the
  moment `sc query FlintIndexer` showed the service running to the moment
  `flint-indexer status` showed `C: live` (33.7 s); working set from
  `tasklist` (238–239 MB, 10–300 s after).
- Runs 2–3 and the restarts, from an elevated script: stop the service
  (`sc stop`, 0.2–0.5 s; it saves its index image), delete the image
  (`%ProgramData%\Flint\index.img`) for a first index or keep it for a
  restart, wait 3 s, `sc start`, and poll `flint-indexer status` every
  50 ms until `C: live` (39.5 s and 26.7 s; restarts 0.33 s each).
  Working set and private bytes from `Get-Process`, 10 s and 60 s after.

Between the runs the system's file cache was warm in different ways; the
spread of the first index (26.7–39.5 s) is that.

### How Everything was run

As its own portable instance (`-instance flintbench`, its own settings
file and database), so the installed Everything was not changed. For the
NTFS setups it read the volume through the installed Everything Service
(`service_pipe_name` set to the default pipe), as any Everything client
does without administrator rights. Its memory is the Everything process
only; the service process stayed under 15 MB.

## Search results

17 searches for files on this PC, fixed before any result was seen, in
four kinds: plain words (4), several words (5), a typo (4), and Turkish
letters typed differently from the file name, such as `s` where the
name has `ş`, or the reverse (4). The file names are personal and not published here. Each
search asked for up to 1,000 results; the table counts where the wanted
file came.

| | Flint Full | Everything, C: | Flint Basic | Everything, same folders |
|---|---|---|---|---|
| Found at all | 17 / 17 | 12 / 17 | 17 / 17 | 12 / 17 |
| First result | 13 / 17 | 6 / 17 | 13 / 17 | 8 / 17 |
| Among the first 3 | 17 / 17 | 9 / 17 | 15 / 17 | 11 / 17 |

By kind, files found / first result:

| Kind | Flint Full | Everything, C: | Flint Basic | Everything, same folders |
|---|---|---|---|---|
| Plain (4) | 4 / 4 | 4 / 2 | 4 / 4 | 4 / 3 |
| Several words (5) | 5 / 3 | 5 / 1 | 5 / 5 | 5 / 2 |
| Typo (4) | 4 / 3 | 0 / 0 | 4 / 2 | 0 / 0 |
| Turkish letters (4) | 4 / 3 | 3 / 3 | 4 / 3 | 3 / 3 |

How to read it:

- Everything matches the typed text literally, by design: none of the
  four typos found anything. Turkish letters typed differently were found
  in 3 of 4 cases (Everything ignores accents by default, and here also
  `ı`/`i`); the fourth search has one word in the file's folder name, and
  Everything matches file names only unless the search names a path.
  Flint matches typos, folds Turkish letters and accents, and matches
  words in the folder names too.
- Everything lists results in its default order (by name), not by how
  well they match, so its "first result" counts measure a different idea
  of order than Flint's ranking. Flint's history boosts were not used.
- Everything 1.5 (alpha) and Everything's search syntax (wildcards,
  `regex:`) were not tried.

## Search time: not compared

Flint's own search time is reported by its engine: median 3.9 ms on the
1.45 M entries of Flint Full (max 12.9 ms), 0.5 ms on Basic. Everything's
command-line client (`es.exe`) reports no search time of its own, and its
wall time (median 56–153 ms per search here) is mostly the client
starting and talking to Everything's window, not the search. A fair
search-speed comparison needs both measured the same way inside each
program; it was not made, so no speed claim is drawn from these runs.

## Not measured here

- Live updates (how fast a new file shows up) in either program.
- Other machines, a hard disk, or a volume with more files.
