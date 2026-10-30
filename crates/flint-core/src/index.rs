//! Arena-backed in-memory index of file-system entries.

#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
pub struct EntryId(pub u32);

pub struct EntryInput<'a> {
    pub name: &'a str,
    pub parent: Option<EntryId>,
    pub is_dir: bool,
    pub last_change: u32,
}

pub(crate) const NONE: u32 = u32::MAX;

/// Who may see an entry (the privacy rule). `OWNER_NONE` is public;
/// any other value is visible only to a viewer with the same value;
/// `OWNER_NOBODY` is visible to no one.
pub type Owner = u16;
pub const OWNER_NONE: Owner = 0;
pub const OWNER_NOBODY: Owner = u16::MAX;

/// The one visibility rule: public entries, and the viewer's own (no one
/// owns `OWNER_NOBODY`'s).
#[inline]
pub fn visible_to(owner: Owner, viewer: Owner) -> bool {
    owner == OWNER_NONE || (owner == viewer && owner != OWNER_NOBODY)
}

/// A folder's own access state, set by the indexer from its security
/// descriptor (see [`Index::set_restriction`]).
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum Restriction {
    /// An ordinary user may not list it: shown to no one, unless an owner
    /// rule gave it (or a folder above it) an owner.
    NoList,
    /// Its name claims an owner it does not have (a recycle-bin folder
    /// created by another account): shown to no one, whatever the rules.
    Untrusted,
}

/// Borrowed entry columns (see [`Index::columns`]).
pub(crate) struct Columns<'a> {
    pub parent: &'a [u32],
    pub flags: &'a [u8],
    pub mask: &'a [u32],
}

/// Directory structure derived from the index, cached until a directory
/// is renamed or moved. Positions index `order`.
pub struct DirTree {
    /// Directory ids (dead ones included), parents before children.
    pub order: Vec<u32>,
    /// Position of each directory's parent (`u32::MAX` for roots).
    pub parent_pos: Vec<u32>,
    /// Path class of each directory, its own name included:
    /// -1 noise (caches, package stores), 1 personal, 0 neutral.
    pub class: Vec<i8>,
    /// Letter mask (`fold::char_mask`) of each directory's own name, so a
    /// scan over directories reads it sequentially.
    pub mask: Vec<u32>,
    /// Position of every entry id (`u32::MAX` for files).
    pub pos_of: Vec<u32>,
    /// Owner of each directory and everything in it (see
    /// [`Index::set_owner_rules`]).
    pub owner: Vec<Owner>,
    /// Owner-rule trie node of each directory (`owner::NO_NODE` = none).
    pub rule_node: Vec<u32>,
    /// Owner of every entry id (a directory's own, a file's folder's),
    /// so the search loop reads it sequentially next to the letter masks.
    pub entry_owner: Vec<Owner>,
}

impl DirTree {
    /// Appends the entry `id` (just inserted into `ix`).
    fn push(&mut self, ix: &Index, id: EntryId) {
        let pos = if ix.is_dir(id) {
            self.add_dir(ix, id)
        } else {
            NONE
        };
        push_col(&mut self.pos_of, pos);
        let o = self.owner_of(ix, id);
        push_col(&mut self.entry_owner, o);
    }

    /// Owner of entry `id` from the directory owners (its directory and
    /// parent positions must be present).
    fn owner_of(&self, ix: &Index, id: EntryId) -> Owner {
        let d = if ix.is_dir(id) {
            Some(id)
        } else {
            ix.parent(id)
        };
        d.map_or(OWNER_NONE, |d| match self.pos_of[d.0 as usize] {
            // a folder missing from the tree: shown to no one (fail closed)
            NONE => OWNER_NOBODY,
            p => self.owner[p as usize],
        })
    }

    /// Appends directory `id`, whose parent is already present; returns
    /// its position.
    fn add_dir(&mut self, ix: &Index, id: EntryId) -> u32 {
        let pos = self.order.len() as u32;
        let (ppos, class, owner, rule_node) = self.derive(ix, id);
        push_col(&mut self.order, id.0);
        push_col(&mut self.parent_pos, ppos);
        push_col(&mut self.class, class);
        push_col(&mut self.mask, ix.mask(id));
        push_col(&mut self.owner, owner);
        push_col(&mut self.rule_node, rule_node);
        pos
    }

    /// Parent position, class, owner and rule node of directory `id`
    /// from its parent's (which must be present).
    fn derive(&self, ix: &Index, id: EntryId) -> (u32, i8, Owner, u32) {
        let (owner, rule_node) = match ix.parent(id) {
            // a parent missing from the tree: fail closed (Index keeps every
            // parent a folder, so this is a guard, not a path)
            Some(p) if self.pos_of[p.0 as usize] == NONE => (OWNER_NOBODY, crate::owner::NO_NODE),
            Some(p) => {
                let pp = self.pos_of[p.0 as usize] as usize;
                dir_owner(ix, Some((self.owner[pp], self.rule_node[pp])), id)
            }
            None => dir_owner(ix, None, id),
        };
        let (ppos, inherited, own) = match ix.parent(id) {
            Some(p) if self.pos_of[p.0 as usize] == NONE => (NONE, -1, -1),
            Some(p) => {
                let pp = self.pos_of[p.0 as usize];
                let tagged = if ix.tags.contains_key(&id.0) { -1 } else { 0 };
                (
                    pp,
                    self.class[pp as usize],
                    merge_class(crate::rank::name_class(ix.folded(id)), tagged),
                )
            }
            None => (NONE, 0, 0), // a drive root's name never classifies
        };
        (ppos, merge_class(inherited, own), owner, rule_node)
    }

    /// Brings folder `id` and everything below it up to date after its
    /// name, parent, restriction or tag changed, without rebuilding the
    /// whole tree (a busy volume renames folders every second, and a
    /// rebuild costs a query ~100 ms on 3 M entries). `false` when the
    /// order no longer holds (moved under a folder listed after it): the
    /// caller drops the tree.
    fn refresh_subtree(&mut self, ix: &Index, id: EntryId) -> bool {
        let p = self.pos_of[id.0 as usize];
        if p == NONE {
            return false;
        }
        if let Some(parent) = ix.parent(id) {
            let pp = self.pos_of[parent.0 as usize];
            if pp == NONE || pp >= p {
                return false;
            }
        }
        let p = p as usize;
        let mut member = vec![false; self.order.len() - p];
        let mut owners_changed = false;
        for q in p..self.order.len() {
            let inside = q == p || {
                let pq = self.parent_pos[q];
                pq != NONE && pq as usize >= p && member[pq as usize - p]
            };
            if !inside {
                continue;
            }
            member[q - p] = true;
            let d = EntryId(self.order[q]);
            let (ppos, class, owner, rule_node) = self.derive(ix, d);
            owners_changed |= owner != self.owner[q];
            self.parent_pos[q] = ppos;
            self.class[q] = class;
            self.mask[q] = ix.mask(d);
            self.owner[q] = owner;
            self.rule_node[q] = rule_node;
        }
        if owners_changed {
            for i in 0..self.entry_owner.len() {
                let e = EntryId(i as u32);
                let d = if ix.is_dir(e) { Some(e) } else { ix.parent(e) };
                if let Some(d) = d
                    && let pos = self.pos_of[d.0 as usize]
                    && pos != NONE
                    && pos as usize >= p
                    && member[pos as usize - p]
                {
                    self.entry_owner[i] = self.owner[pos as usize];
                }
            }
        }
        true
    }
}

