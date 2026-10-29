//! Top-k pruning must never change results. Each test compares a search with
//! a small `limit` against the same search with no effective limit (the
//! oracle), or runs one query set on 1, 3 and 8 threads. A failure means an
//! upper bound in `search.rs` is lower than some entry's real score, which
//! silently drops a correct result.

use flint_core::{Boosts, EntryId, EntryInput, Index, SearchOptions, SearchResult, parse, search};
use std::sync::atomic::AtomicBool;

struct Rng(u64);
impl Rng {
    fn n(&mut self, n: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % n as u64) as usize
    }
}

const WORDS: &[&str] = &[
    "rapor",
    "bütçe",
    "ışık",
    "finans",
    "sunum",
    "notlar",
    "taslak",
    "proje",
    "arsiv",
    "yedek",
    "ozet",
    "ekim",
    "deniz",
    "aile",
    "Report",
    "Quarterly",
    "plan",
    "rabor",
    "rapro",
    "taslka",
    "temp",
    "documents",
    "desktop",
    "node_modules",
    "2026",
    "v1.2",
    "AppData",
];
const SEPS: &[&str] = &["_", "-", " ", "", "."];

fn typo(r: &mut Rng, w: &str) -> String {
    let mut c: Vec<char> = w.chars().collect();
    if c.len() < 3 {
        return w.to_string();
    }
    let i = 1 + r.n(c.len() - 1);
    match r.n(4) {
        0 => {
            c.remove(i);
        }
        1 => {
            c.insert(i, 'x');
        }
        2 => {
            c[i] = 'q';
        }
        _ => {
            if i + 1 < c.len() {
                c.swap(i, i + 1);
            }
        }
    }
    c.into_iter().collect()
}

fn corpus(seed: u64, n: usize) -> Index {
    let mut r = Rng(seed);
    let mut ix = Index::new();
    let mut dirs = vec![];
    for d in ["C:", "D:"] {
        dirs.push(ix.insert(EntryInput {
            name: d,
            parent: None,
            is_dir: true,
            last_change: 0,
        }));
    }
    for i in 0..n {
        let k = 1 + r.n(3);
        let mut name = String::new();
        for j in 0..k {
            if j > 0 {
                name.push_str(SEPS[r.n(SEPS.len())]);
            }
            let w = WORDS[r.n(WORDS.len())];
            if r.n(5) == 0 {
                name.push_str(&typo(&mut r, w));
            } else {
                name.push_str(w);
            }
        }
        name.push_str([".pdf", ".txt", "", ".docx"][r.n(4)]);
        let parent = dirs[r.n(dirs.len())];
        let is_dir = r.n(6) == 0;
        let id = ix.insert(EntryInput {
            name: &name,
            parent: Some(parent),
            is_dir,
            last_change: 1_700_000_000 + (i as u32) * 7919,
        });
        if is_dir {
            dirs.push(id);
        }
        if r.n(50) == 0 {
            ix.remove(EntryId(r.n(ix.raw_len()) as u32));
        }
    }
    ix
}

fn queries(r: &mut Rng) -> Vec<String> {
    let mut out = vec![];
    for _ in 0..120 {
        let k = 1 + r.n(3);
        let mut q = vec![];
        for _ in 0..k {
            let w = flint_core::fold::fold_str(WORDS[r.n(WORDS.len())]);
            let w = match r.n(4) {
                0 => typo(r, &w),
                1 => w.chars().take(2 + r.n(3)).collect(),
                _ => w,
            };
            q.push(w);
        }
        if r.n(9) == 0 {
            let w = q[0].clone();
            q[0] = format!("\"{w}\"");
        }
        match r.n(8) {
            0 => q.push("ext:pdf".into()),
            1 => q.push("d:".into()),
            2 => {
                let l = q.len() - 1;
                q[l].push('/')
            }
            _ => {}
        }
        out.push(q.join(" "));
    }
    out
}

