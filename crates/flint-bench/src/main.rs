//! `flint-bench`: builds the seeded synthetic corpus and measures
//! in-process `search()` latency (the core's share of the 16 ms
//! keystroke-to-results budget).
//!
//! `flint-bench gen --entries N --seed S` prints corpus stats and samples.
//! `flint-bench bench --entries N --seed S [--boosts B] [--max-p95-ms X]`
//! runs the fixed query set with B frecency boosts (default 300, like a
//! seeded Windows Recent list; production always sends some); exits 1 when
//! overall p95 (queries of >= 2 chars) exceeds X.
//! `flint-bench snapshot --entries N --seed S` times the warm-start path:
//! corpus as one NTFS-like volume, snapshot encode, save, load, decode and
//! the volume rebuild from the decoded records.

mod corpus;

use clap::{Parser, Subcommand};
use flint_core::{Boosts, EntryId, SearchOptions, parse, search};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const QUERIES: &[&str] = &[
    "b",
    "bu",
    "ra",
    "butce",
    "rapor",
    "isik",
    "finans butce",
    "node",
    "rapro",
    "rabor taslka",
    "ext:pdf rapor",
    "d: sunum",
    "zzqxw",
];
const WARMUP: usize = 5;
const CANCEL_RUNS: usize = 100;
const NOW: u32 = 1_793_000_000; // 2026-10-27

#[derive(Parser)]
#[command(about = "Flint search core corpus generator and latency benchmark")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Build the corpus and print stats plus sample paths.
    Gen {
        #[arg(long, default_value_t = 2_000_000)]
        entries: usize,
        #[arg(long, default_value_t = 42)]
        seed: u64,
        #[arg(long, default_value_t = 20)]
        show: usize,
    },
    /// Build the corpus and measure query latency.
    Bench {
        #[arg(long, default_value_t = 2_000_000)]
        entries: usize,
        #[arg(long, default_value_t = 42)]
        seed: u64,
        /// Fail (exit 1) when overall p95 for queries of >= 2 chars exceeds this.
        #[arg(long)]
        max_p95_ms: Option<f64>,
        /// Timed runs per query.
        #[arg(long, default_value_t = 200)]
        runs: usize,
        /// Frecency boosts on seeded random live entries (0 = none).
        #[arg(long, default_value_t = 300)]
        boosts: usize,
        /// Tag the corpus user's profile as owner 1 and search as viewer 1,
        /// so every entry goes through the privacy filter (the service's
        /// normal case).
        #[arg(long)]
        private: bool,
    },
    /// Time the gate queries through the named pipe (query + round trip),
    /// with the boosts sent on every request as the app does.
    #[cfg(windows)]
    Pipe {
        #[arg(long, default_value_t = 2_000_000)]
        entries: usize,
        #[arg(long, default_value_t = 42)]
        seed: u64,
        #[arg(long, default_value_t = 50)]
        runs: usize,
        #[arg(long, default_value_t = 300)]
        boosts: usize,
        /// Exit 1 when overall p95 (queries of >= 2 chars) exceeds this.
        #[arg(long)]
        max_p95_ms: Option<f64>,
    },
    /// Time the snapshot (warm start) path on the corpus.
    Snapshot {
        #[arg(long, default_value_t = 2_000_000)]
        entries: usize,
        #[arg(long, default_value_t = 42)]
        seed: u64,
    },
}

fn pct(sorted: &[Duration], p: f64) -> f64 {
    let i = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[i].as_secs_f64() * 1e3
}

fn build(entries: usize, seed: u64) -> flint_core::Index {
    let t = Instant::now();
    let ix = corpus::generate(seed, entries);
    println!(
        "corpus: {} live entries, seed {seed}, built in {:.2} s, heap {:.1} MiB",
        ix.live_len(),
        t.elapsed().as_secs_f64(),
        ix.heap_bytes() as f64 / (1024.0 * 1024.0)
    );
    ix
}

