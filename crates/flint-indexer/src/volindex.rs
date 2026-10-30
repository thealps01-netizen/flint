//! One NTFS volume inside the shared `Index`: maps file reference numbers
//! (FRNs) to entry ids, builds the volume from MFT-enumeration records in
//! any order, and applies change-journal records. Pure: no OS calls.

use crate::cut::{Cut, Depth, Place, Rule};
use crate::usn::{
    USN_REASON_FILE_DELETE, USN_REASON_RENAME_NEW_NAME, USN_REASON_RENAME_OLD_NAME, UsnRecord,
};
use flint_core::{EntryId, EntryInput, Index};
use std::collections::HashMap;

/// FRNs are local file-system numbers, not attacker-chosen keys, so a
/// multiply-shift hash replaces SipHash (the map is hit for every entry
/// of a scan and every journal record).
#[derive(Default, Clone, Copy)]
pub struct FrnHasher(u64);

impl std::hash::Hasher for FrnHasher {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0.rotate_left(8) ^ b as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        }
    }
    fn write_u64(&mut self, n: u64) {
        let x = (self.0 ^ n).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        self.0 = x ^ (x >> 29);
    }
    fn write_u32(&mut self, n: u32) {
        self.write_u64(n as u64);
    }
}

pub type FrnMap<K, V> = HashMap<K, V, std::hash::BuildHasherDefault<FrnHasher>>;

/// Folder under the volume root that holds records whose parent is
/// unknown or in a cycle. The privacy rules hide it from everyone: such a
/// record may come from any user's folder.
pub const ORPHANS: &str = "$Flint.Orphans";

/// Parent FRN [`VolumeIndex::entries`] reports for an entry in
/// [`ORPHANS`]: matches no record, so a rebuild puts it there again.
pub const ORPHAN_FRN: u64 = u64::MAX - 1;

pub struct VolumeIndex {
    root_frn: u64,
    root: EntryId,
    /// The [`ORPHANS`] folder, created on first use.
    orphans: Option<EntryId>,
    map: FrnMap<u64, EntryId>,
    /// FRN of each entry id this volume owns (`u64::MAX` = not ours/dead).
    frn_of: Vec<u64>,
    /// The folders whose contents are left out.
    cut: Cut,
}

const NO_FRN: u64 = u64::MAX;

/// What an index image keeps of a volume besides the entries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VolumeParts {
    pub root_frn: u64,
    pub root: EntryId,
    pub orphans: Option<EntryId>,
    /// (entry id, FRN) of every entry the volume owns.
    pub frns: Vec<(u32, u64)>,
    /// The rules the volume was read with, its skipped folders (FRN,
    /// depth) and its hidden ones.
    pub rules: Vec<Rule>,
    pub cut: Vec<(u64, Depth)>,
    pub hidden: Vec<u64>,
}

impl VolumeIndex {
    /// What [`VolumeIndex::from_parts`] needs to rebuild this volume over
    /// the same index.
    pub fn parts(&self) -> VolumeParts {
        let mut frns: Vec<(u32, u64)> = self
            .frn_of
            .iter()
            .enumerate()
            .filter(|&(_, &f)| f != NO_FRN)
            .map(|(i, &f)| (i as u32, f))
            .collect();
        frns.sort_unstable();
        let (cut, hidden) = self.cut.parts();
        VolumeParts {
            root_frn: self.root_frn,
            root: self.root,
            orphans: self.orphans,
            frns,
            rules: self.cut.rules().to_vec(),
            cut,
            hidden,
        }
    }

    /// The volume over `ix` (restored from the same image). `None` when
    /// the parts do not fit `ix` (an id outside it, a root that is not a
    /// live folder, an FRN bound twice).
    pub fn from_parts(ix: &Index, p: VolumeParts) -> Option<VolumeIndex> {
        let n = ix.raw_len();
        let ok = |id: EntryId| (id.0 as usize) < n && ix.is_live(id) && ix.is_dir(id);
        if !ok(p.root) || p.orphans.is_some_and(|o| !ok(o)) {
            return None;
        }
        let mut v = VolumeIndex {
            root_frn: p.root_frn,
            root: p.root,
            orphans: p.orphans,
            map: FrnMap::with_capacity_and_hasher(p.frns.len(), Default::default()),
            frn_of: vec![NO_FRN; n],
            cut: Cut::from_parts(p.rules, &p.cut, &p.hidden),
        };
        for (id, frn) in p.frns {
            if id as usize >= n || frn == NO_FRN || v.map.insert(frn, EntryId(id)).is_some() {
                return None;
            }
            v.frn_of[id as usize] = frn;
        }
        (v.map.get(&p.root_frn) == Some(&p.root)).then_some(v)
    }