/// Owner and rule-trie node of folder `id` from its parent's (`None` for
/// a drive root). The one owner step, shared by the tree and
/// [`Index::owner_uncached`].
fn dir_owner(ix: &Index, parent: Option<(Owner, u32)>, id: EntryId) -> (Owner, u32) {
    let (owner, node) = match parent {
        Some((po, pn)) => {
            let (node, ruled) = ix.owner_rules.child(Some(pn), ix.name(id));
            // an owned subtree stays owned: a rule deeper inside never
            // makes part of it more visible
            match po {
                OWNER_NONE => (ruled.unwrap_or(OWNER_NONE), node),
                o => (o, node),
            }
        }
        None => (OWNER_NONE, ix.owner_rules.child(None, ix.name(id)).0),
    };
    let owner = match ix.restricted.get(&id.0) {
        Some(Restriction::Untrusted) => OWNER_NOBODY,
        Some(Restriction::NoList) if owner == OWNER_NONE => OWNER_NOBODY,
        _ => owner,
    };
    (owner, node)
}

/// Noise wins over personal: a cache inside Documents is still a cache.
fn merge_class(a: i8, b: i8) -> i8 {
    if a < 0 || b < 0 { -1 } else { a.max(b) }
}
pub(crate) const F_DIR: u8 = 1;
pub(crate) const F_DEAD: u8 = 2;
/// The original name has a camelCase word start (`fold::has_camel`).
const F_CAMEL: u8 = 4;
/// The folded name equals the original, which is stored once.
const F_SAME: u8 = 8;

/// Parallel-vector storage. Names live in one append-only text: each
/// entry's folded form at `text_off` (what search scans), then its
/// original, unless the two are equal ([`F_SAME`], most names), when it
/// is stored once. A rename
/// appends new text; the old bytes stay as garbage (see
/// [`Index::garbage_ratio`]).
#[derive(Default)]
pub struct Index {
    text: String,
    text_off: Vec<u32>,
    name_len: Vec<u16>,
    fold_len: Vec<u16>,
    parent: Vec<u32>,
    flags: Vec<u8>,
    last_change: Vec<u32>,
    child_count: Vec<u32>,
    /// Lowercase drive letter of each entry's root, copied from the parent
    /// at insert (volumes never move between drives).
    drive: Vec<u8>,
    /// `fold::char_mask` of each folded name: a cheap reject before any
    /// text is read.
    mask: Vec<u32>,
    live: usize,
    /// Directory id -> number of live `CACHEDIR.TAG` children; only tagged
    /// directories have an entry. A tagged directory is noise.
    tags: std::collections::HashMap<u32, u16>,
    /// Name-based owner rules (privacy filter).
    owner_rules: crate::owner::OwnerRules,
    /// Folder id -> its access restriction; only restricted folders have
    /// an entry. Kept across renames and moves (the ACL moves too).
    restricted: std::collections::HashMap<u32, Restriction>,
    /// Built on first use, extended by `insert`, updated in place when a
    /// folder is renamed, moved, restricted or tagged.
    dir_tree: std::sync::OnceLock<DirTree>,
}

/// How much a full column grows: an eighth of its length, at least
/// `min`. Not doubling (what `Vec::push` does): after a warm start every
/// column is exactly full, and doubling them all at the first new file
/// commits as much memory again as the whole index. The cost is more
/// copies when a column is filled one entry at a time, so a bulk load
/// reserves its room first ([`Index::reserve`]).
fn room(len: usize, min: usize) -> usize {
    (len / 8).max(min)
}

/// Appends `x`, growing a full column by [`room`].
fn push_col<T>(v: &mut Vec<T>, x: T) {
    if v.len() == v.capacity() {
        v.reserve_exact(room(v.len(), 1024));
    }
    v.push(x);
}

/// Lengthens `v` to `len` with `fill`, growing it the way the index's
/// own columns grow (by an eighth, not by doubling). For columns kept
/// beside the index, one slot per entry id.
pub fn grow_col<T: Clone>(v: &mut Vec<T>, len: usize, fill: T) {
    if len > v.capacity() {
        v.reserve_exact((len - v.len()).max(room(v.len(), 1024)));
    }
    if len > v.len() {
        v.resize(len, fill);
    }
}

/// Appends `folded`, then `name` unless it equals `folded`; returns the
/// offset and whether the two are equal ([`F_SAME`]).
fn push_text(text: &mut String, name: &str, folded: &str) -> (u32, bool) {
    let off = text.len();
    let same = name == folded;
    let add = name.len() + if same { 0 } else { folded.len() };
    assert!(
        name.len().max(folded.len()) <= u16::MAX as usize,
        "name longer than 65535 bytes"
    );
    assert!(off + add <= u32::MAX as usize, "name text exceeds 4 GiB");
    if off + add > text.capacity() {
        text.reserve_exact(room(off, 64 * 1024).max(add));
    }
    text.push_str(folded);
    if !same {
        text.push_str(name);
    }
    (off as u32, same)
}

/// Folded name of the Cache Directory Tagging marker file.
const CACHEDIR_TAG: &str = "cachedir.tag";

impl Index {
    pub fn new() -> Self {
        Self::default()
    }

    /// Room for exactly `n` more entries (a scan inserts millions at
    /// once; `Vec::reserve` would double columns already full).
    pub fn reserve(&mut self, n: usize) {
        // a name and, for a third of names, its folded form: 44 bytes an
        // entry on a measured system drive (34-byte names, two thirds
        // equal to their fold)
        self.text.reserve_exact(n * 44);
        self.text_off.reserve_exact(n);
        self.name_len.reserve_exact(n);
        self.fold_len.reserve_exact(n);
        self.parent.reserve_exact(n);
        self.flags.reserve_exact(n);
        self.last_change.reserve_exact(n);
        self.child_count.reserve_exact(n);
        self.drive.reserve_exact(n);
        self.mask.reserve_exact(n);
    }

    /// Counts a tag child in or out of `dir`; the cached tree is dropped
    /// only when the directory's tagged state flips.
    fn retag(&mut self, dir: u32, add: bool) {
        let flipped = if add {
            let n = self.tags.entry(dir).or_insert(0);
            *n += 1;
            *n == 1
        } else {
            match self.tags.get_mut(&dir) {
                Some(n) if *n > 1 => {
                    *n -= 1;
                    false
                }
                Some(_) => {
                    self.tags.remove(&dir);
                    true
                }
                None => false,
            }
        };
        if flipped {
            self.refresh_dir(EntryId(dir));
        }
    }