fn main() -> ExitCode {
    match Cli::parse().cmd {
        Cmd::Gen {
            entries,
            seed,
            show,
        } => {
            let ix = build(entries, seed);
            let step = (ix.raw_len() / show.max(1)).max(1);
            for i in (0..ix.raw_len()).step_by(step).take(show) {
                println!("  {}", ix.path(EntryId(i as u32)));
            }
            ExitCode::SUCCESS
        }
        Cmd::Bench {
            entries,
            seed,
            max_p95_ms,
            runs,
            boosts,
            private,
        } => bench(entries, seed, max_p95_ms, runs, boosts, private),
        Cmd::Snapshot { entries, seed } => snapshot(entries, seed),
        #[cfg(windows)]
        Cmd::Pipe {
            entries,
            seed,
            runs,
            boosts,
            max_p95_ms,
        } => pipe_bench(entries, seed, runs, boosts, max_p95_ms),
    }
}

/// Keystroke-to-results through the pipe: a server in this process on a
/// private pipe name, one client sending each gate query with boosts.
#[cfg(windows)]
fn pipe_bench(
    entries: usize,
    seed: u64,
    runs: usize,
    n_boosts: usize,
    max_p95_ms: Option<f64>,
) -> ExitCode {
    use flint_indexer::pipe::{Client, current_user_sid, serve_index, user_sddl};
    use flint_indexer::protocol::ServerMsg;
    use flint_indexer::server::Shared;
    let ix = build(entries, seed);
    let boosts = seeded_boost_list(&ix, seed, n_boosts);
    println!("threads: {}", rayon::current_num_threads());
    println!("boosts sent per query: {}", boosts.len());
    let mut shared = Shared::new(ix, "user");
    shared.clock = || NOW;
    let name = format!(r"\\.\pipe\flint-bench-{}", std::process::id());
    let sid = current_user_sid().expect("current user");
    let _server = serve_index(&name, &user_sddl(&sid), std::sync::Arc::new(shared)).expect("serve");
    let client = Client::connect(&name, &sid).expect("connect");
    println!(
        "{:<18} {:>9} {:>9} {:>9} {:>11}",
        "query", "p50 ms", "p95 ms", "max ms", "pipe p95 ms"
    );
    let mut all = Vec::new();
    for q in QUERIES {
        let mut rt = Vec::with_capacity(runs);
        let mut pipe_only = Vec::with_capacity(runs);
        for i in 0..WARMUP + runs {
            let t = Instant::now();
            let m = client.query(q, 10, boosts.clone()).expect("query");
            let d = t.elapsed();
            let ServerMsg::Results { elapsed_us, .. } = m else {
                panic!("{q}: {m:?}")
            };
            if i >= WARMUP {
                rt.push(d);
                pipe_only.push(d.saturating_sub(Duration::from_micros(elapsed_us as u64)));
            }
        }
        rt.sort();
        pipe_only.sort();
        println!(
            "{:<18} {:>9.3} {:>9.3} {:>9.3} {:>11.3}",
            format!("{q:?}"),
            pct(&rt, 0.5),
            pct(&rt, 0.95),
            pct(&rt, 1.0),
            pct(&pipe_only, 0.95)
        );
        if q.chars().count() >= 2 {
            all.extend(rt);
        }
    }
    all.sort();
    let p95 = pct(&all, 0.95);
    println!(
        "overall through the pipe (>= 2 chars): p50 {:.3} ms, p95 {p95:.3} ms",
        pct(&all, 0.5)
    );
    match max_p95_ms {
        Some(m) if p95 > m => {
            println!("FAIL: p95 {p95:.3} ms > {m} ms");
            ExitCode::from(1)
        }
        Some(m) => {
            println!("PASS: p95 {p95:.3} ms <= {m} ms");
            ExitCode::SUCCESS
        }
        None => ExitCode::SUCCESS,
    }
}