    /// Adds the volume root `drive` ("C:") and every record to `ix`.
    /// Records may come in any order (MFT order puts many children before
    /// their parents); a record whose parent never appears, or that sits
    /// in a parent cycle, goes into [`ORPHANS`].
    pub fn build(
        ix: &mut Index,
        drive: &str,
        root_frn: u64,
        records: impl IntoIterator<Item = UsnRecord>,
    ) -> VolumeIndex {
        let root = ix.insert(EntryInput {
            name: drive,
            parent: None,
            is_dir: true,
            last_change: 0,
        });
        let records = records.into_iter();
        let mut v = VolumeIndex {
            root_frn,
            root,
            orphans: None,
            map: FrnMap::with_capacity_and_hasher(records.size_hint().0 + 1, Default::default()),
            frn_of: Vec::with_capacity(ix.raw_len() + records.size_hint().0),
            cut: Cut::default(),
        };
        v.bind(root_frn, root);
        ix.reserve(records.size_hint().0);
        // fast path: a record whose parent is already placed (a snapshot
        // lists parents first) goes in at once; the rest wait
        let mut recs: Vec<UsnRecord> = Vec::new();
        for r in records {
            if r.frn == root_frn {
                continue;
            }
            let parent = if r.parent == root_frn {
                Some(root)
            } else {
                v.map.get(&r.parent).copied()
            };
            match parent {
                Some(p) if !v.map.contains_key(&r.frn) => {
                    v.insert(ix, &r, p);
                }
                _ => recs.push(r),
            }
        }
        // a repeated FRN keeps its last record
        let (repeats, recs): (Vec<UsnRecord>, Vec<UsnRecord>) =
            recs.into_iter().partition(|r| v.map.contains_key(&r.frn));
        // record index by FRN; a repeated FRN keeps its last record
        let mut at: FrnMap<u64, u32> =
            FrnMap::with_capacity_and_hasher(recs.len(), Default::default());
        for (i, r) in recs.iter().enumerate() {
            at.insert(r.frn, i as u32);
        }
        const UNPLACED: u32 = u32::MAX;
        let mut placed = vec![UNPLACED; recs.len()];
        let mut chain: Vec<usize> = Vec::new();
        for i in 0..recs.len() {
            if placed[i] != UNPLACED || at[&recs[i].frn] != i as u32 {
                continue;
            }
            // walk up to a placed ancestor, then insert top-down
            let mut cur = i;
            let base = loop {
                chain.push(cur);
                let p = recs[cur].parent;
                if p == root_frn {
                    break root;
                }
                if let Some(&id) = v.map.get(&p) {
                    break id;
                }
                match at.get(&p) {
                    None => break v.orphans(ix),
                    Some(&j) if placed[j as usize] != UNPLACED => {
                        break EntryId(placed[j as usize]);
                    }
                    Some(&j) if chain.contains(&(j as usize)) => break v.orphans(ix), // parent cycle
                    Some(&j) => cur = j as usize,
                }
            };
            let mut parent = base;
            while let Some(k) = chain.pop() {
                parent = v.insert(ix, &recs[k], parent);
                placed[k] = parent.0;
            }
        }
        for r in &repeats {
            let parent = match v.id(r.parent) {
                Some(p) => p,
                None => v.orphans(ix),
            };
            if let Some(id) = v.id(r.frn)
                && id != parent
                && !ix.ancestors(parent).any(|a| a == id)
            {
                ix.rename(id, &r.name, Some(parent));
                ix.touch(id, r.time);
            }
        }
        v
    }

    fn orphans(&mut self, ix: &mut Index) -> EntryId {
        if let Some(o) = self.orphans.filter(|&o| ix.is_live(o)) {
            return o;
        }
        let o = ix.insert(EntryInput {
            name: ORPHANS,
            parent: Some(self.root),
            is_dir: true,
            last_change: 0,
        });
        self.orphans = Some(o);
        o
    }

    fn bind(&mut self, frn: u64, id: EntryId) {
        let i = id.0 as usize;
        if self.frn_of.len() <= i {
            self.frn_of.resize(i + 1, NO_FRN);
        }
        self.frn_of[i] = frn;
        self.map.insert(frn, id);
    }

    fn insert(&mut self, ix: &mut Index, r: &UsnRecord, parent: EntryId) -> EntryId {
        let id = ix.insert(EntryInput {
            name: &r.name,
            parent: Some(parent),
            is_dir: r.is_dir(),
            last_change: r.time,
        });
        self.bind(r.frn, id);
        id
    }

    fn unbind(&mut self, ix: &mut Index, frn: u64) {
        if let Some(id) = self.map.remove(&frn) {
            ix.remove(id);
            self.frn_of[id.0 as usize] = NO_FRN;
        }
    }

    /// The folders whose contents the volume leaves out.
    pub fn cut(&self) -> &Cut {
        &self.cut
    }

    /// Sets what the volume leaves out (read with [`Cut::filter`]).
    pub fn set_cut(&mut self, cut: Cut) {
        self.cut = cut;
    }

    /// Whether a change left the volume's contents unknown (a skipped
    /// folder moved, or an indexed one became skipped): it needs a new
    /// scan. Cleared by the call.
    pub fn take_stale(&mut self) -> bool {
        self.cut.take_stale()
    }

    /// The folders that came out of skipped ones since the last call:
    /// their contents were never read (the caller reads and applies
    /// them). Cleared by the call.
    pub fn take_surfaced(&mut self) -> Vec<u64> {
        self.cut.take_surfaced()
    }