    /// Makes `p` a folder: an entry with children is one, whatever it was
    /// indexed as (a file replaced by a folder before its record arrived).
    /// Keeps every parent in the folder tree, which owners and path
    /// scores rely on.
    fn ensure_dir(&mut self, p: EntryId) {
        let f = &mut self.flags[p.0 as usize];
        if *f & F_DIR == 0 {
            *f |= F_DIR;
            self.dir_tree = std::sync::OnceLock::new();
        }
    }

    pub fn insert(&mut self, e: EntryInput) -> EntryId {
        if let Some(p) = e.parent {
            self.ensure_dir(p);
        }
        let id = EntryId(self.parent.len() as u32);
        let folded = crate::fold::fold_str(e.name);
        let (off, same) = push_text(&mut self.text, e.name, &folded);
        push_col(&mut self.mask, crate::fold::char_mask(&folded));
        push_col(&mut self.text_off, off);
        push_col(&mut self.name_len, e.name.len() as u16);
        push_col(&mut self.fold_len, folded.len() as u16);
        push_col(&mut self.parent, e.parent.map_or(NONE, |p| p.0));
        let camel = if crate::fold::has_camel(e.name) {
            F_CAMEL
        } else {
            0
        };
        let same = if same { F_SAME } else { 0 };
        push_col(
            &mut self.flags,
            if e.is_dir { F_DIR } else { 0 } | camel | same,
        );
        push_col(&mut self.last_change, e.last_change);
        push_col(&mut self.child_count, 0);
        let drive = match e.parent {
            Some(p) => self.drive[p.0 as usize],
            None => e
                .name
                .bytes()
                .next()
                .map_or(b'?', |b| b.to_ascii_lowercase()),
        };
        push_col(&mut self.drive, drive);
        if let Some(p) = e.parent {
            self.child_count[p.0 as usize] += 1;
        }
        self.live += 1;
        if let Some(mut tree) = self.dir_tree.take() {
            tree.push(self, id); // its parent already exists, so order holds
            let _ = self.dir_tree.set(tree);
        }
        if let Some(p) = e.parent
            && folded == CACHEDIR_TAG
        {
            self.retag(p.0, true);
        }
        id
    }

    pub fn remove(&mut self, id: EntryId) {
        let i = id.0 as usize;
        if self.flags[i] & F_DEAD != 0 {
            return;
        }
        self.flags[i] |= F_DEAD;
        if let Some(p) = self.parent(id) {
            self.child_count[p.0 as usize] -= 1;
            if self.folded(id) == CACHEDIR_TAG {
                self.retag(p.0, false);
            }
        }
        self.live -= 1;
    }

    /// Renames `id`; `new_parent: None` keeps the current parent.
    pub fn rename(&mut self, id: EntryId, new_name: &str, new_parent: Option<EntryId>) {
        let i = id.0 as usize;
        let was_tag = self.is_live(id) && self.folded(id) == CACHEDIR_TAG;
        let old_parent = self.parent(id);
        let folded = crate::fold::fold_str(new_name);
        let (off, same) = push_text(&mut self.text, new_name, &folded);
        self.mask[i] = crate::fold::char_mask(&folded);
        self.flags[i] &= !(F_CAMEL | F_SAME);
        if crate::fold::has_camel(new_name) {
            self.flags[i] |= F_CAMEL;
        }
        if same {
            self.flags[i] |= F_SAME;
        }
        self.text_off[i] = off;
        self.name_len[i] = new_name.len() as u16;
        self.fold_len[i] = folded.len() as u16;
        if let Some(np) = new_parent
            && self.parent[i] != np.0
        {
            self.ensure_dir(np);
            if let Some(old) = self.parent(id) {
                self.child_count[old.0 as usize] -= 1;
            }
            self.parent[i] = np.0;
            self.child_count[np.0 as usize] += 1;
            if !self.is_dir(id)
                && let Some(mut tree) = self.dir_tree.take()
            {
                // a moved file takes its new folder's owner
                tree.entry_owner[i] = tree.owner_of(self, id);
                let _ = self.dir_tree.set(tree);
            }
        }
        let is_tag = self.is_live(id) && folded == CACHEDIR_TAG;
        let new_parent = self.parent(id);
        if was_tag
            && (!is_tag || new_parent != old_parent)
            && let Some(p) = old_parent
        {
            self.retag(p.0, false);
        }
        if is_tag
            && (!was_tag || new_parent != old_parent)
            && let Some(p) = new_parent
        {
            self.retag(p.0, true);
        }
        if self.is_dir(id) {
            // class, owner and inherited path scores of the whole subtree
            // may change
            self.refresh_dir(id);
        }
    }

    /// The original name when it has a camelCase word start, else `""`:
    /// the typo tier reads word starts from it (folding lowercased them)
    /// and skips the work for every other name.
    pub fn camel_source(&self, id: EntryId) -> &str {
        if self.flags[id.0 as usize] & F_CAMEL != 0 {
            self.name(id)
        } else {
            ""
        }
    }

    pub fn touch(&mut self, id: EntryId, t: u32) {
        self.last_change[id.0 as usize] = t;
    }

    pub fn name(&self, id: EntryId) -> &str {
        let i = id.0 as usize;
        let o = self.text_off[i] as usize;
        let o = if self.flags[i] & F_SAME != 0 {
            o
        } else {
            o + self.fold_len[i] as usize
        };
        &self.text[o..o + self.name_len[i] as usize]
    }

    pub fn folded(&self, id: EntryId) -> &str {
        let i = id.0 as usize;
        let o = self.text_off[i] as usize;
        &self.text[o..o + self.fold_len[i] as usize]
    }

    pub fn parent(&self, id: EntryId) -> Option<EntryId> {
        match self.parent[id.0 as usize] {
            NONE => None,
            p => Some(EntryId(p)),
        }
    }

    pub fn is_dir(&self, id: EntryId) -> bool {
        self.flags[id.0 as usize] & F_DIR != 0
    }

    pub fn is_live(&self, id: EntryId) -> bool {
        self.flags[id.0 as usize] & F_DEAD == 0
    }

    pub fn last_change(&self, id: EntryId) -> u32 {
        self.last_change[id.0 as usize]
    }

    pub fn child_count(&self, id: EntryId) -> u32 {
        self.child_count[id.0 as usize]
    }

    /// Full path joined with `\`; a root's own path is `"C:\"`.
    pub fn path(&self, id: EntryId) -> String {
        let mut parts: Vec<&str> = self.ancestors(id).map(|a| self.name(a)).collect();
        parts.reverse();
        parts.push(self.name(id));
        if parts.len() == 1 {
            return format!("{}\\", parts[0]);
        }
        parts.join("\\")
    }

