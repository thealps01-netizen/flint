//! Score composition: constants, path heuristics, recency, frecency keys.
//!
//! Tests pin orderings, not these values.

use crate::fold::fold_str;
use crate::index::{EntryId, Index};
use std::collections::HashMap;

/// nucleo scores are multiplied by this. One nucleo gap start costs 3
/// points (30 after scaling); recency and the personal-folder raise stay
/// below that, so they only order matches of equal quality.
pub const MATCH_SCALE: i64 = 10;
/// A typo hit: each corrected term is credited `TYPO_CHAR_SCORE` per char
/// (a plain match without bonuses), and the hit pays this once. A clean
/// match of the same name ranks first; a typo hit still beats a weak,
/// gappy fuzzy match.
pub const TYPO_PENALTY: i64 = -300;
pub const TYPO_CHAR_SCORE: u32 = 16;
/// The first term is an exact prefix of the folded name.
pub const PREFIX_BONUS: i64 = 2_000;
/// Multiplied by the app-supplied frecency weight in [0, 1].
pub const FRECENCY_SCALE: f32 = 3_000.0;
/// Recency bonus for a file changed right now, decaying over ~30 days;
/// a tie-breaker between equal matches (see `MATCH_SCALE`).
pub const RECENCY_MAX: f32 = 14.0;
pub const RECENCY_DAYS: f32 = 30.0;
/// An ancestor folder is noise (caches, package stores, system). This
/// one deliberately outweighs match quality: such files are rarely wanted.
pub const PATH_DEMOTE: i64 = -1_500;
/// An ancestor folder is where people keep their own files; a
/// tie-breaker like recency.
pub const PATH_RAISE: i64 = 14;
/// Largest score `path_adjust` can add.
pub const MAX_PATH_BONUS: i64 = PATH_RAISE;

const DEMOTE: &[&str] = &[
    "appdata",
    "node_modules",
    "$recycle.bin",
    "winsxs",
    ".git",
    "__pycache__",
    "temp",
    "tmp",
    "package cache",
];
const RAISE: &[&str] = &["desktop", "documents", "downloads"];

/// FNV-1a 64.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Frecency weights in [0, 1] keyed by folded full path.
#[derive(Default)]
pub struct Boosts {
    map: HashMap<u64, f32>,
    /// Folded leaf name -> highest weight among boosts with that name, so
    /// search bounds each entry by its own name instead of the global max
    /// and builds full paths only for entries whose name was boosted.
    leaf: HashMap<u64, f32>,
    max: f32,
}

impl Boosts {
    pub fn new() -> Self {
        Self::default()
    }

    /// `full_path` uses `\` separators, e.g. `C:\Users\x\a.txt`.
    pub fn add(&mut self, full_path: &str, weight: f32) {
        let w = weight.clamp(0.0, 1.0);
        let folded = fold_str(full_path);
        let leaf = folded.rsplit('\\').next().unwrap_or(&folded);
        let lw = self.leaf.entry(fnv1a(leaf.as_bytes())).or_insert(0.0);
        *lw = lw.max(w);
        self.map.insert(fnv1a(folded.as_bytes()), w);
        self.max = self.max.max(w);
    }

