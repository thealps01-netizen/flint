//! Parallel, cancellable search over an [`Index`].
//!
//! One pass over every entry. Each term matches the name compactly
//! (`fuzzy::quick_match`, scored by nucleo) or, failing that, with one
//! typo (edit distance 1, terms of >= 4 chars); typo hits pay a fixed
//! penalty instead of sinking below every fuzzy hit. A single-term query
//! matches names only. With two or more terms, each term matches
//! the name or an ancestor folder name (path-only), and at least one term
//! must match the name. Path-only scores come from a per-query table over
//! directories, so the hot loop never walks ancestors.

use crate::fold::{char_mask, fold, utf16_ranges};
use crate::fuzzy::{FuzzyMatcher, atom, max_score, quick_match};
use crate::index::{Columns, DirTree, EntryId, F_DEAD, F_DIR, Index, NONE, Owner, visible_to};
use crate::query::{Query, Term};
use crate::rank::{self, Boosts};
use crate::typo::ApproxFinder;
use nucleo_matcher::pattern::Atom;
use rayon::prelude::*;
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::Instant;

pub struct SearchOptions<'a> {
    pub limit: usize,
    pub now: u32,
    pub boosts: &'a Boosts,
    pub cancel: &'a AtomicBool,
    /// Who is asking (privacy filter): entries owned by someone else, or
    /// by `OWNER_NOBODY`, are skipped before counting and ranking. Use
    /// `OWNER_NONE` when the index has no owner roots.
    pub viewer: Owner,
}

pub struct Hit {
    pub id: EntryId,
    pub score: i64,
    pub typo: bool,
    /// Half-open UTF-16 ranges on the original name.
    pub highlights: Vec<(u32, u32)>,
}

pub struct SearchResult {
    pub hits: Vec<Hit>,
    /// Every match without a typo, plus the typo hits in `hits`. Typo
    /// candidates that cannot reach the top `limit` are never checked,
    /// so they are not counted; the count stays deterministic.
    pub total_matches: u32,
    pub cancelled: bool,
    pub elapsed_us: u32,
}

/// Entries per work unit (load balancing across threads).
const CHUNK: usize = 16_384;
/// The cancel flag is read every this many entries inside a chunk.
const CANCEL_STRIDE: usize = 1_024;

thread_local! {
    /// One matcher per thread: `Matcher::new` allocates ~135 KB, far too
    /// much to repeat per rayon split.
    static MATCHER: std::cell::RefCell<FuzzyMatcher> = std::cell::RefCell::new(FuzzyMatcher::new());
}

fn with_matcher<R>(f: impl FnOnce(&mut FuzzyMatcher) -> R) -> R {
    MATCHER.with(|m| f(&mut m.borrow_mut()))
}
/// Path-only matches are worth this fraction of a name match.
const PATH_ONLY_DIVISOR: u32 = 4;

/// Min-heap key: lowest score first; on equal score the higher id is worse.
type Key = Reverse<(i64, Reverse<u32>, bool)>;

struct Ctx<'a> {
    ix: &'a Index,
    cols: Columns<'a>,
    tree: &'a DirTree,
    /// Per term: prepared nucleo atom.
    atoms: Vec<Atom>,
    /// Lowest score any worker's full top-k holds; an entry whose upper
    /// bound is below it cannot reach the global top k.
    floor: AtomicI64,
    /// Scores of the best `limit` entries any worker has scored so far.
    /// Rare queries never fill a single worker's heap (2 M names in
    /// many chunks, a few hundred matches), so without this the floor
    /// would stay at its minimum and nothing would be pruned. Locked only
    /// for scores at or above the floor.
    top: Mutex<BinaryHeap<Reverse<i64>>>,
    /// Per term: `fold::char_mask` of its text.
    masks: Vec<u32>,
    q: &'a Query,
    opt: &'a SearchOptions<'a>,
    ext_suffix: Option<String>,
    /// See [`path_tables`]; empty for a single-term query.
    up: Vec<u16>,
    /// Per term: typo searcher (`None` for terms too short for typos).
    typo: Vec<Option<ApproxFinder>>,
    /// Per term: one byte class may be missing from the name's mask
    /// (a typo can drop one letter class).
    slack: Vec<u32>,
    /// Per term: mask bit of its first char; a typo never changes the
    /// first letter, so a typo candidate's name must contain it.
    first: Vec<u32>,
    /// Per term: `fuzzy::max_score`.
    max_scores: Vec<u32>,
    /// Per term: match score credited to a typo hit.
    credits: Vec<u32>,
    /// Best score an entry can reach when no term's letters are all in
    /// its name: every term then contributes at most a typo credit or a
    /// path-only score.
    slack_upper: i64,
    /// The index has owner roots, so the viewer filter runs.
    private: bool,
}