    /// Lowercase drive letter of the entry's root.
    pub fn drive(&self, id: EntryId) -> char {
        self.drive[id.0 as usize] as char
    }

    /// Parent first, root last.
    pub fn ancestors(&self, id: EntryId) -> impl Iterator<Item = EntryId> + '_ {
        std::iter::successors(self.parent(id), move |&p| self.parent(p))
    }

    pub fn live_len(&self) -> usize {
        self.live
    }

    /// Raw columns for the search hot loop, indexed by entry id.
    pub(crate) fn columns(&self) -> Columns<'_> {
        Columns {
            parent: &self.parent,
            flags: &self.flags,
            mask: &self.mask,
        }
    }

    pub fn mask(&self, id: EntryId) -> u32 {
        self.mask[id.0 as usize]
    }

    pub fn dir_tree(&self) -> &DirTree {
        self.dir_tree.get_or_init(|| self.build_tree())
    }

    /// Updates the cached tree for folder `id` (see
    /// [`DirTree::refresh_subtree`]), or drops it when that cannot hold.
    fn refresh_dir(&mut self, id: EntryId) {
        if let Some(mut tree) = self.dir_tree.take()
            && tree.refresh_subtree(self, id)
        {
            let _ = self.dir_tree.set(tree);
        }
    }

    /// Every folder below folder `id` (dead ones included), from the
    /// cached tree: one pass over the folders listed after it.
    pub fn descendant_dirs(&self, id: EntryId) -> Vec<EntryId> {
        let t = self.dir_tree();
        let p = t.pos_of[id.0 as usize];
        if p == NONE {
            return Vec::new();
        }
        let p = p as usize;
        let mut member = vec![false; t.order.len() - p];
        member[0] = true;
        let mut out = Vec::new();
        for q in p + 1..t.order.len() {
            let pq = t.parent_pos[q];
            if pq != NONE && pq as usize >= p && member[pq as usize - p] {
                member[q - p] = true;
                out.push(EntryId(t.order[q]));
            }
        }
        out
    }

    /// Test aid: `Err` names the first entry whose cached owner, class,
    /// letter mask or parent differs from a tree built from scratch.
    #[doc(hidden)]
    pub fn check_tree(&self) -> Result<(), String> {
        let (a, b) = (self.dir_tree(), &self.build_tree());
        for i in 0..self.raw_len() {
            let id = EntryId(i as u32);
            if !self.is_live(id) {
                continue;
            }
            let what = || self.path(id);
            if a.entry_owner[i] != b.entry_owner[i] {
                return Err(format!("entry owner of {}", what()));
            }
            let (pa, pb) = (a.pos_of[i], b.pos_of[i]);
            if (pa == NONE) != (pb == NONE) {
                return Err(format!("folder or not: {}", what()));
            }
            if pa == NONE {
                continue;
            }
            let (pa, pb) = (pa as usize, pb as usize);
            let parent_id = |t: &DirTree, pp: u32| (pp != NONE).then(|| t.order[pp as usize]);
            if a.owner[pa] != b.owner[pb]
                || a.class[pa] != b.class[pb]
                || a.mask[pa] != b.mask[pb]
                || a.rule_node[pa] != b.rule_node[pb]
                || parent_id(a, a.parent_pos[pa]) != parent_id(b, b.parent_pos[pb])
            {
                return Err(format!("folder fields of {}", what()));
            }
        }
        Ok(())
    }

    fn build_tree(&self) -> DirTree {
        {
            let n = self.raw_len();
            let mut dirs: Vec<(u16, u32)> = (0..n as u32)
                .filter(|&i| self.flags[i as usize] & F_DIR != 0)
                .map(|i| (self.ancestors(EntryId(i)).count() as u16, i))
                .collect();
            dirs.sort_unstable();
            let mut tree = DirTree {
                order: Vec::with_capacity(dirs.len()),
                parent_pos: Vec::with_capacity(dirs.len()),
                class: Vec::with_capacity(dirs.len()),
                mask: Vec::with_capacity(dirs.len()),
                pos_of: vec![NONE; n],
                owner: Vec::with_capacity(dirs.len()),
                rule_node: Vec::with_capacity(dirs.len()),
                entry_owner: Vec::new(),
            };
            for (_, d) in dirs {
                let pos = tree.add_dir(self, EntryId(d));
                tree.pos_of[d as usize] = pos;
            }
            tree.entry_owner = (0..n as u32)
                .map(|i| tree.owner_of(self, EntryId(i)))
                .collect();
            tree
        }
    }

    /// Replaces the owner rules (see [`crate::OwnerRule`]). A folder an
    /// outer rule gave an owner keeps it for everything below, whatever
    /// rules match deeper.
    pub fn set_owner_rules(&mut self, rules: &[crate::OwnerRule]) {
        self.owner_rules = crate::owner::OwnerRules::new(rules);
        self.dir_tree = std::sync::OnceLock::new();
    }

    /// Sets or clears folder `id`'s restriction (privacy filter: see
    /// [`Restriction`]). The folder tree is rebuilt only when it changes.
    pub fn set_restriction(&mut self, id: EntryId, r: Option<Restriction>) {
        let old = match r {
            Some(r) => self.restricted.insert(id.0, r),
            None => self.restricted.remove(&id.0),
        };
        if old != r {
            if self.is_dir(id) {
                self.refresh_dir(id);
            } else {
                self.dir_tree = std::sync::OnceLock::new();
            }
        }
    }

    pub fn restriction(&self, id: EntryId) -> Option<Restriction> {
        self.restricted.get(&id.0).copied()
    }

    /// Whether any owner rule is set (without rules every entry is
    /// public and a search skips the filter).
    pub fn has_owners(&self) -> bool {
        !self.owner_rules.is_empty()
    }

    /// Live entries `viewer` may see (every live entry without rules), so
    /// a status reply never counts another user's files.
    pub fn visible_len(&self, viewer: Owner) -> usize {
        if !self.has_owners() {
            return self.live;
        }
        let t = self.dir_tree();
        (0..self.raw_len())
            .filter(|&i| self.flags[i] & F_DEAD == 0 && visible_to(t.entry_owner[i], viewer))
            .count()
    }

    /// [`Index::visible_len`] per drive: `(drive letter, lowercase ASCII;
    /// '?' for an unnamed root, count)`, sorted by drive.
    pub fn visible_len_by_drive(&self, viewer: Owner) -> Vec<(u8, usize)> {
        let mut by = std::collections::BTreeMap::<u8, usize>::new();
        let t = self.has_owners().then(|| self.dir_tree());
        for i in 0..self.raw_len() {
            if self.flags[i] & F_DEAD == 0 && t.is_none_or(|t| visible_to(t.entry_owner[i], viewer))
            {
                *by.entry(self.drive[i]).or_default() += 1;
            }
        }
        by.into_iter().collect()
    }

    /// Owner of the entry: its own for a directory, its folder's for a file.
    pub fn owner(&self, id: EntryId) -> Owner {
        self.dir_tree().entry_owner[id.0 as usize]
    }

    /// [`Index::owner`] computed from the entry's ancestors, without the
    /// cached folder tree (which a folder rename drops): O(depth).
    pub fn owner_uncached(&self, id: EntryId) -> Owner {
        let dir = if self.is_dir(id) {
            id
        } else {
            match self.parent(id) {
                Some(p) => p,
                None => return OWNER_NONE,
            }
        };
        let mut chain: Vec<EntryId> = self.ancestors(dir).collect();
        chain.reverse();
        chain.push(dir);
        let mut state = None;
        for x in chain {
            state = Some(dir_owner(self, state, x));
        }
        state.map_or(OWNER_NONE, |(o, _)| o)
    }

    /// Directory ids (dead ones included), parents before children.
    pub fn dir_order(&self) -> &[u32] {
        &self.dir_tree().order
    }

    /// Path class of the entry's ancestors (see [`DirTree::class`]).
    pub fn path_class(&self, id: EntryId) -> i8 {
        match self.parent(id) {
            Some(p) => {
                let t = self.dir_tree();
                match t.pos_of[p.0 as usize] {
                    NONE => 0,
                    pos => t.class[pos as usize],
                }
            }
            None => 0,
        }
    }

    /// Number of ids ever allocated, dead ones included; ids are `0..raw_len()`.
    pub fn raw_len(&self) -> usize {
        self.parent.len()
    }

    /// Test aid: bytes of name text held (live, dead and left over).
    #[doc(hidden)]
    pub fn text_bytes(&self) -> usize {
        self.text.len()
    }

    pub fn heap_bytes(&self) -> usize {
        self.text.capacity()
            + self.text_off.capacity() * 4
            + self.name_len.capacity() * 2
            + self.fold_len.capacity() * 2
            + self.parent.capacity() * 4
            + self.flags.capacity()
            + self.last_change.capacity() * 4
            + self.child_count.capacity() * 4
            + self.drive.capacity()
            + self.mask.capacity() * 4
            + self.dir_tree.get().map_or(0, |t| {
                t.order.capacity() * 4
                    + t.parent_pos.capacity() * 4
                    + t.class.capacity()
                    + t.owner.capacity() * 2
                    + t.rule_node.capacity() * 4
                    + t.entry_owner.capacity() * 2
                    + t.mask.capacity() * 4
                    + t.pos_of.capacity() * 4
            })
    }
}