    pub fn path_key(full_path: &str) -> u64 {
        fnv1a(fold_str(full_path).as_bytes())
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Largest score any boost can add.
    pub fn max_bonus(&self) -> i64 {
        (self.max * FRECENCY_SCALE) as i64
    }

    /// Largest score a boost can add to an entry with this folded name.
    pub fn leaf_bonus(&self, folded_name: &str) -> i64 {
        if self.leaf.is_empty() {
            return 0;
        }
        let w = self.leaf.get(&fnv1a(folded_name.as_bytes())).copied();
        (w.unwrap_or(0.0) * FRECENCY_SCALE) as i64
    }

    fn weight(&self, key: u64) -> f32 {
        self.map.get(&key).copied().unwrap_or(0.0)
    }
}

pub fn entry_key(ix: &Index, id: EntryId) -> u64 {
    Boosts::path_key(&ix.path(id))
}

pub fn frecency(ix: &Index, id: EntryId, boosts: &Boosts) -> i64 {
    if boosts.leaf_bonus(ix.folded(id)) == 0 {
        return 0; // no boost has this name: skip building the path
    }
    (boosts.weight(entry_key(ix, id)) * FRECENCY_SCALE) as i64
}

/// A change in the future (clock skew) counts as now.
pub fn recency(last_change: u32, now: u32) -> i64 {
    let age_days = now.saturating_sub(last_change) as f32 / 86_400.0;
    (RECENCY_MAX * (-age_days / RECENCY_DAYS).exp()) as i64
}

/// Class of one folded folder name: -1 noise, 1 personal, 0 neutral.
/// A hidden dot folder (".vscode", ".cargo", ".cache") holds tools' files,
/// so it is noise like AppData.
pub fn name_class(folded: &str) -> i8 {
    let dot_folder = folded.len() > 1 && folded.starts_with('.') && folded != "..";
    if dot_folder || DEMOTE.contains(&folded) {
        -1
    } else if RAISE.contains(&folded) {
        1
    } else {
        0
    }
}

/// O(1): the class of every directory is cached in the index's dir tree.
pub fn path_adjust(ix: &Index, id: EntryId) -> i64 {
    match ix.path_class(id) {
        c if c < 0 => PATH_DEMOTE,
        c if c > 0 => PATH_RAISE,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Folders that hold tools' and caches' files, not the user's own:
    /// hidden dot folders (.vscode, .cargo, .cache), "tmp" and
    /// "Package Cache" are noise like AppData and Temp.
    #[test]
    fn tool_and_cache_folders_are_noise() {
        for n in [
            ".vscode",
            ".cargo",
            ".cache",
            ".nuget",
            ".llmwiki-audit",
            "tmp",
            "package cache",
        ] {
            assert_eq!(name_class(n), -1, "{n}");
        }
        for n in ["projects", "rapor", "documents", ".", "..", "dev"] {
            assert_ne!(name_class(n), -1, "{n}");
        }
    }
    #[test]
    fn path_key_folds() {
        assert_eq!(
            Boosts::path_key(r"C:\Users\Bütçe.xlsx"),
            Boosts::path_key(r"c:\users\BUTCE.XLSX")
        );
    }
    /// An entry whose name was boosted gets the bound and the boost even
    /// when the boost path is spelled with other case, accents or NFD;
    /// an entry with another name gets neither.
    #[test]
    fn boost_reaches_entry_through_leaf_table() {
        let mut ix = Index::new();
        let c = ix.insert(crate::EntryInput {
            name: "C:",
            parent: None,
            is_dir: true,
            last_change: 0,
        });
        let u = ix.insert(crate::EntryInput {
            name: "Users",
            parent: Some(c),
            is_dir: true,
            last_change: 0,
        });
        let hit = ix.insert(crate::EntryInput {
            name: "Işık Cafe\u{301}.txt",
            parent: Some(u),
            is_dir: false,
            last_change: 0,
        });
        let other = ix.insert(crate::EntryInput {
            name: "other.txt",
            parent: Some(u),
            is_dir: false,
            last_change: 0,
        });
        let mut b = Boosts::new();
        b.add(r"c:\USERS\IŞIK Café.txt", 0.5);
        assert_eq!(b.leaf_bonus(ix.folded(hit)), 1_500);
        assert_eq!(frecency(&ix, hit, &b), 1_500);
        assert_eq!(b.leaf_bonus(ix.folded(other)), 0);
        assert_eq!(frecency(&ix, other, &b), 0);
        // same name in another folder: bound allows it, the full path decides
        let d = ix.insert(crate::EntryInput {
            name: "D:",
            parent: None,
            is_dir: true,
            last_change: 0,
        });
        let twin = ix.insert(crate::EntryInput {
            name: "ışık café.txt",
            parent: Some(d),
            is_dir: false,
            last_change: 0,
        });
        assert_eq!(b.leaf_bonus(ix.folded(twin)), 1_500);
        assert_eq!(frecency(&ix, twin, &b), 0);
    }
    #[test]
    fn recency_decays() {
        assert_eq!(recency(100, 100), RECENCY_MAX as i64);
        assert!(recency(0, 86_400 * 60) < recency(0, 86_400));
        assert_eq!(recency(200, 100), RECENCY_MAX as i64);
    }
}