    /// What the volume holds is not known any more (see
    /// [`VolumeIndex::take_stale`]).
    pub fn mark_stale(&mut self) {
        self.cut.mark_stale();
    }

    /// Removes `id` and everything below it; the folders among them
    /// become hidden (moved into a skipped folder).
    fn hide_tree(&mut self, ix: &mut Index, frn: u64, id: EntryId) {
        if ix.child_count(id) == 0 {
            self.unbind(ix, frn);
            return;
        }
        // whether a folder is inside `id`, decided once per folder
        let mut known: HashMap<u32, bool> = HashMap::from([(id.0, true)]);
        let mut chain: Vec<u32> = Vec::new();
        let mut inside_of = |x: EntryId| -> bool {
            let mut cur = ix.parent(x);
            let found = loop {
                match cur {
                    None => break false,
                    Some(p) => {
                        if let Some(&b) = known.get(&p.0) {
                            break b;
                        }
                        chain.push(p.0);
                        cur = ix.parent(p);
                    }
                }
            };
            for c in chain.drain(..) {
                known.insert(c, found);
            }
            found
        };
        let inside: Vec<u64> = self
            .map
            .iter()
            .filter(|&(_, &x)| x != id && inside_of(x))
            .map(|(&f, _)| f)
            .collect();
        for f in inside {
            if self.id(f).is_some_and(|x| ix.is_dir(x)) {
                self.cut.hide(f);
            }
            self.unbind(ix, f);
        }
        self.unbind(ix, frn);
    }

    /// The names from below the root to `parent`, then `name`, lowercase.
    fn path_below_root(&self, ix: &Index, parent: EntryId, name: &str) -> Vec<String> {
        let mut path: Vec<String> = std::iter::once(parent)
            .chain(ix.ancestors(parent))
            .take_while(|&a| a != self.root)
            .map(|a| ix.name(a).to_lowercase())
            .collect();
        path.reverse();
        path.push(name.to_lowercase());
        path
    }

    /// Applies one change-journal record.
    pub fn apply(&mut self, ix: &mut Index, rec: &UsnRecord) {
        if rec.frn == self.root_frn {
            return;
        }
        if rec.reason & USN_REASON_FILE_DELETE != 0 {
            if self.cut.forget(rec.frn) {
                return; // inside a skipped folder: never indexed
            }
            let Some(id) = self.id(rec.frn) else { return };
            if ix.is_dir(id) && ix.child_count(id) > 0 {
                // Windows deletes children first; a missed record must not
                // leave live entries under a dead folder
                let inside: Vec<u64> = self
                    .map
                    .iter()
                    .filter(|&(_, &x)| x != id && ix.ancestors(x).any(|a| a == id))
                    .map(|(&f, _)| f)
                    .collect();
                for f in inside {
                    self.unbind(ix, f);
                }
            }
            self.unbind(ix, rec.frn);
            return;
        }
        if rec.reason & USN_REASON_RENAME_OLD_NAME != 0
            && rec.reason & USN_REASON_RENAME_NEW_NAME == 0
        {
            return; // the old name; the new-name record follows
        }
        match self.cut.place(rec.parent, &rec.name, rec.is_dir()) {
            Place::Hidden => {
                // made in, or moved into, a skipped folder
                if let Some(id) = self.id(rec.frn) {
                    self.hide_tree(ix, rec.frn, id);
                }
                if rec.is_dir() {
                    self.cut.hide(rec.frn);
                }
                return;
            }
            Place::Cut(depth) if rec.is_dir() => self.cut_folder(ix, rec.frn, depth),
            Place::Shown if rec.is_dir() && self.cut.may_name(&rec.name) => {
                if let Some(p) = self.id(rec.parent) {
                    let path = self.path_below_root(ix, p, &rec.name);
                    if let Some(depth) = self.cut.rule_depth(&path) {
                        self.cut_folder(ix, rec.frn, depth);
                    }
                }
            }
            _ => {}
        }
        // out of a skipped folder: what it holds was never read
        self.cut.unhide(rec.frn);
        let parent = match self.id(rec.parent) {
            Some(p) => p,
            None => self.orphans(ix),
        };
        match self.id(rec.frn) {
            Some(id) => {
                // a move under its own subtree can only come from a stale
                // view (moves made during the scan window, replayed in another
                // order): park it in the hidden orphans folder until a later
                // record places it, never build a parent cycle
                let parent = if parent == id || ix.ancestors(parent).any(|a| a == id) {
                    self.orphans(ix)
                } else {
                    parent
                };
                if ix.name(id) != rec.name || ix.parent(id) != Some(parent) {
                    ix.rename(id, &rec.name, Some(parent));
                    if rec.is_dir() {
                        self.recheck_moved(ix, rec.frn, id);
                    }
                }
                if rec.time != 0 {
                    ix.touch(id, rec.time);
                }
            }
            None => {
                self.insert(ix, rec, parent);
            }
        }
    }