/// Why [`Index::from_image`] refused its input.
#[derive(Debug, PartialEq, Eq)]
pub enum ImageError {
    Truncated,
    /// A name offset, parent or key outside the index, a parent that is
    /// not a folder, a parent cycle, or text that is not UTF-8.
    Invalid(&'static str),
}

struct Reader<'a> {
    b: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], ImageError> {
        let end = self.at.checked_add(n).ok_or(ImageError::Truncated)?;
        let s = self.b.get(self.at..end).ok_or(ImageError::Truncated)?;
        self.at = end;
        Ok(s)
    }
    fn u64(&mut self) -> Result<u64, ImageError> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn len(&mut self, max: usize) -> Result<usize, ImageError> {
        let n = self.u64()?;
        if n > max as u64 {
            return Err(ImageError::Truncated);
        }
        Ok(n as usize)
    }
    fn text(&mut self) -> Result<String, ImageError> {
        let n = self.len(self.b.len())?;
        String::from_utf8(self.take(n)?.to_vec()).map_err(|_| ImageError::Invalid("not UTF-8"))
    }
    fn col<const W: usize, T>(
        &mut self,
        n: usize,
        f: fn([u8; W]) -> T,
    ) -> Result<Vec<T>, ImageError> {
        let bytes = self.take(n.checked_mul(W).ok_or(ImageError::Truncated)?)?;
        Ok(bytes.as_chunks::<W>().0.iter().map(|c| f(*c)).collect())
    }
}

fn put_col<const W: usize, T: Copy>(out: &mut Vec<u8>, v: &[T], f: fn(T) -> [u8; W]) {
    out.reserve(v.len() * W);
    for &x in v {
        out.extend_from_slice(&f(x));
    }
}

/// Whether `len` bytes at `off` lie in `t` on char boundaries.
fn in_text(t: &str, off: usize, len: u16) -> bool {
    let e = off + len as usize;
    e <= t.len() && t.is_char_boundary(off) && t.is_char_boundary(e)
}

/// The image columns after the names (both layouts).
struct Rest {
    parent: Vec<u32>,
    flags: Vec<u8>,
    last_change: Vec<u32>,
    drive: Vec<u8>,
    mask: Vec<u32>,
}

impl Rest {
    fn read(r: &mut Reader<'_>, n: usize) -> Result<Rest, ImageError> {
        Ok(Rest {
            parent: r.col(n, u32::from_le_bytes)?,
            flags: r.col(n, |x: [u8; 1]| x[0])?,
            last_change: r.col(n, u32::from_le_bytes)?,
            drive: r.col(n, |x: [u8; 1]| x[0])?,
            mask: r.col(n, u32::from_le_bytes)?,
        })
    }
}

impl Index {
    /// Appends the index's own state (names, structure, tags,
    /// restrictions; not the owner rules, which the caller sets again) as
    /// bytes [`Index::from_image`] reads back. Dead entries are kept, so
    /// entry ids stay the same.
    pub fn to_image(&self, out: &mut Vec<u8>) {
        let n = self.raw_len();
        out.extend_from_slice(&(n as u64).to_le_bytes());
        out.extend_from_slice(&(self.text.len() as u64).to_le_bytes());
        out.extend_from_slice(self.text.as_bytes());
        put_col(out, &self.text_off, u32::to_le_bytes);
        put_col(out, &self.name_len, u16::to_le_bytes);
        put_col(out, &self.fold_len, u16::to_le_bytes);
        self.put_rest(out);
    }

    /// The columns and tables after the names, the same in both layouts.
    fn put_rest(&self, out: &mut Vec<u8>) {
        put_col(out, &self.parent, u32::to_le_bytes);
        put_col(out, &self.flags, |x: u8| [x]);
        put_col(out, &self.last_change, u32::to_le_bytes);
        put_col(out, &self.drive, |x: u8| [x]);
        put_col(out, &self.mask, u32::to_le_bytes);
        let mut tags: Vec<(u32, u16)> = self.tags.iter().map(|(&k, &v)| (k, v)).collect();
        tags.sort_unstable();
        out.extend_from_slice(&(tags.len() as u64).to_le_bytes());
        for (k, v) in tags {
            out.extend_from_slice(&k.to_le_bytes());
            out.extend_from_slice(&v.to_le_bytes());
        }
        let mut res: Vec<(u32, Restriction)> =
            self.restricted.iter().map(|(&k, &v)| (k, v)).collect();
        res.sort_unstable_by_key(|&(k, _)| k);
        out.extend_from_slice(&(res.len() as u64).to_le_bytes());
        for (k, r) in res {
            out.extend_from_slice(&k.to_le_bytes());
            out.push(match r {
                Restriction::NoList => 1,
                Restriction::Untrusted => 2,
            });
        }
    }