struct Worker {
    /// Scratch: indices of the terms matched in the current name.
    in_name: Vec<usize>,
    heap: BinaryHeap<Key>,
    total: u32,
    cancelled: bool,
}

impl Worker {
    fn new() -> Self {
        Self {
            in_name: Vec::new(),
            heap: BinaryHeap::new(),
            total: 0,
            cancelled: false,
        }
    }

    /// Tight first loop: only the letter mask, flags and parent are read,
    /// so most entries are rejected without touching their name.
    fn run_chunk(&mut self, cx: &Ctx, chunk: usize) {
        if self.cancelled {
            return;
        }
        let start = chunk * CHUNK;
        let end = (start + CHUNK).min(cx.ix.raw_len());
        let mask = &cx.cols.mask[start..end];
        let flags = &cx.cols.flags[start..end];
        let parent = &cx.cols.parent[start..end];
        let owner = if cx.private {
            &cx.tree.entry_owner[start..end]
        } else {
            &[][..]
        };
        let need_dir = cx.q.folders_only;
        for k in 0..mask.len() {
            if k % CANCEL_STRIDE == 0 && cx.opt.cancel.load(Ordering::Relaxed) {
                self.cancelled = true;
                return;
            }
            let em = mask[k];
            if !cx.masks.iter().any(|&m| m & !em == 0) {
                // only a typo can match: skip unless it can still rank
                if !(0..cx.masks.len()).any(|t| {
                    (cx.masks[t] & !em).count_ones() <= cx.slack[t] && em & cx.first[t] != 0
                }) || cx.slack_upper < self.floor(cx)
                {
                    continue;
                }
            }
            let f = flags[k];
            if f & F_DEAD != 0 || parent[k] == NONE || (need_dir && f & F_DIR == 0) {
                continue;
            }
            if cx.private && !visible_to(owner[k], cx.opt.viewer) {
                continue;
            }
            if cx.q.terms.len() >= 2 && !every_term_possible(cx, em, parent[k]) {
                continue;
            }
            self.visit(
                cx,
                EntryId((start + k) as u32),
                em,
                f & F_DIR != 0,
                parent[k],
            );
        }
    }

    /// Lowest score that can still enter the top k (exclusive).
    fn floor(&self, cx: &Ctx) -> i64 {
        let shared = cx.floor.load(Ordering::Relaxed);
        match self.heap.peek() {
            Some(Reverse((s, _, _))) if self.heap.len() >= cx.opt.limit => shared.max(*s),
            _ => shared,
        }
    }