/// Warm-start cost on the corpus seen as one volume (FRN = id + 100).
fn snapshot(entries: usize, seed: u64) -> ExitCode {
    use flint_indexer::{snapshot as snap, usn::UsnRecord, volindex::VolumeIndex};
    let ix = build(entries, seed);
    const ROOT: u64 = 5;
    let frn = |id: EntryId| id.0 as u64 + 100;
    let mut records = Vec::with_capacity(ix.raw_len());
    for i in 0..ix.raw_len() as u32 {
        let id = EntryId(i);
        let Some(p) = ix.parent(id) else { continue }; // the corpus root becomes the volume root
        if !ix.is_live(id) {
            continue;
        }
        let parent = if ix.parent(p).is_none() { ROOT } else { frn(p) };
        records.push(UsnRecord {
            frn: frn(id),
            parent,
            usn: 0,
            time: ix.last_change(id),
            reason: 0,
            attrs: if ix.is_dir(id) { 0x10 } else { 0 },
            security_id: 0,
            name: ix.name(id).to_string(),
        });
    }
    let t = Instant::now();
    let mut vix = flint_core::Index::new();
    let v = VolumeIndex::build(&mut vix, "C:", ROOT, records);
    let first = t.elapsed();
    let t = Instant::now();
    let s = snap::capture(&v, &vix, "C:", 1, 1, 1);
    let bytes = snap::encode(std::slice::from_ref(&s));
    let enc = t.elapsed();
    let dir = std::env::temp_dir().join(format!("flint-bench-snap-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("index.bin");
    let t = Instant::now();
    snap::save_atomic(&path, &bytes).expect("save");
    let save = t.elapsed();
    drop((v, vix, s));
    let t = Instant::now();
    let back = snap::load(&path).expect("load");
    let load = t.elapsed();
    let t = Instant::now();
    let mut warm = flint_core::Index::new();
    let b = back.into_iter().next().expect("one volume");
    let wv = VolumeIndex::build(&mut warm, &b.drive, b.root_frn, b.records);
    let rebuild = t.elapsed();
    let _ = std::fs::remove_dir_all(&dir);
    let ms = |d: Duration| d.as_secs_f64() * 1000.0;
    println!("entries: {} (volume map {})", warm.live_len(), wv.len());
    println!("file size: {:.1} MiB", bytes.len() as f64 / 1048576.0);
    println!(
        "build from records (first index, no I/O): {:.0} ms",
        ms(first)
    );
    println!("capture + encode: {:.0} ms", ms(enc));
    println!("save (atomic): {:.0} ms", ms(save));
    println!("load + decode: {:.0} ms", ms(load));
    println!("rebuild from snapshot: {:.0} ms", ms(rebuild));
    println!("warm start (load + rebuild): {:.0} ms", ms(load + rebuild));
    ExitCode::SUCCESS
}

/// `n` boosts on distinct live entries chosen by a seeded xorshift, with
/// weights spread over 0.1..=1.0.
fn seeded_boosts(ix: &flint_core::Index, seed: u64, n: usize) -> Boosts {
    let mut b = Boosts::new();
    for (p, w) in seeded_boost_list(ix, seed, n) {
        b.add(&p, w);
    }
    b
}

/// The paths and weights behind [`seeded_boosts`] (what the app sends).
fn seeded_boost_list(ix: &flint_core::Index, seed: u64, n: usize) -> Vec<(String, f32)> {
    let mut b = Vec::new();
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let mut added = std::collections::HashSet::new();
    while added.len() < n.min(ix.live_len()) {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let id = EntryId((x % ix.raw_len() as u64) as u32);
        if ix.is_live(id) && added.insert(id) {
            b.push((ix.path(id), 0.1 + 0.9 * (added.len() % 10) as f32 / 9.0));
        }
    }
    b
}

fn bench(
    entries: usize,
    seed: u64,
    max_p95_ms: Option<f64>,
    runs: usize,
    n_boosts: usize,
    private: bool,
) -> ExitCode {
    let mut ix = build(entries, seed);
    let viewer = if private {
        ix.set_owner_rules(&[flint_core::OwnerRule {
            path: vec!["C:".into(), "Users".into()],
            children: vec![("Deniz".into(), 1)],
            default: flint_core::OWNER_NOBODY,
        }]);
        1
    } else {
        flint_core::OWNER_NONE
    };
    println!("threads: {}", rayon::current_num_threads());
    println!(
        "privacy filter: {}",
        if private { "on (viewer 1)" } else { "off" }
    );
    let boosts = seeded_boosts(&ix, seed, n_boosts);
    println!("boosts: {n_boosts}");
    let never = AtomicBool::new(false);
    let opt = SearchOptions {
        limit: 50,
        now: NOW,
        boosts: &boosts,
        cancel: &never,
        viewer,
    };

    println!(
        "{:<16} {:>9} {:>9} {:>9} {:>10}",
        "query", "p50 ms", "p95 ms", "max ms", "matches"
    );
    let mut gated: Vec<Duration> = Vec::new();
    for q in QUERIES {
        let parsed = parse(q);
        for _ in 0..WARMUP {
            search(&ix, &parsed, &opt);
        }
        let mut times = Vec::with_capacity(runs);
        let mut matches = 0;
        for _ in 0..runs {
            let t = Instant::now();
            let r = search(&ix, &parsed, &opt);
            times.push(t.elapsed());
            matches = r.total_matches;
        }
        times.sort();
        println!(
            "{:<16} {:>9.3} {:>9.3} {:>9.3} {:>10}",
            format!("{q:?}"),
            pct(&times, 0.5),
            pct(&times, 0.95),
            pct(&times, 1.0),
            matches
        );
        if q.chars().count() >= 2 {
            gated.extend(times);
        }
    }
    gated.sort();
    let p95 = pct(&gated, 0.95);
    println!(
        "overall (>= 2 chars): p50 {:.3} ms, p95 {:.3} ms",
        pct(&gated, 0.5),
        p95
    );

    // Cancel latency: start a broad query, set the flag after 1 ms from this
    // thread, time flag -> return.
    let mut lat = Vec::new();
    let mut finished_early = 0;
    let parsed = parse("e");
    for _ in 0..CANCEL_RUNS {
        let cancel = AtomicBool::new(false);
        let (r, t_ret, t_flag) = std::thread::scope(|s| {
            let h = s.spawn(|| {
                let o = SearchOptions {
                    limit: 50,
                    now: NOW,
                    boosts: &boosts,
                    cancel: &cancel,
                    viewer,
                };
                let r = search(&ix, &parsed, &o);
                (r, Instant::now())
            });
            std::thread::sleep(Duration::from_millis(1));
            let t_flag = Instant::now();
            cancel.store(true, Ordering::Relaxed);
            let (r, t_ret) = h.join().unwrap();
            (r, t_ret, t_flag)
        });
        if r.cancelled {
            lat.push(t_ret.saturating_duration_since(t_flag));
        } else {
            finished_early += 1;
        }
    }
    lat.sort();
    if lat.is_empty() {
        println!("cancel: every run finished before the flag ({finished_early}/{CANCEL_RUNS})");
    } else {
        println!(
            "cancel latency (flag -> return): p50 {:.3} ms, p95 {:.3} ms over {} runs ({} finished first)",
            pct(&lat, 0.5),
            pct(&lat, 0.95),
            lat.len(),
            finished_early
        );
    }

    match max_p95_ms {
        Some(max) if p95 > max => {
            println!("FAIL: overall p95 {p95:.3} ms > {max} ms");
            ExitCode::FAILURE
        }
        Some(max) => {
            println!("PASS: overall p95 {p95:.3} ms <= {max} ms");
            ExitCode::SUCCESS
        }
        None => ExitCode::SUCCESS,
    }
}