    /// `frn` becomes a skipped folder at `depth`. One already indexed
    /// with contents, or skipped at another depth, leaves the volume
    /// stale (what it should hold now was not read).
    fn cut_folder(&mut self, ix: &Index, frn: u64, depth: Depth) {
        let was = self.cut.depth(frn);
        if was != Some(depth) {
            if was.is_some() || self.id(frn).is_some_and(|id| ix.child_count(id) > 0) {
                self.cut.mark_stale();
            }
            self.cut.set_cut(frn, depth);
        }
    }

    /// Folder `id` was renamed or moved: the volume is stale when that
    /// changes what is skipped. That is when it is skipped and no longer
    /// where its rule puts it, when a skipped folder lies inside it, or
    /// when a rule now leads through it.
    fn recheck_moved(&mut self, ix: &Index, frn: u64, id: EntryId) {
        let path = match ix.parent(id) {
            Some(p) => self.path_below_root(ix, p, ix.name(id)),
            None => return,
        };
        let stale = if let Some(depth) = self.cut.depth(frn) {
            let parent_frn = ix
                .parent(id)
                .and_then(|p| self.frn(p))
                .unwrap_or(self.root_frn);
            let now = match self.cut.place(parent_frn, ix.name(id), true) {
                Place::Cut(d) => Some(d),
                _ => self.cut.rule_depth(&path),
            };
            now != Some(depth)
        } else {
            self.cut
                .skipped()
                .filter_map(|f| self.id(f))
                .any(|s| ix.ancestors(s).any(|a| a == id))
                || (ix.child_count(id) > 0 && self.cut.rule_prefix(&path))
        };
        if stale {
            self.cut.mark_stale();
        }
    }

    pub fn root(&self) -> EntryId {
        self.root
    }

    pub fn root_frn(&self) -> u64 {
        self.root_frn
    }

    pub fn frn(&self, id: EntryId) -> Option<u64> {
        self.frn_of
            .get(id.0 as usize)
            .copied()
            .filter(|&f| f != NO_FRN)
    }

    pub fn id(&self, frn: u64) -> Option<EntryId> {
        self.map.get(&frn).copied()
    }