    fn visit(&mut self, cx: &Ctx, id: EntryId, emask: u32, is_dir: bool, parent: u32) {
        let ix = cx.ix;
        let folded = ix.folded(id);
        if let Some(sfx) = &cx.ext_suffix
            && (is_dir || !folded.ends_with(sfx.as_str()))
        {
            return;
        }

        // Cheap acceptance first (subsequence / substring, then path table
        // and typo for the remaining terms); the nucleo score runs only for
        // entries that can still enter this worker's top k.
        self.in_name.clear();
        let mut name_upper: u32 = 0; // bound on the nucleo scores still to compute
        for (t, term) in cx.q.terms.iter().enumerate() {
            if cx.masks[t] & !emask == 0 && quick_match(folded, term) {
                self.in_name.push(t);
                name_upper += cx.max_scores[t];
            }
        }
        // At least one term must be in the name (compactly or with a typo);
        // with none compactly in it, the entry is bounded by `slack_upper`.
        if self.in_name.is_empty()
            && (cx.typo.iter().all(Option::is_none) || cx.slack_upper < self.floor(cx))
        {
            return;
        }
        // Reject before any typo search when some term can match neither
        // through a folder nor through a typo (same tests as the loop
        // below, minus the costly `find`): with several terms, the first
        // term's typo search would otherwise run on names the second
        // term's letters already rule out.
        // A term not in the name scores exactly its folder score or, with
        // no folder match, its typo credit: `pending` sums those, so the
        // typo bounds below are tight (a nucleo maximum here would keep
        // every typo search above the floor).
        let nt = cx.q.terms.len();
        let pos = if cx.up.is_empty() {
            usize::MAX
        } else {
            cx.tree.pos_of[parent as usize] as usize * nt
        };
        let path_of = |t: usize| if pos == usize::MAX { 0 } else { cx.up[pos + t] };
        let mut pending: u32 = 0;
        for t in 0..nt {
            if self.in_name.contains(&t) {
                continue;
            }
            let path = path_of(t);
            if path > 0 {
                pending += path as u32 / PATH_ONLY_DIVISOR;
                continue;
            }
            if cx.typo[t].is_none()
                || (cx.masks[t] & !emask).count_ones() > 1
                || emask & cx.first[t] == 0
            {
                return;
            }
            pending += cx.credits[t];
        }
        // The most a frecency boost can add to this entry (0 unless a boost
        // has its name). Hashing the name costs as much as a typo search,
        // so it runs only once a bound with the largest boost of all
        // reaches the floor.
        let max_boost = cx.opt.boosts.max_bonus();
        let mut boost_upper: Option<i64> = None;
        let mut known: u32 = 0; // path-only and typo contributions
        let mut typo = false;
        let mut typo_prefix = false; // the first term's typo match starts the name
        let name_prefix =
            cx.q.terms
                .first()
                .is_some_and(|t| folded.starts_with(t.text.as_str()));
        let floor = self.floor(cx);
        for t in 0..cx.q.terms.len() {
            if self.in_name.len() == cx.q.terms.len() {
                break;
            }
            if self.in_name.contains(&t) {
                continue;
            }
            let path = path_of(t);
            pending -= if path > 0 {
                path as u32 / PATH_ONLY_DIVISOR
            } else {
                cx.credits[t]
            };
            if path > 0 {
                known += path as u32 / PATH_ONLY_DIVISOR;
                continue;
            }
            // letters and first letter were checked above
            let Some(f) = &cx.typo[t] else { return };
            // Check the typo only where it can still reach the top k: every
            // other unresolved term counts with its exact contribution.
            let credit = cx.credits[t];
            let base = (known + name_upper + credit + pending) as i64 * rank::MATCH_SCALE
                + rank::TYPO_PENALTY
                + rank::RECENCY_MAX as i64
                + rank::MAX_PATH_BONUS
                // an earlier term's typo at the start already earned it
                + if name_prefix || typo_prefix { rank::PREFIX_BONUS } else { 0 };
            let start_bonus = if t == 0 { rank::PREFIX_BONUS } else { 0 };
            let upper = if base + max_boost + start_bonus < floor {
                base // below the floor with any boost: both checks fail
            } else {
                base + *boost_upper.get_or_insert_with(|| cx.opt.boosts.leaf_bonus(folded))
            };
            let found = if upper >= floor {
                f.find(folded.as_bytes(), ix.camel_source(id))
            } else if t == 0 && upper + rank::PREFIX_BONUS >= floor {
                // only a typo at the very start (prefix bonus) can make it
                f.find_at_start(folded.as_bytes())
            } else {
                return;
            };
            let Some((at, _)) = found else { return };
            typo = true; // a typo hit counts as a name match
            typo_prefix |= t == 0 && at == 0;
            known += credit;
        }
        if self.in_name.is_empty() && !typo {
            return; // every term only in folder names
        }
        if let Some(d) = cx.q.drive
            && ix.drive(id) != d
        {
            return;
        }
        if !typo {
            self.total += 1; // typo hits are counted from the final list
        }

        // Bound with the context terms at their maximum first: for broad
        // queries most matches stop here, before recency, path class or
        // nucleo are computed.
        let mut fixed: i64 = 0;
        if typo {
            fixed += rank::TYPO_PENALTY;
        }
        if typo_prefix || name_prefix {
            fixed += rank::PREFIX_BONUS;
        }
        let match_upper = (known + name_upper) as i64 * rank::MATCH_SCALE;
        let bound = match_upper + fixed + rank::RECENCY_MAX as i64 + rank::MAX_PATH_BONUS;
        if bound + max_boost < floor {
            return;
        }
        let boost_upper = *boost_upper.get_or_insert_with(|| cx.opt.boosts.leaf_bonus(folded));
        if bound + boost_upper < floor {
            return;
        }
        let context =
            fixed + rank::recency(ix.last_change(id), cx.opt.now) + rank::path_adjust(ix, id);
        if match_upper + context + boost_upper < floor {
            return;
        }
        let sum = known
            + with_matcher(|m| {
                self.in_name
                    .iter()
                    .map(|&t| m.score_atom(folded, &cx.atoms[t]).unwrap_or(0))
                    .sum::<u32>()
            });
        let boost = if boost_upper > 0 {
            rank::frecency(ix, id, cx.opt.boosts)
        } else {
            0
        };
        let score = sum as i64 * rank::MATCH_SCALE + context + boost;
        let limit = cx.opt.limit;
        self.heap.push(Reverse((score, Reverse(id.0), typo)));
        if self.heap.len() > limit {
            self.heap.pop();
        }
        if self.heap.len() >= limit
            && let Some(Reverse((min, _, _))) = self.heap.peek()
        {
            cx.floor.fetch_max(*min, Ordering::Relaxed);
        }
        // the shared top k holds distinct entries' scores, so its minimum
        // is a valid floor once it is full
        if score >= cx.floor.load(Ordering::Relaxed) {
            let mut top = cx.top.lock().unwrap_or_else(|e| e.into_inner());
            top.push(Reverse(score));
            if top.len() > limit {
                top.pop();
            }
            if top.len() >= limit
                && let Some(Reverse(min)) = top.peek()
            {
                cx.floor.fetch_max(*min, Ordering::Relaxed);
            }
        }
    }
}