    /// Test aid: the image layout of Flint 0.1.3 and before (names and
    /// folded names in two texts), which [`Index::from_image_v2`] reads.
    #[doc(hidden)]
    pub fn to_image_v2(&self, out: &mut Vec<u8>) {
        let n = self.raw_len();
        let ids = || (0..n as u32).map(EntryId);
        let (mut names, mut folded) = (String::new(), String::new());
        let (mut name_off, mut fold_off) = (Vec::new(), Vec::new());
        for id in ids() {
            name_off.push(names.len() as u32);
            names.push_str(self.name(id));
            fold_off.push(folded.len() as u32);
            folded.push_str(self.folded(id));
        }
        out.extend_from_slice(&(n as u64).to_le_bytes());
        for t in [&names, &folded] {
            out.extend_from_slice(&(t.len() as u64).to_le_bytes());
            out.extend_from_slice(t.as_bytes());
        }
        put_col(out, &name_off, u32::to_le_bytes);
        put_col(out, &self.name_len, u16::to_le_bytes);
        put_col(out, &fold_off, u32::to_le_bytes);
        put_col(out, &self.fold_len, u16::to_le_bytes);
        let mut rest = Vec::new();
        self.put_rest(&mut rest);
        // 0.1.3 had no F_SAME bit: the flags follow the 4-byte parents
        for f in &mut rest[4 * n..5 * n] {
            *f &= !F_SAME;
        }
        out.extend_from_slice(&rest);
    }

    /// Reads an image of Flint 0.1.3 and before (see
    /// [`Index::to_image_v2`]): names and folded names in two texts. The
    /// names are copied into one text, each equal pair once; text left by
    /// renames is dropped.
    pub fn from_image_v2(b: &[u8]) -> Result<(Index, usize), ImageError> {
        let mut r = Reader { b, at: 0 };
        let n = r.len(u32::MAX as usize - 1)?;
        let names = r.text()?;
        let folded = r.text()?;
        let name_off = r.col(n, u32::from_le_bytes)?;
        let name_len = r.col(n, u16::from_le_bytes)?;
        let fold_off = r.col(n, u32::from_le_bytes)?;
        let fold_len = r.col(n, u16::from_le_bytes)?;
        let mut rest = Rest::read(&mut r, n)?;
        let mut size = 0usize;
        for i in 0..n {
            if !in_text(&names, name_off[i] as usize, name_len[i])
                || !in_text(&folded, fold_off[i] as usize, fold_len[i])
            {
                return Err(ImageError::Invalid("name outside the text"));
            }
            size += name_len[i] as usize + fold_len[i] as usize;
        }
        if size > u32::MAX as usize {
            return Err(ImageError::Invalid("name text exceeds 4 GiB"));
        }
        let mut text = String::with_capacity(size);
        let mut offs = Vec::with_capacity(n);
        for i in 0..n {
            let (o, f) = (name_off[i] as usize, fold_off[i] as usize);
            let name = &names[o..o + name_len[i] as usize];
            let fold = &folded[f..f + fold_len[i] as usize];
            let (off, same) = push_text(&mut text, name, fold);
            offs.push(off);
            rest.flags[i] = (rest.flags[i] & !F_SAME) | if same { F_SAME } else { 0 };
        }
        text.shrink_to_fit();
        Self::assemble(r, n, text, offs, name_len, fold_len, rest)
    }

    /// Reads an index written by [`Index::to_image`] from the start of
    /// `b`; returns it and the number of bytes read. Every offset, parent
    /// and key is checked, so a damaged image is refused, never a panic
    /// later. Owner rules start empty.
    pub fn from_image(b: &[u8]) -> Result<(Index, usize), ImageError> {
        let mut r = Reader { b, at: 0 };
        let n = r.len(u32::MAX as usize - 1)?;
        let text = r.text()?;
        let text_off = r.col(n, u32::from_le_bytes)?;
        let name_len = r.col(n, u16::from_le_bytes)?;
        let fold_len = r.col(n, u16::from_le_bytes)?;
        let rest = Rest::read(&mut r, n)?;
        for i in 0..n {
            let f = text_off[i] as usize;
            let o = if rest.flags[i] & F_SAME != 0 {
                f
            } else {
                f + fold_len[i] as usize
            };
            if !in_text(&text, f, fold_len[i]) || !in_text(&text, o, name_len[i]) {
                return Err(ImageError::Invalid("name outside the text"));
            }
        }
        Self::assemble(r, n, text, text_off, name_len, fold_len, rest)
    }

    /// Checks parents and tables and builds the index (names checked by
    /// the caller).
    fn assemble(
        mut r: Reader<'_>,
        n: usize,
        text: String,
        text_off: Vec<u32>,
        name_len: Vec<u16>,
        fold_len: Vec<u16>,
        rest: Rest,
    ) -> Result<(Index, usize), ImageError> {
        let Rest {
            parent,
            flags,
            last_change,
            drive,
            mask,
        } = rest;
        let mut live = 0;
        let mut child_count = vec![0u32; n];
        for i in 0..n {
            let p = parent[i];
            if p != NONE {
                let p = p as usize;
                if p >= n || p == i || flags[p] & F_DIR == 0 {
                    return Err(ImageError::Invalid("parent"));
                }
            }
            if flags[i] & F_DEAD == 0 {
                live += 1;
                if p != NONE {
                    child_count[p as usize] += 1;
                }
            }
        }
        // parent cycles would loop every ancestor walk
        let mut state = vec![0u8; n]; // 0 unseen, 1 on the current path, 2 done
        let mut path = Vec::new();
        for start in 0..n {
            let mut cur = start;
            while state[cur] == 0 {
                state[cur] = 1;
                path.push(cur);
                match parent[cur] {
                    NONE => break,
                    p => cur = p as usize,
                }
            }
            if state[cur] == 1 && parent[cur] != NONE {
                return Err(ImageError::Invalid("parent cycle"));
            }
            for &x in &path {
                state[x] = 2;
            }
            path.clear();
        }
        let key = |k: u32| (k as usize) < n;
        let mut tags = std::collections::HashMap::new();
        for _ in 0..r.len(n)? {
            let k = u32::from_le_bytes(r.take(4)?.try_into().unwrap());
            let v = u16::from_le_bytes(r.take(2)?.try_into().unwrap());
            if !key(k) {
                return Err(ImageError::Invalid("tag key"));
            }
            tags.insert(k, v);
        }
        let mut restricted = std::collections::HashMap::new();
        for _ in 0..r.len(n)? {
            let k = u32::from_le_bytes(r.take(4)?.try_into().unwrap());
            let v = match r.take(1)?[0] {
                1 => Restriction::NoList,
                2 => Restriction::Untrusted,
                _ => return Err(ImageError::Invalid("restriction")),
            };
            if !key(k) {
                return Err(ImageError::Invalid("restriction key"));
            }
            restricted.insert(k, v);
        }
        let ix = Index {
            text,
            text_off,
            name_len,
            fold_len,
            parent,
            flags,
            last_change,
            child_count,
            drive,
            mask,
            live,
            tags,
            owner_rules: Default::default(),
            restricted,
            dir_tree: std::sync::OnceLock::new(),
        };
        Ok((ix, r.at))
    }