#[test]
fn pruning_matches_oracle_randomized() {
    let mut failures = vec![];
    for seed in 1..=30u64 {
        let ix = corpus(seed * 7777, 15_000 + (seed as usize % 4) * 9_000);
        let mut r = Rng(seed * 31 + 1);
        let mut boosts = Boosts::new();
        if seed % 2 == 0 {
            for _ in 0..20 {
                boosts.add(&ix.path(EntryId(r.n(ix.raw_len()) as u32)), 0.5);
            }
        }
        let cancel = AtomicBool::new(false);
        for q in queries(&mut r) {
            let pq = parse(&q);
            let go = |limit| {
                search(
                    &ix,
                    &pq,
                    &SearchOptions {
                        limit,
                        now: 1_700_300_000,
                        boosts: &boosts,
                        cancel: &cancel,
                        viewer: flint_core::OWNER_NONE,
                    },
                )
            };
            let full = go(10_000_000);
            for k in [1usize, 5, 50] {
                let top = go(k);
                let want: Vec<_> = full.hits.iter().take(k).map(|h| (h.id, h.score)).collect();
                let got: Vec<_> = top.hits.iter().map(|h| (h.id, h.score)).collect();
                if got != want {
                    let w0 = want
                        .iter()
                        .find(|x| !got.contains(x))
                        .map(|x| (ix.name(x.0).to_string(), x.1));
                    failures.push(format!("seed {seed} q {q:?} k {k}: missing {w0:?}"));
                    break;
                }
            }
        }
    }
    for f in failures.iter().take(25) {
        println!("{f}");
    }
    println!("{} failures", failures.len());
    assert!(failures.is_empty());
}

#[test]
fn deterministic_across_thread_counts() {
    let ix = corpus(99, 60_000);
    let b = Boosts::new();
    let cancel = AtomicBool::new(false);
    let mut r = Rng(5);
    let qs = queries(&mut r);
    let runs: Vec<Vec<Vec<(EntryId, i64)>>> = [1usize, 3, 8]
        .iter()
        .map(|&t| {
            let pool = rayon::ThreadPoolBuilder::new()
                .num_threads(t)
                .build()
                .unwrap();
            pool.install(|| {
                qs.iter()
                    .map(|q| {
                        search(
                            &ix,
                            &parse(q),
                            &SearchOptions {
                                limit: 20,
                                now: 1_700_300_000,
                                boosts: &b,
                                cancel: &cancel,
                                viewer: flint_core::OWNER_NONE,
                            },
                        )
                        .hits
                        .iter()
                        .map(|h| (h.id, h.score))
                        .collect()
                    })
                    .collect()
            })
        })
        .collect();
    assert!(runs[0] == runs[1] && runs[1] == runs[2]);
}

fn flat(names: &[&str]) -> Index {
    let mut ix = Index::new();
    let c = ix.insert(EntryInput {
        name: "C:",
        parent: None,
        is_dir: true,
        last_change: 0,
    });
    for n in names {
        ix.insert(EntryInput {
            name: n,
            parent: Some(c),
            is_dir: false,
            last_change: 0,
        });
    }
    ix
}

fn run(ix: &Index, q: &str, limit: usize) -> SearchResult {
    let cancel = AtomicBool::new(false);
    let b = Boosts::new();
    search(
        ix,
        &parse(q),
        &SearchOptions {
            limit,
            now: 0,
            boosts: &b,
            cancel: &cancel,
            viewer: flint_core::OWNER_NONE,
        },
    )
}

/// Two typo terms, the first at the start of the name: the second term's
/// bound must include the prefix bonus the first typo already earned.
#[test]
fn double_typo_with_prefix_is_not_pruned() {
    let ix = flat(&["x_rabor_taslka.txt", "rapor_taslak.pdf"]);
    let full = run(&ix, "rabor taslka", 1000);
    let top1 = run(&ix, "rabor taslka", 1);
    assert_eq!(ix.name(full.hits[0].id), "rapor_taslak.pdf");
    assert_eq!(top1.hits[0].id, full.hits[0].id);
}