/// Multi-term queries need every term somewhere: all its letters in the
/// name, a typo (one letter class missing at most, first letter present),
/// or a matching folder. Decided from the letter mask and the folder table
/// alone, so names that fail are never read.
fn every_term_possible(cx: &Ctx, em: u32, parent: u32) -> bool {
    let nt = cx.q.terms.len();
    let mut pos = usize::MAX;
    (0..nt).all(|t| {
        let missing = cx.masks[t] & !em;
        if missing == 0
            || (cx.typo[t].is_some() && missing.count_ones() == 1 && em & cx.first[t] != 0)
        {
            return true;
        }
        if pos == usize::MAX {
            pos = cx.tree.pos_of[parent as usize] as usize * nt;
        }
        cx.up[pos + t] > 0
    })
}

/// Directories per parallel work unit in [`path_tables`].
const DIR_CHUNK: usize = 8_192;

/// Per term, the best non-root folder score of every directory and its
/// ancestors (0 = none), interleaved by dir-tree position:
/// `up[pos * terms.len() + t]`. One parallel pass over the directories
/// serves all terms; the letter masks are read in tree order and only
/// directories that pass them touch their names.
/// `viewer`: folders it may not see are not scored (their whole subtree
/// is hidden too), so the time a query takes says nothing about them.
fn path_tables(
    ix: &Index,
    tree: &DirTree,
    terms: &[Term],
    atoms: &[Atom],
    masks: &[u32],
    viewer: Option<Owner>,
) -> Vec<u16> {
    let nt = terms.len();
    let mut up = vec![0u16; tree.order.len() * nt];
    up.par_chunks_mut(DIR_CHUNK * nt)
        .enumerate()
        .for_each(|(c, out)| {
            with_matcher(|m| {
                for (k, slot) in out.chunks_mut(nt).enumerate() {
                    let pos = c * DIR_CHUNK + k;
                    if tree.parent_pos[pos] == NONE {
                        continue; // a drive root is never a match target
                    }
                    if viewer.is_some_and(|v| !visible_to(tree.owner[pos], v)) {
                        continue;
                    }
                    let dmask = tree.mask[pos];
                    for t in 0..nt {
                        if masks[t] & !dmask != 0 {
                            continue;
                        }
                        let id = EntryId(tree.order[pos]);
                        if !ix.is_live(id) {
                            break;
                        }
                        let folded = ix.folded(id);
                        if quick_match(folded, &terms[t]) {
                            slot[t] = m
                                .score_atom(folded, &atoms[t])
                                .map_or(0, |s| s.clamp(1, u16::MAX as u32) as u16);
                        }
                    }
                }
            })
        });
    // parent positions come first, so one forward sweep inherits scores
    for pos in 0..tree.order.len() {
        let pp = tree.parent_pos[pos];
        if pp != NONE {
            let (pp, pos) = (pp as usize * nt, pos * nt);
            for t in 0..nt {
                up[pos + t] = up[pos + t].max(up[pp + t]);
            }
        }
    }
    up
}

