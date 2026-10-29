# Core latency baseline

What this file is: the recorded result of the search-core latency gate on a
synthetic 2 M-entry Windows-like corpus. Later plans compare against it; a
change that pushes overall p95 above 8 ms fails the gate
(`flint-bench bench --max-p95-ms 8` exits 1).

How to reproduce:

```
cargo run -p flint-bench --release -- bench --entries 2000000 --seed 42 --max-p95-ms 8
```

Run it on an otherwise idle machine. Another CPU-heavy process makes the
numbers meaningless (during development, a parallel simulation produced
71–121 ms spikes on queries that do almost no work).

## Machine

- CPU: AMD Ryzen 9 9900X, 12 cores / 24 threads (search uses all 24)
- RAM: 31.1 GB
- OS: Windows 11 Pro, build 26300
- Rust: 1.99.1, release profile

## Result (2026-10-29, commit ca4a4a0, 300 boosts, 3 consecutive runs, all PASS)

The bench sends 300 seeded frecency boosts by default, as the app always
does (Windows Recent seeds them), and includes the two-typo query
"rabor taslka".

| Run | overall p50 | overall p95 (≥ 2 chars) | cancel p95 |
|---|---|---|---|
| 1 | 3.266 ms | 7.963 ms | 0.235 ms |
| 2 | 3.131 ms | 6.934 ms | 0.345 ms |
| 3 | 3.211 ms | 7.130 ms | 0.265 ms |

Budget: overall p95 ≤ 8 ms (the core's share of the 16 ms keystroke-to-result
budget); cancel latency target ≤ 2 ms. The margin is thin: run 1 passed by
0.04 ms, and "rabor taslka" alone is above 8 ms at p95 in runs 1 and 3. It is
the query to speed up first if a later change costs time; about 40 % of its
time goes to the compact name check on the ~343 k names whose letters allow
both terms.

Per query, run 1 (200 timed runs each):

| query | p50 ms | p95 ms | max ms | matches |
|---|---|---|---|---|
| "b" | 2.950 | 3.154 | 3.279 | 458865 |
| "bu" | 2.363 | 2.519 | 3.073 | 84913 |
| "ra" | 3.554 | 3.916 | 5.982 | 234661 |
| "butce" | 2.053 | 2.350 | 2.954 | 11192 |
| "rapor" | 4.565 | 4.947 | 5.274 | 20130 |
| "isik" | 2.472 | 2.667 | 3.741 | 35534 |
| "finans butce" | 4.597 | 4.967 | 5.217 | 2034 |
| "node" | 2.069 | 2.319 | 2.807 | 31024 |
| "rapro" | 4.760 | 5.197 | 6.312 | 12358 |
| "rabor taslka" | 8.123 | 9.376 | 10.613 | 436 |
| "ext:pdf rapor" | 1.711 | 1.927 | 2.822 | 618 |
| "d: sunum" | 3.487 | 3.828 | 4.026 | 2099 |
| "zzqxw" | 0.532 | 0.665 | 0.842 | 0 |

Earlier result without boosts (commit 40fb29b, the same 12 queries minus
"rabor taslka"): overall p95 5.21–5.26 ms in 3 runs.

`matches` counts every match without a typo plus the typo hits in the
returned top 50 (see `SearchResult::total_matches`).

## Memory and build time

- Corpus build: 0.64–0.74 s for 2 000 000 entries (camelCase detection on
  insert added about 6 %).
- `heap_bytes` after build: 188.0 MiB. It is printed before the first search,
  so it excludes the directory tree that the first search builds and caches
  (`DirTree`, 269 260 directories: about 11.5 MB, computed from its column
  sizes, mostly the 8 MB entry→position map). Steady state is therefore about
  200 MiB. Each multi-term query also allocates its folder tables
  (2 bytes per directory per term, about 1 MB for two terms) for its own
  duration.

## How the budget was reached

Measured per step on this corpus (first gate run: p95 62 ms, cancel 25 ms):
thread-local nucleo matcher; cached directory tree; letter-mask rejection
before touching names; a top-k floor shared across workers with exact upper
bounds (pinned by `pruning_keeps_the_exact_top_k`); compact fuzzy acceptance
(scattered letters across long names no longer match); a typo tier anchored
at word starts (camelCase boundaries included) with the first letter fixed,
checked only where it can still reach the top k; one parallel pass for all
terms' folder tables; frecency bounded per entry by its leaf name; a top k
shared by all workers so rare queries build a floor; multi-term names
rejected on every term's letters before the name is read.

Not measured here: slower machines (fewer cores), the indexer service's
background load (indexer-baseline.md), and end-to-end keystroke-to-pixel
time (app-baseline.md).