    /// Share of the name text no live entry uses (renames and removals
    /// leave their old names behind), 0.0 to 1.0.
    pub fn garbage_ratio(&self) -> f64 {
        let used: usize = (0..self.raw_len())
            .filter(|&i| self.flags[i] & F_DEAD == 0)
            .map(|i| {
                self.name_len[i] as usize
                    + if self.flags[i] & F_SAME != 0 {
                        0
                    } else {
                        self.fold_len[i] as usize
                    }
            })
            .sum();
        let total = self.text.len();
        if total == 0 {
            0.0
        } else {
            1.0 - used as f64 / total as f64
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(ix: &mut Index, name: &str, parent: Option<EntryId>) -> EntryId {
        ix.insert(EntryInput {
            name,
            parent,
            is_dir: true,
            last_change: 0,
        })
    }
    fn file(ix: &mut Index, name: &str, parent: EntryId, t: u32) -> EntryId {
        ix.insert(EntryInput {
            name,
            parent: Some(parent),
            is_dir: false,
            last_change: t,
        })
    }

    /// An index of `n` entries read back from its image: every column at
    /// exactly its length, as a warm start has it.
    fn loaded(n: usize) -> Index {
        let mut ix = Index::new();
        let c = dir(&mut ix, "C:", None);
        let mut dirs = vec![c];
        for i in 0..n {
            let parent = dirs[i % dirs.len()];
            if i % 10 == 0 {
                dirs.push(dir(&mut ix, &format!("folder {i}"), Some(parent)));
            } else {
                file(&mut ix, &format!("file number {i}.txt"), parent, 1);
            }
        }
        let mut img = Vec::new();
        ix.to_image(&mut img);
        Index::from_image(&img).unwrap().0
    }

    /// A full column grows by an eighth, not by doubling: a new file
    /// after a warm start must not double every column at once.
    #[test]
    fn a_full_index_grows_by_an_eighth() {
        let mut ix = loaded(20_000);
        let before = ix.heap_bytes();
        let root = EntryId(0);
        file(&mut ix, "new.txt", root, 1);
        let after = ix.heap_bytes();
        assert!(
            after <= before + before / 8 + 64 * 1024,
            "{before} -> {after}"
        );
        // and keeps room for more without growing again
        for i in 0..1000 {
            file(&mut ix, &format!("n{i}.txt"), root, 1);
        }
        assert_eq!(ix.heap_bytes(), after);
    }

    #[test]
    fn a_full_folder_tree_grows_by_an_eighth() {
        let mut ix = loaded(20_000);
        let t = ix.dir_tree();
        assert_eq!(t.order.capacity(), t.order.len());
        assert_eq!(t.pos_of.capacity(), t.pos_of.len());
        dir(&mut ix, "new folder", Some(EntryId(0)));
        let t = ix.dir_tree();
        let fits = |len: usize, cap: usize| cap <= len + room(len, 1024);
        for (col, len, cap) in [
            ("order", t.order.len(), t.order.capacity()),
            ("parent_pos", t.parent_pos.len(), t.parent_pos.capacity()),
            ("class", t.class.len(), t.class.capacity()),
            ("mask", t.mask.len(), t.mask.capacity()),
            ("owner", t.owner.len(), t.owner.capacity()),
            ("rule_node", t.rule_node.len(), t.rule_node.capacity()),
            ("pos_of", t.pos_of.len(), t.pos_of.capacity()),
            ("entry_owner", t.entry_owner.len(), t.entry_owner.capacity()),
        ] {
            assert!(fits(len, cap), "{col}: {len} entries, room for {cap}");
        }
    }

    /// Room asked for on top of full columns is exactly that room.
    #[test]
    fn reserve_on_a_full_index_adds_only_what_is_asked() {
        let mut ix = loaded(20_000);
        let n = ix.raw_len();
        assert_eq!(ix.parent.capacity(), n);
        ix.reserve(100);
        let cap = ix.parent.capacity();
        assert!(cap >= n + 100 && cap < n + 100 + n / 8, "{n} -> {cap}");
    }

    /// Most names (a lowercase ASCII name) equal their folded form: the
    /// text is kept once for them, twice only when the two differ.
    #[test]
    fn a_name_equal_to_its_fold_is_stored_once() {
        let mut ix = Index::new();
        let c = dir(&mut ix, "C:", None);
        let before = ix.text_bytes();
        let a = file(&mut ix, "readme.txt", c, 0);
        assert_eq!(ix.text_bytes() - before, "readme.txt".len());
        let b = file(&mut ix, "Bütçe.xlsx", c, 0);
        assert_eq!(
            ix.text_bytes() - before,
            "readme.txt".len() + "Bütçe.xlsx".len() + "butce.xlsx".len()
        );
        assert_eq!((ix.name(a), ix.folded(a)), ("readme.txt", "readme.txt"));
        assert_eq!((ix.name(b), ix.folded(b)), ("Bütçe.xlsx", "butce.xlsx"));
        // a rename moves an entry between the two forms
        ix.rename(a, "ReadMe.txt", None);
        ix.rename(b, "butce.xlsx", None);
        assert_eq!((ix.name(a), ix.folded(a)), ("ReadMe.txt", "readme.txt"));
        assert_eq!((ix.name(b), ix.folded(b)), ("butce.xlsx", "butce.xlsx"));
        assert_eq!(ix.camel_source(a), "ReadMe.txt");
    }

    #[test]
    fn build_and_path() {
        let mut ix = Index::new();
        let c = dir(&mut ix, "C:", None);
        let u = dir(&mut ix, "Users", Some(c));
        let f = file(&mut ix, "Bütçe.xlsx", u, 5);
        assert_eq!(ix.path(f), "C:\\Users\\Bütçe.xlsx");
        assert_eq!(ix.path(c), "C:\\");
        assert_eq!(ix.folded(f), "butce.xlsx");
        assert_eq!(ix.name(f), "Bütçe.xlsx");
        assert_eq!(ix.drive(f), 'c');
        assert_eq!(ix.child_count(u), 1);
        assert_eq!(ix.last_change(f), 5);
        assert!(!ix.is_dir(f) && ix.is_dir(u));
        assert_eq!(ix.ancestors(f).collect::<Vec<_>>(), vec![u, c]);
    }

    #[test]
    fn remove_tombstones_and_counts() {
        let mut ix = Index::new();
        let c = dir(&mut ix, "C:", None);
        let a = dir(&mut ix, "A", Some(c));
        let f = file(&mut ix, "x.txt", a, 0);
        ix.remove(f);
        assert!(!ix.is_live(f));
        assert_eq!(ix.child_count(a), 0);
        assert_eq!(ix.live_len(), 2);
        ix.remove(f); // idempotent
        assert_eq!(ix.child_count(a), 0);
        assert_eq!(ix.live_len(), 2);
    }

    #[test]
    fn rename_moves_and_refolds() {
        let mut ix = Index::new();
        let c = dir(&mut ix, "C:", None);
        let a = dir(&mut ix, "A", Some(c));
        let b = dir(&mut ix, "B", Some(c));
        let f = file(&mut ix, "x.txt", a, 0);
        ix.rename(f, "Özet.pdf", Some(b));
        assert!(ix.path(f).ends_with("\\B\\Özet.pdf"));
        assert_eq!(ix.folded(f), "ozet.pdf");
        assert_eq!(ix.child_count(a), 0);
        assert_eq!(ix.child_count(b), 1);
        ix.rename(f, "Son.pdf", None); // None keeps the parent
        assert_eq!(ix.path(f), "C:\\B\\Son.pdf");
        ix.touch(f, 99);
        assert_eq!(ix.last_change(f), 99);
    }

    #[test]
    fn deep_and_long() {
        let mut ix = Index::new();
        let mut p = dir(&mut ix, "C:", None);
        for i in 0..40 {
            p = dir(&mut ix, &format!("d{i}"), Some(p));
        }
        let long = "n".repeat(251) + ".txt";
        let f = file(&mut ix, &long, p, 0);
        assert_eq!(ix.path(f).split('\\').count(), 42);
        assert_eq!(ix.ancestors(f).count(), 41);
    }

    #[test]
    fn dir_order_parents_first() {
        let mut ix = Index::new();
        let c = dir(&mut ix, "C:", None);
        let a = dir(&mut ix, "A", Some(c));
        let b = dir(&mut ix, "B", Some(c));
        let a1 = dir(&mut ix, "A1", Some(a));
        let pos = |ix: &Index, id: EntryId| ix.dir_order().iter().position(|&x| x == id.0).unwrap();
        assert!(pos(&ix, c) < pos(&ix, a) && pos(&ix, a) < pos(&ix, a1));
        // move B under A1: B was created before A1, so the cached order must be rebuilt
        ix.rename(b, "B", Some(a1));
        assert!(pos(&ix, a1) < pos(&ix, b));
        // a new dir under B appends after B
        let b1 = dir(&mut ix, "B1", Some(b));
        assert!(pos(&ix, b) < pos(&ix, b1));
        file(&mut ix, "f.txt", b1, 0);
        assert_eq!(ix.dir_order().len(), 5); // C, A, B, A1, B1 — files excluded
    }

    #[test]
    fn path_class_follows_renames() {
        let mut ix = Index::new();
        let c = dir(&mut ix, "C:", None);
        let docs = dir(&mut ix, "Documents", Some(c));
        let f = file(&mut ix, "a.txt", docs, 0);
        assert_eq!(ix.path_class(f), 1);
        let cache = dir(&mut ix, "Temp", Some(docs)); // inserted after the tree was built
        let g = file(&mut ix, "b.tmp", cache, 0);
        assert_eq!(ix.path_class(g), -1, "noise wins over personal");
        ix.rename(cache, "Notes", None);
        assert_eq!(ix.path_class(g), 1);
        ix.rename(docs, "Stuff", None);
        assert_eq!(ix.path_class(f), 0);
        assert_eq!(ix.path_class(c), 0);
        // a dot folder demotes what it holds, not itself
        let vs = dir(&mut ix, ".vscode", Some(docs));
        let h = file(&mut ix, "settings.json", vs, 0);
        assert_eq!(ix.path_class(h), -1);
        assert_eq!(ix.path_class(vs), 0);
    }

    /// A folder holding a `CACHEDIR.TAG` (Cache Directory Tagging, written
    /// by Cargo's `target/` among others) is noise for everything below
    /// it, while the tag lives there.
    #[test]
    fn cachedir_tag_marks_its_folder_noise() {
        let mut ix = Index::new();
        let c = dir(&mut ix, "C:", None);
        let proj = dir(&mut ix, "proj", Some(c));
        let target = dir(&mut ix, "target", Some(proj));
        let src = dir(&mut ix, "src", Some(proj));
        let tag = file(&mut ix, "CACHEDIR.TAG", target, 0);
        let exe = file(&mut ix, "flint.exe", target, 0);
        let main = file(&mut ix, "main.rs", src, 0);
        assert_eq!(ix.path_class(exe), -1);
        assert_eq!(ix.path_class(main), 0);
        ix.remove(tag);
        assert_eq!(ix.path_class(exe), 0, "tag removed");
        let tag2 = file(&mut ix, "CACHEDIR.TAG", target, 0); // after the tree was built
        assert_eq!(ix.path_class(exe), -1, "tag inserted later");
        let tag3 = file(&mut ix, "cachedir.tag", target, 0);
        ix.remove(tag2);
        assert_eq!(ix.path_class(exe), -1, "a second tag keeps it noise");
        ix.rename(tag3, "notes.txt", None);
        assert_eq!(ix.path_class(exe), 0, "tag renamed away");
        ix.rename(tag3, "CACHEDIR.TAG", Some(src));
        assert_eq!(ix.path_class(main), -1, "tag moved into src");
        assert_eq!(ix.path_class(exe), 0);
    }

    /// Only names with a camelCase boundary hand their original to the
    /// typo tier; a rename recomputes it.
    #[test]
    fn camel_source_follows_the_name() {
        let mut ix = Index::new();
        let c = dir(&mut ix, "C:", None);
        let a = ix.insert(EntryInput {
            name: "QuarterlyReport.xlsx",
            parent: Some(c),
            is_dir: false,
            last_change: 0,
        });
        let b = ix.insert(EntryInput {
            name: "Report_2024.xlsx",
            parent: Some(c),
            is_dir: false,
            last_change: 0,
        });
        assert_eq!(ix.camel_source(a), "QuarterlyReport.xlsx");
        assert_eq!(ix.camel_source(b), "");
        ix.rename(a, "quarterly_report.xlsx", None);
        assert_eq!(ix.camel_source(a), "");
        ix.rename(b, "tipiYıllıkTalep.txt", None);
        assert_eq!(ix.camel_source(b), "tipiYıllıkTalep.txt");
    }
    #[test]
    fn heap_bytes_grows() {
        let mut ix = Index::new();
        let c = dir(&mut ix, "C:", None);
        let before = ix.heap_bytes();
        for i in 0..10_000 {
            file(&mut ix, &format!("file_{i}.txt"), c, 0);
        }
        assert!(ix.heap_bytes() > before);
        assert_eq!(ix.child_count(c), 10_000);
    }
}