fn run_pass(cx: &Ctx) -> (Vec<Key>, u32, bool) {
    cx.floor.store(i64::MIN, Ordering::Relaxed);
    cx.top.lock().unwrap_or_else(|e| e.into_inner()).clear();
    let chunks = cx.ix.raw_len().div_ceil(CHUNK);
    let workers: Vec<Worker> = (0..chunks)
        .into_par_iter()
        .fold(Worker::new, |mut w, c| {
            w.run_chunk(cx, c);
            w
        })
        .collect();
    let cancelled = workers.iter().any(|w| w.cancelled);
    let total = workers.iter().map(|w| w.total).sum();
    let keys = workers
        .into_iter()
        .flat_map(|w| w.heap.into_vec())
        .collect();
    (keys, total, cancelled)
}

fn highlights(
    m: &mut FuzzyMatcher,
    ix: &Index,
    id: EntryId,
    q: &Query,
    typo: bool,
) -> Vec<(u32, u32)> {
    let folded = ix.folded(id);
    let mut pos: Vec<u32> = Vec::new();
    for term in &q.terms {
        match m.match_term(ix, id, term) {
            Some(tm) if tm.in_name => pos.extend(tm.name_positions),
            _ if typo => {
                let found = ApproxFinder::new(term.text.as_bytes())
                    .and_then(|f| f.find(folded.as_bytes(), ix.camel_source(id)));
                if let Some((a, b)) = found {
                    pos.extend(a as u32..b as u32);
                }
            }
            _ => {}
        }
    }
    utf16_ranges(&fold(ix.name(id)), &pos)
}