    /// Mapped entries, the root included.
    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// `(frn, parent frn, id)` of every live entry of this volume except
    /// the root, parents before children.
    pub fn entries<'a>(&'a self, ix: &'a Index) -> impl Iterator<Item = (u64, u64, EntryId)> + 'a {
        let mut all: Vec<(usize, EntryId, u64)> = self
            .map
            .iter()
            .filter(|&(&f, _)| f != self.root_frn)
            .map(|(&f, &id)| (ix.ancestors(id).count(), id, f))
            .collect();
        all.sort_unstable_by_key(|&(depth, id, _)| (depth, id.0));
        all.into_iter().map(move |(_, id, f)| {
            // only the orphans folder has no FRN
            let parent = ix
                .parent(id)
                .map_or(self.root_frn, |p| self.frn(p).unwrap_or(ORPHAN_FRN));
            (f, parent, id)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usn::{FILE_ATTRIBUTE_DIRECTORY, USN_REASON_CLOSE, USN_REASON_FILE_CREATE};

    const ROOT: u64 = 5;

    fn rec(frn: u64, parent: u64, name: &str, dir: bool, reason: u32) -> UsnRecord {
        UsnRecord {
            frn,
            parent,
            usn: 0,
            time: 1000,
            reason,
            attrs: if dir { FILE_ATTRIBUTE_DIRECTORY } else { 0x20 },
            security_id: 0,
            name: name.into(),
        }
    }
    fn d(frn: u64, parent: u64, name: &str) -> UsnRecord {
        rec(frn, parent, name, true, 0)
    }
    fn f(frn: u64, parent: u64, name: &str) -> UsnRecord {
        rec(frn, parent, name, false, 0)
    }
    fn path(ix: &Index, v: &VolumeIndex, frn: u64) -> String {
        ix.path(v.id(frn).expect("mapped"))
    }

    /// C:\Users\deniz\AppData\{Local,Roaming\Microsoft\Windows\Start Menu}
    /// and C:\Users\deniz\Documents, read with the app data rules.
    fn skipped() -> (Index, VolumeIndex) {
        use crate::cut::{Cut, Rule};
        let rules = vec![
            Rule {
                path: vec!["users".into(), "*".into(), "appdata".into(), "local".into()],
                depth: 3,
            },
            Rule {
                path: vec![
                    "users".into(),
                    "*".into(),
                    "appdata".into(),
                    "roaming".into(),
                ],
                depth: 0,
            },
        ];
        let mut recs = vec![
            d(10, ROOT, "Users"),
            d(11, 10, "deniz"),
            d(12, 11, "AppData"),
            d(13, 12, "Local"),
            d(14, 12, "Roaming"),
            d(15, 13, "Temp"),
            f(16, 15, "a.tmp"),
            d(17, 14, "Microsoft"),
            d(18, 17, "Windows"),
            d(19, 18, "Start Menu"),
            f(20, 19, "Discord.lnk"),
            d(21, 11, "Documents"),
            f(22, 21, "rapor.txt"),
        ];
        let mut cut = Cut::new(rules);
        cut.filter(ROOT, &mut recs);
        let mut ix = Index::new();
        let mut v = VolumeIndex::build(&mut ix, "C:", ROOT, recs);
        v.set_cut(cut);
        (ix, v)
    }

    fn live_paths(ix: &Index) -> Vec<String> {
        let mut p: Vec<String> = (0..ix.raw_len() as u32)
            .map(EntryId)
            .filter(|&id| ix.is_live(id))
            .map(|id| ix.path(id))
            .collect();
        p.sort();
        p
    }

    #[test]
    fn a_skipped_folder_takes_no_new_entries() {
        let (mut ix, mut v) = skipped();
        let before = live_paths(&ix);
        assert!(!before.iter().any(|p| p.contains("Temp")));
        assert!(before.contains(
            &r"C:\Users\deniz\AppData\Roaming\Microsoft\Windows\Start Menu\Discord.lnk".to_string()
        ));
        v.apply(&mut ix, &rec(30, 13, "Cache", true, USN_REASON_FILE_CREATE));
        v.apply(
            &mut ix,
            &rec(31, 30, "x.bin", false, USN_REASON_FILE_CREATE),
        );
        v.apply(
            &mut ix,
            &rec(32, 15, "b.tmp", false, USN_REASON_FILE_CREATE),
        );
        v.apply(&mut ix, &rec(33, 14, "Code", true, USN_REASON_FILE_CREATE));
        v.apply(
            &mut ix,
            &rec(34, 19, "Yeni.lnk", false, USN_REASON_FILE_CREATE),
        );
        let after = live_paths(&ix);
        let added: Vec<&String> = after.iter().filter(|p| !before.contains(p)).collect();
        assert_eq!(
            added,
            [r"C:\Users\deniz\AppData\Roaming\Microsoft\Windows\Start Menu\Yeni.lnk"]
        );
        assert!(v.cut().hides(30) && v.cut().hides(33));
        assert!(!v.take_stale());
    }

    #[test]
    fn moving_into_a_skipped_folder_hides_and_out_of_one_is_read() {
        let (mut ix, mut v) = skipped();
        v.apply(&mut ix, &rec(40, 21, "proje", true, USN_REASON_FILE_CREATE));
        v.apply(
            &mut ix,
            &rec(41, 40, "kod.rs", false, USN_REASON_FILE_CREATE),
        );
        v.apply(
            &mut ix,
            &rec(40, 13, "proje", true, USN_REASON_RENAME_NEW_NAME),
        );
        assert!(
            !live_paths(&ix)
                .iter()
                .any(|p| p.contains("proje") || p.contains("kod.rs"))
        );
        assert!(v.id(40).is_none() && v.id(41).is_none() && v.cut().hides(40));
        assert!(v.take_surfaced().is_empty() && !v.take_stale());
        // and back out: its contents were not followed meanwhile, so they
        // are to be read
        v.apply(
            &mut ix,
            &rec(40, 21, "proje", true, USN_REASON_RENAME_NEW_NAME),
        );
        assert!(live_paths(&ix).contains(&r"C:\Users\deniz\Documents\proje".to_string()));
        assert_eq!(v.take_surfaced(), [40]);
        assert!(v.take_surfaced().is_empty() && !v.take_stale());
        // a file out of one is simply new
        v.apply(
            &mut ix,
            &rec(16, 21, "a.tmp", false, USN_REASON_RENAME_NEW_NAME),
        );
        assert!(live_paths(&ix).contains(&r"C:\Users\deniz\Documents\a.tmp".to_string()));
        assert!(v.take_surfaced().is_empty() && !v.take_stale());
        // one file moved in is gone again
        v.apply(
            &mut ix,
            &rec(16, 15, "a.tmp", false, USN_REASON_RENAME_NEW_NAME),
        );
        assert!(v.id(16).is_none());
    }

    #[test]
    fn a_tree_moved_into_a_skipped_folder_leaves_whole() {
        let (mut ix, mut v) = skipped();
        let before = live_paths(&ix);
        for (frn, parent, name, dir) in [
            (80, 21, "oyun", true),
            (81, 80, "kayit", true),
            (82, 81, "derin", true),
            (83, 82, "a.sav", false),
            (84, 80, "b.txt", false),
        ] {
            v.apply(
                &mut ix,
                &rec(frn, parent, name, dir, USN_REASON_FILE_CREATE),
            );
        }
        assert_eq!(live_paths(&ix).len(), before.len() + 5);
        v.apply(
            &mut ix,
            &rec(80, 13, "oyun", true, USN_REASON_RENAME_NEW_NAME),
        );
        assert_eq!(live_paths(&ix), before);
        for f in [80, 81, 82, 83, 84] {
            assert!(v.id(f).is_none(), "{f}");
        }
        // its folders are known as hidden, its files are not tracked
        assert!(v.cut().hides(80) && v.cut().hides(81) && v.cut().hides(82));
        assert!(!v.cut().hides(83));
        assert!(!v.take_stale());
    }

    #[test]
    fn a_new_profile_gets_its_app_data_skipped() {
        let (mut ix, mut v) = skipped();
        for (frn, parent, name) in [
            (50, 10, "bob"),
            (51, 50, "AppData"),
            (52, 51, "LOCAL"),
            (53, 52, "Temp"),
        ] {
            v.apply(
                &mut ix,
                &rec(frn, parent, name, true, USN_REASON_FILE_CREATE),
            );
        }
        v.apply(
            &mut ix,
            &rec(54, 53, "c.tmp", false, USN_REASON_FILE_CREATE),
        );
        let p = live_paths(&ix);
        assert!(p.contains(&r"C:\Users\bob\AppData\LOCAL".to_string()));
        assert!(!p.iter().any(|p| p.contains("Temp") || p.contains("c.tmp")));
        assert_eq!(v.cut().depth(52), Some(3));
        assert!(!v.take_stale());
    }

    #[test]
    fn a_folder_with_contents_becoming_skipped_is_stale() {
        let (mut ix, mut v) = skipped();
        v.apply(&mut ix, &rec(60, 21, "Local", true, USN_REASON_FILE_CREATE));
        v.apply(
            &mut ix,
            &rec(61, 60, "x.txt", false, USN_REASON_FILE_CREATE),
        );
        assert!(!v.take_stale());
        // the first Local moves away: what it held is now to be shown
        v.apply(
            &mut ix,
            &rec(13, 21, "Old", true, USN_REASON_RENAME_NEW_NAME),
        );
        assert!(v.take_stale());
        v.apply(
            &mut ix,
            &rec(60, 12, "Local", true, USN_REASON_RENAME_NEW_NAME),
        );
        assert_eq!(v.cut().depth(60), Some(3));
        assert!(v.take_stale());
    }

    #[test]
    fn moves_that_change_what_is_skipped_are_stale() {
        // a skipped folder renamed in place
        let (mut ix, mut v) = skipped();
        v.apply(
            &mut ix,
            &rec(13, 12, "Local.old", true, USN_REASON_RENAME_NEW_NAME),
        );
        assert!(v.take_stale());
        // ... or to another skipped name with another depth
        let (mut ix, mut v) = skipped();
        v.apply(
            &mut ix,
            &rec(14, 12, "Roaming2", true, USN_REASON_RENAME_NEW_NAME),
        );
        assert!(v.take_stale());
        v.apply(
            &mut ix,
            &rec(13, 12, "Roaming", true, USN_REASON_RENAME_NEW_NAME),
        );
        assert_eq!(v.cut().depth(13), Some(0));
        assert!(v.take_stale());
        // a profile moved out of the profiles folder holds skipped folders
        let (mut ix, mut v) = skipped();
        v.apply(
            &mut ix,
            &rec(70, ROOT, "Backup", true, USN_REASON_FILE_CREATE),
        );
        assert!(!v.take_stale());
        v.apply(
            &mut ix,
            &rec(11, 70, "deniz", true, USN_REASON_RENAME_NEW_NAME),
        );
        assert!(v.take_stale());
        // a folder with contents moved to where a rule leads through it
        let (mut ix, mut v) = skipped();
        v.apply(&mut ix, &rec(71, 21, "eski", true, USN_REASON_FILE_CREATE));
        v.apply(
            &mut ix,
            &rec(72, 71, "AppData", true, USN_REASON_FILE_CREATE),
        );
        assert!(!v.take_stale());
        v.apply(
            &mut ix,
            &rec(71, 10, "eski", true, USN_REASON_RENAME_NEW_NAME),
        );
        assert!(v.take_stale());
        // plain renames, and records on a skipped folder in place, are not
        let (mut ix, mut v) = skipped();
        v.apply(
            &mut ix,
            &rec(21, 11, "Belgeler", true, USN_REASON_RENAME_NEW_NAME),
        );
        v.apply(
            &mut ix,
            &rec(17, 14, "Microsoft", true, USN_REASON_FILE_CREATE),
        );
        v.apply(&mut ix, &rec(13, 12, "Local", true, USN_REASON_CLOSE));
        assert!(!v.take_stale());
    }

    #[test]
    fn deleting_inside_a_skipped_folder_is_forgotten() {
        let (mut ix, mut v) = skipped();
        let before = live_paths(&ix);
        assert!(v.cut().hides(15));
        v.apply(
            &mut ix,
            &rec(16, 15, "a.tmp", false, USN_REASON_FILE_DELETE),
        );
        v.apply(&mut ix, &rec(15, 13, "Temp", true, USN_REASON_FILE_DELETE));
        assert!(!v.cut().hides(15));
        assert_eq!(live_paths(&ix), before);
    }

    #[test]
    fn parts_keep_what_is_skipped() {
        let (ix, v) = skipped();
        let p = v.parts();
        assert_eq!(p.rules, v.cut().rules());
        assert_eq!((p.cut.clone(), p.hidden.clone()), v.cut().parts());
        let back = VolumeIndex::from_parts(&ix, p).unwrap();
        assert_eq!(back.cut(), v.cut());
    }

    #[test]
    fn children_before_parent() {
        let mut ix = Index::new();
        let v = VolumeIndex::build(
            &mut ix,
            "C:",
            ROOT,
            [
                f(30, 20, "file.txt"),
                d(21, 20, "sub"),
                f(31, 21, "deep.pdf"),
                d(20, ROOT, "dir"),
            ],
        );
        assert_eq!(path(&ix, &v, 30), r"C:\dir\file.txt");
        assert_eq!(path(&ix, &v, 31), r"C:\dir\sub\deep.pdf");
        assert_eq!(ix.live_len(), 5);
        assert_eq!(v.frn(v.id(31).unwrap()), Some(31));
        assert_eq!(ix.path(v.root()), r"C:\");
    }

    #[test]
    fn root_record_and_self_parent_are_not_duplicated() {
        let mut ix = Index::new();
        // MFT enumeration lists the root itself (its parent is itself)
        let v = VolumeIndex::build(
            &mut ix,
            "C:",
            ROOT,
            [d(ROOT, ROOT, "."), f(30, ROOT, "a.txt")],
        );
        assert_eq!(ix.live_len(), 2);
        assert_eq!(path(&ix, &v, 30), r"C:\a.txt");
    }

    #[test]
    fn orphan_and_cycle_attach_to_root() {
        let mut ix = Index::new();
        let v = VolumeIndex::build(
            &mut ix,
            "C:",
            ROOT,
            [
                f(30, 999, "orphan.txt"),
                d(40, 41, "a"),
                d(41, 40, "b"),
                f(42, 41, "in-cycle.txt"),
            ],
        );
        // not under the root, where everyone sees them, but
        // in a folder the privacy rules hide from everyone
        assert_eq!(path(&ix, &v, 30), r"C:\$Flint.Orphans\orphan.txt");
        // one of the cycle's dirs is cut loose there; all stay reachable
        let p = path(&ix, &v, 42);
        assert!(
            p == r"C:\$Flint.Orphans\a\b\in-cycle.txt" || p == r"C:\$Flint.Orphans\b\in-cycle.txt",
            "{p}"
        );
        assert_eq!(ix.live_len(), 6, "root, orphans folder, 4 records");
        // a snapshot rebuilt from entries() keeps them there
        let recs: Vec<UsnRecord> = v
            .entries(&ix)
            .map(|(frn, parent, id)| rec(frn, parent, ix.name(id), ix.is_dir(id), 0))
            .collect();
        let mut ix2 = Index::new();
        let v2 = VolumeIndex::build(&mut ix2, "C:", ROOT, recs);
        assert_eq!(path(&ix2, &v2, 30), r"C:\$Flint.Orphans\orphan.txt");
        assert_eq!(ix2.live_len(), 6);
    }

    #[test]
    fn create_rename_move_delete() {
        let mut ix = Index::new();
        let mut v = VolumeIndex::build(&mut ix, "C:", ROOT, [d(20, ROOT, "a"), d(21, ROOT, "b")]);
        v.apply(
            &mut ix,
            &rec(30, 20, "x.txt", false, USN_REASON_FILE_CREATE),
        );
        v.apply(
            &mut ix,
            &rec(
                30,
                20,
                "x.txt",
                false,
                USN_REASON_FILE_CREATE | USN_REASON_CLOSE,
            ),
        );
        assert_eq!(path(&ix, &v, 30), r"C:\a\x.txt");
        assert_eq!(ix.live_len(), 4, "the close record does not duplicate");
        // a rename emits the old name first; it must not undo anything
        v.apply(
            &mut ix,
            &rec(30, 20, "x.txt", false, USN_REASON_RENAME_OLD_NAME),
        );
        v.apply(
            &mut ix,
            &rec(30, 21, "y.txt", false, USN_REASON_RENAME_NEW_NAME),
        );
        assert_eq!(path(&ix, &v, 30), r"C:\b\y.txt");
        let id = v.id(30).unwrap();
        v.apply(
            &mut ix,
            &rec(
                30,
                21,
                "y.txt",
                false,
                USN_REASON_FILE_DELETE | USN_REASON_CLOSE,
            ),
        );
        assert!(!ix.is_live(id));
        assert_eq!(v.id(30), None);
        assert_eq!(v.frn(id), None);
        assert_eq!(ix.live_len(), 3);
    }

    #[test]
    fn data_change_only_touches_time() {
        let mut ix = Index::new();
        let mut v = VolumeIndex::build(&mut ix, "C:", ROOT, [f(30, ROOT, "a.txt")]);
        let mut r = rec(30, ROOT, "a.txt", false, 0x2 /* DATA_EXTEND */);
        r.time = 5000;
        v.apply(&mut ix, &r);
        assert_eq!(ix.last_change(v.id(30).unwrap()), 5000);
        assert_eq!(ix.live_len(), 2);
    }

    #[test]
    fn folder_move_moves_descendants() {
        let mut ix = Index::new();
        let mut recs = vec![d(20, ROOT, "proje"), d(21, ROOT, "arsiv")];
        recs.extend((0..1000).map(|i| f(1000 + i, 20, &format!("dosya{i}.txt"))));
        let mut v = VolumeIndex::build(&mut ix, "C:", ROOT, recs);
        v.apply(
            &mut ix,
            &rec(20, 21, "proje-2025", true, USN_REASON_RENAME_NEW_NAME),
        );
        for i in 0..1000 {
            assert_eq!(
                path(&ix, &v, 1000 + i),
                format!(r"C:\arsiv\proje-2025\dosya{i}.txt")
            );
        }
    }

    #[test]
    fn burst_of_creates_and_deletes() {
        let mut ix = Index::new();
        let mut v = VolumeIndex::build(&mut ix, "C:", ROOT, [d(20, ROOT, "repo")]);
        for i in 0..10_000 {
            v.apply(
                &mut ix,
                &rec(
                    100 + i,
                    20,
                    &format!("f{i}.rs"),
                    false,
                    USN_REASON_FILE_CREATE,
                ),
            );
        }
        for i in 0..9_999 {
            v.apply(
                &mut ix,
                &rec(
                    100 + i,
                    20,
                    &format!("f{i}.rs"),
                    false,
                    USN_REASON_FILE_DELETE,
                ),
            );
        }
        assert_eq!(ix.live_len(), 3);
        assert_eq!(v.len(), 3, "root, repo, last file");
        assert_eq!(path(&ix, &v, 100 + 9_999), r"C:\repo\f9999.rs");
    }

    #[test]
    fn create_of_known_frn_is_rename() {
        let mut ix = Index::new();
        let mut v = VolumeIndex::build(
            &mut ix,
            "C:",
            ROOT,
            [d(20, ROOT, "a"), f(30, ROOT, "old.txt")],
        );
        v.apply(
            &mut ix,
            &rec(30, 20, "new.txt", false, USN_REASON_FILE_CREATE),
        );
        assert_eq!(path(&ix, &v, 30), r"C:\a\new.txt");
        assert_eq!(ix.live_len(), 3);
    }

    #[test]
    fn unknown_parent_goes_to_root_and_unknown_delete_is_ignored() {
        let mut ix = Index::new();
        let mut v = VolumeIndex::build(&mut ix, "C:", ROOT, []);
        v.apply(
            &mut ix,
            &rec(77, 12345, "lost.txt", false, USN_REASON_FILE_CREATE),
        );
        assert_eq!(path(&ix, &v, 77), r"C:\$Flint.Orphans\lost.txt");
        v.apply(
            &mut ix,
            &rec(88, ROOT, "never.txt", false, USN_REASON_FILE_DELETE),
        );
        assert_eq!(ix.live_len(), 3);
    }

    #[test]
    fn deleting_a_folder_removes_what_is_left_inside() {
        // Windows deletes children first, but a missed record must not
        // leave entries under a dead folder.
        let mut ix = Index::new();
        let mut v = VolumeIndex::build(
            &mut ix,
            "C:",
            ROOT,
            [d(20, ROOT, "a"), d(21, 20, "b"), f(30, 21, "x.txt")],
        );
        let x = v.id(30).unwrap();
        v.apply(&mut ix, &rec(20, ROOT, "a", true, USN_REASON_FILE_DELETE));
        assert!(!ix.is_live(x));
        assert_eq!(v.id(30), None);
        assert_eq!(ix.live_len(), 1);
    }

    #[test]
    fn a_move_into_its_own_subtree_is_parked_not_a_cycle() {
        let mut ix = Index::new();
        let mut v = VolumeIndex::build(&mut ix, "C:", ROOT, [d(20, ROOT, "a"), d(21, 20, "b")]);
        // stale order: "a" said to move under its own child "b"
        let mut mv = d(20, 21, "a");
        mv.reason = crate::usn::USN_REASON_RENAME_NEW_NAME;
        v.apply(&mut ix, &mv);
        let a = v.id(20).unwrap();
        assert_eq!(ix.path(a), format!(r"C:\{ORPHANS}\a"));
        assert_eq!(ix.ancestors(a).count(), 2, "no cycle");
        // the record that comes next places it for real
        let mut b = d(21, ROOT, "b");
        b.reason = crate::usn::USN_REASON_RENAME_NEW_NAME;
        v.apply(&mut ix, &b);
        v.apply(&mut ix, &mv);
        assert_eq!(ix.path(a), r"C:\b\a");
    }

    #[test]
    fn a_repeated_frn_keeps_its_last_record_in_parent_first_order() {
        // parents first (a snapshot's order) takes the fast path
        let mut ix = Index::new();
        let v = VolumeIndex::build(
            &mut ix,
            "C:",
            ROOT,
            [
                d(20, ROOT, "a"),
                d(21, ROOT, "b"),
                f(30, 20, "eski.txt"),
                f(30, 21, "yeni.txt"),
            ],
        );
        let id = v.id(30).unwrap();
        assert_eq!(ix.path(id), r"C:\b\yeni.txt");
        assert_eq!(ix.live_len(), 4, "one entry for FRN 30");
    }

    #[test]
    fn entries_lists_parents_first() {
        let mut ix = Index::new();
        let v = VolumeIndex::build(&mut ix, "C:", ROOT, [f(30, 20, "x"), d(20, ROOT, "a")]);
        let e: Vec<(u64, u64)> = v.entries(&ix).map(|(f, p, _)| (f, p)).collect();
        assert_eq!(e, vec![(20, ROOT), (30, 20)]);
    }
}
