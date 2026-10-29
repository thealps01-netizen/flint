# Indexer baseline

What this file is: the measured result of the indexer's end-to-end gates
on a real machine, with the service installed, used and removed again.
Later plans compare against it. A budget that was missed is marked
**MISSED** with its cause; it is not silently relaxed.

How it was measured: an elevated script installs the `FlintIndexer`
service from a release build, waits until it is live, lets an unelevated
script (the normal user) run the gate queries, privacy checks and a live
journal check, samples idle CPU and memory, times a stop and a warm
start, and uninstalls everything (`%ProgramData%\Flint`, `%ProgramFiles%\Flint`
and the service registration are checked to be gone). The scripts only
read file names and dates; they create, rename and delete files only in
their own scratch folder.

## Machine

- CPU: AMD Ryzen 9 9900X, 12 cores / 24 threads
- RAM: 31 GiB
- OS: Windows 11 Pro, build 26300
- Disk: Kingston NVMe SSD
- Volumes: C: NTFS 1.9 TB (indexed by the service); D: ReFS 64 GB (not
  NTFS, so the service does not index it: user mode walks such drives)

## Result (2026-10-29, third run, the code of this commit)

| What | Measured | Budget | |
|---|---|---|---|
| Entries on C: | 3,534,562 (3,379,446 visible to the signed-in user) | | |
| First index, MFT read + build | 19.76 s + 2.50 s = 6.3 s per 1 M | ≤ 10 s per 1 M | PASS |
| Folder access checks in the first index | 19.12 s for 295,847 folders (+86 %) | +20 % of the first index | **MISSED** |
| Install to live (cold start) | 52.5 s | | |
| Warm start, service start to first answer | 0.72 s (image read 0.31 s, volumes 0.13 s; live after 0.96 s) | ≤ 2 s | PASS |
| Query, server time, 13 gate queries × 3 rounds | p50 5.1 ms, p95 12.7 ms, max 13.6 ms | ≤ 16 ms incl. the pipe | PASS |
| Same, after 2,000 files came and went | p95 12.6 ms, max 12.6 ms | | PASS |
| Idle CPU over 60 s | 0.026 % of one core (16 ms CPU) | ~0 % | PASS |
| Memory after the cold start | working set 413 MiB, private 662 MiB | target ≤ 200 MB at 2 M | **MISSED** (target) |
| Journal writer's wait for the index lock, 8 clients × "b" | p95 44.9 ms, max 126 ms | move readers to a snapshot if p95 > 100 ms | kept |
| Service stop | 0.59–0.79 s | ≤ 5 s | PASS |
| Change seen in results (create, rename, move, delete) | 36–42 ms | | |
| Privacy: 8 file names from `C:\Windows\Prefetch` | 0 results from that folder | 0 | PASS |
| Privacy: `ntuser dat` | 0 results from other profiles | 0 | PASS |

Query times are the server's own; the CLI's wall time adds its process
start (about 15 ms). The core's share of the query budget (≤ 8 ms p95) is
defined on the 2 M synthetic corpus (`core-baseline.md`); this volume has
1.7 times as many entries.

## Misses and their causes

- **Access checks +86 %.** Every folder no profile rule owns is opened
  by FRN and its security descriptor checked (2,210 came out restricted,
  14 checks failed and are hidden). Eight threads cut it from 49.4 s
  (first run, one thread) to 19.1 s; the kernel's per-open cost dominates
  beyond that. Sharing one check per NTFS security id was built in, but
  `FSCTL_ENUM_USN_DATA` leaves `SecurityId` at 0 for all 295,847 folders,
  so it never applies. Paid once per install: warm starts load the saved
  restrictions. Options, not built: read security ids from the raw MFT
  (`$STANDARD_INFORMATION`), or publish profile-owned entries first and
  public folders as their checks finish.
- **Memory.** About 188 bytes per entry private: original and folded
  name text, ten per-entry columns, the FRN maps and the folder tree, plus
  what the cold start's private build and copy left in the allocator. Not
  measured after a warm start. The 200 MB figure was an unmeasured target;
  this is its first measurement.

## Fixed during the gate runs

| Found in | Problem | Fix | Before → after |
|---|---|---|---|
| Run 1 | Every folder rename on the busy volume dropped the folder tree, so the next query rebuilt it | The tree is updated in place for the renamed folder's subtree | next query after a folder rename (2 M synthetic): 73.4 ms → 4.1 ms median |
| Run 1 | Idle CPU 6.7 %: one batch per journal record, and a security check for every folder created inside a profile | 30 ms debounce gathers a burst into one batch; folders inside a profile are not checked (the profile owner already limits them) | 6.7 % → 0.026 % |
| Run 2 | Warm start 8.35 s: the snapshot was rebuilt record by record | The service saves an image of the index (names, columns, FRN maps) and loads it with a copy | 8.35 s → 0.72 s |
| Run 1 | Access checks 49.4 s on one thread | Eight threads | 49.4 s → 19.1 s |

## Not measured here

- The enumeration timestamp finding (how many MFT records carry a time):
  the service does not count it; `flint-indexer scan C:` prints it and
  was not run separately.
- Pipe round trip on an idle machine (3 runs): measured with
  `flint-bench pipe` (p95 7.1 / 9.6 / 9.4 ms through the pipe on 2 M).