pub fn search(ix: &Index, q: &Query, opt: &SearchOptions) -> SearchResult {
    let started = Instant::now();
    let done = |hits: Vec<Hit>, total: u32, cancelled: bool| SearchResult {
        hits,
        total_matches: total,
        cancelled,
        elapsed_us: started.elapsed().as_micros().min(u32::MAX as u128) as u32,
    };
    if q.is_empty() || opt.limit == 0 {
        return done(Vec::new(), 0, false);
    }
    if opt.cancel.load(Ordering::Relaxed) {
        return done(Vec::new(), 0, true);
    }

    let tree = ix.dir_tree();
    // phrases are exact by definition: no typo tier
    let typo: Vec<Option<ApproxFinder>> = q
        .terms
        .iter()
        .map(|t| {
            if t.phrase {
                None
            } else {
                ApproxFinder::new(t.text.as_bytes())
            }
        })
        .collect();
    let slack = typo.iter().map(|f| u32::from(f.is_some())).collect();
    let first = q
        .terms
        .iter()
        .map(|t| {
            t.text
                .chars()
                .next()
                .map_or(0, |c| char_mask(c.encode_utf8(&mut [0; 4])))
        })
        .collect();
    let max_scores: Vec<u32> = q.terms.iter().map(max_score).collect();
    let credits: Vec<u32> = q
        .terms
        .iter()
        .map(|t| t.text.chars().count() as u32 * rank::TYPO_CHAR_SCORE)
        .collect();
    let slack_upper = (0..q.terms.len())
        .map(|t| credits[t].max(max_scores[t] / PATH_ONLY_DIVISOR))
        .sum::<u32>() as i64
        * rank::MATCH_SCALE
        + rank::TYPO_PENALTY
        + rank::PREFIX_BONUS
        + rank::RECENCY_MAX as i64
        + rank::MAX_PATH_BONUS
        + opt.boosts.max_bonus();
    let atoms: Vec<Atom> = q.terms.iter().map(atom).collect();
    let masks: Vec<u32> = q.terms.iter().map(|t| char_mask(&t.text)).collect();
    let up = if q.terms.len() >= 2 {
        path_tables(
            ix,
            tree,
            &q.terms,
            &atoms,
            &masks,
            ix.has_owners().then_some(opt.viewer),
        )
    } else {
        Vec::new()
    };
    let cx = Ctx {
        ix,
        cols: ix.columns(),
        tree,
        atoms,
        floor: AtomicI64::new(i64::MIN),
        top: Mutex::new(BinaryHeap::new()),
        masks,
        q,
        opt,
        ext_suffix: q.ext.as_ref().map(|e| format!(".{e}")),
        up,
        typo,
        slack,
        first,
        max_scores,
        credits,
        slack_upper,
        private: ix.has_owners(),
    };
    let (mut keys, mut total, cancelled) = run_pass(&cx);
    if cancelled {
        return done(Vec::new(), 0, true);
    }

    keys.sort_unstable();
    let mut m = FuzzyMatcher::new();
    let hits = keys
        .into_iter()
        .take(opt.limit)
        .map(|Reverse((score, Reverse(id), typo))| {
            let id = EntryId(id);
            Hit {
                id,
                score,
                typo,
                highlights: highlights(&mut m, ix, id, q, typo),
            }
        })
        .collect::<Vec<Hit>>();
    total += hits.iter().filter(|h| h.typo).count() as u32;
    done(hits, total, false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::EntryInput;
    use crate::query::parse;

    #[test]
    fn pruning_keeps_the_exact_top_k() {
        // Oracle: with a limit above the match count nothing is pruned, so
        // its first 50 hits are the true top 50 (same score + id order).
        let words = [
            "rapor", "bütçe", "ışık", "finans", "sunum", "notlar", "node", "deniz", "özet", "aile",
            "ra", "bu", "ekim", "taslak", "yedek",
            // lacks the "a" of "rapro": a typo-only hit the mask gate must keep
            "rpro",
        ];
        let mut ix = Index::new();
        let c = ix.insert(EntryInput {
            name: "C:",
            parent: None,
            is_dir: true,
            last_change: 0,
        });
        let mut dirs = vec![c];
        let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut next = |n: usize| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x % n as u64) as usize
        };
        // only ever the second word: its typo hits never start a name
        let second = ["arsiv", "proje"];
        for i in 0..60_000u32 {
            let name = format!(
                "{}_{}{}",
                words[next(words.len())],
                if next(3) == 0 {
                    second[next(2)]
                } else {
                    words[next(words.len())]
                },
                [".pdf", ".txt", ".xlsx", ""][next(4)]
            );
            let parent = dirs[next(dirs.len())];
            let is_dir = next(8) == 0;
            let id = ix.insert(EntryInput {
                name: &name,
                parent: Some(parent),
                is_dir,
                last_change: 1_700_000_000 + i * 977,
            });
            if is_dir {
                dirs.push(id);
            }
        }
        let cancel = AtomicBool::new(false);
        let none = Boosts::new();
        let mut one = Boosts::new();
        one.add(&ix.path(EntryId(500)), 0.7);
        // without boosts the bounds are tight enough for the typo pruning
        // to engage; with one they are not
        for (boosts, q) in [&none, &one].into_iter().flat_map(|b| {
            [
                "ra",
                "rapor",
                "bu",
                "finans butce",
                "ext:pdf rapor",
                "raporr",
                "rapro",
                "finans rapro",
                "isik ozet",
                "arsvi",
                "proej",
                "e",
            ]
            .map(move |q| (b, q))
        }) {
            let run = |limit| {
                let opt = SearchOptions {
                    limit,
                    now: 1_760_000_000,
                    boosts,
                    cancel: &cancel,
                    viewer: crate::OWNER_NONE,
                };
                search(&ix, &parse(q), &opt)
            };
            let full = run(1_000_000);
            let top = run(50);
            let key = |h: &Hit| (h.id, h.score, h.typo);
            let want: Vec<_> = full.hits.iter().take(50).map(key).collect();
            let got: Vec<_> = top.hits.iter().map(key).collect();
            assert_eq!(got, want, "query {q:?}, boosts: {}", !boosts.is_empty());
            assert!(
                !got.is_empty(),
                "query {q:?} found nothing; the test would be vacuous"
            );
        }
    }

    #[test]
    fn precancelled_search_stops() {
        let mut ix = Index::new();
        let c = ix.insert(EntryInput {
            name: "C:",
            parent: None,
            is_dir: true,
            last_change: 0,
        });
        for i in 0..200_000 {
            let name = format!("file_{i}.txt");
            ix.insert(EntryInput {
                name: &name,
                parent: Some(c),
                is_dir: false,
                last_change: 0,
            });
        }
        let cancel = AtomicBool::new(true);
        let boosts = Boosts::new();
        let opt = SearchOptions {
            limit: 50,
            now: 0,
            boosts: &boosts,
            cancel: &cancel,
            viewer: crate::OWNER_NONE,
        };
        let r = search(&ix, &parse("file"), &opt);
        assert!(r.cancelled);
        assert!(r.hits.is_empty());
    }
}
