//! User-mode indexing: walks folders with `std::fs` in the user's own
//! process (so Windows permissions apply) and keeps a
//! path → entry lookup for applying directory-watch events.
//!
//! Never opens a file: names, folder bit and modified time come from the
//! directory listing. Links (junctions, symlinks) are listed but not
//! followed, so link loops cannot recurse. Other reparse points are real
//! folders and are walked: OneDrive's cloud-file folders are one.

use crate::volindex::FrnMap;
use flint_core::{EntryId, EntryInput, Index};
use std::hash::{BuildHasher, Hash, Hasher};
use std::path::{Component, Path, PathBuf, Prefix};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::UNIX_EPOCH;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct WalkStats {
    pub entries: usize,
    pub unreadable_dirs: usize,
    pub links_skipped: usize,
    /// Folders listed but not entered (see [`Skip`]).
    pub skipped_dirs: usize,
    /// The walk was stopped before it finished.
    pub stopped: bool,
}

/// Lookup of an entry by (parent, name), case-insensitive like Windows.
/// Stores a 64-bit hash per entry, not the name; a hit is confirmed
/// against the index, and a hash collision falls back to a scan.
#[derive(Default)]
pub struct PathMap {
    by_key: FrnMap<u64, u32>,
}

const NO_PARENT: u32 = u32::MAX;

fn key(parent: Option<EntryId>, name: &str) -> u64 {
    let mut h =
        std::hash::BuildHasherDefault::<std::collections::hash_map::DefaultHasher>::default()
            .build_hasher();
    parent.map_or(NO_PARENT, |p| p.0).hash(&mut h);
    for c in name.chars().flat_map(char::to_lowercase) {
        c.hash(&mut h);
    }
    h.finish()
}

fn same_name(a: &str, b: &str) -> bool {
    a.chars()
        .flat_map(char::to_lowercase)
        .eq(b.chars().flat_map(char::to_lowercase))
}

impl PathMap {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn bind(&mut self, ix: &Index, id: EntryId) {
        self.by_key.insert(key(ix.parent(id), ix.name(id)), id.0);
    }

    /// Forgets `id` (call before the index renames or removes it).
    pub fn unbind(&mut self, ix: &Index, id: EntryId) {
        let k = key(ix.parent(id), ix.name(id));
        if self.by_key.get(&k) == Some(&id.0) {
            self.by_key.remove(&k);
        }
    }

    /// The live child `name` of `parent` (`None` = a root entry).
    pub fn child(&self, ix: &Index, parent: Option<EntryId>, name: &str) -> Option<EntryId> {
        let hit = self.by_key.get(&key(parent, name)).map(|&i| EntryId(i));
        if let Some(id) = hit
            && ix.is_live(id)
            && ix.parent(id) == parent
            && same_name(ix.name(id), name)
        {
            return Some(id);
        }
        // a 64-bit hash collision overwrote the slot: find it the slow way
        hit.and_then(|_| {
            (0..ix.raw_len() as u32).map(EntryId).find(|&id| {
                ix.is_live(id) && ix.parent(id) == parent && same_name(ix.name(id), name)
            })
        })
    }

    /// The entry for an absolute path (`C:\Users\x\a.txt`), if indexed.
    pub fn resolve(&self, ix: &Index, path: &Path) -> Option<EntryId> {
        let mut cur: Option<EntryId> = None;
        for part in components(path)? {
            cur = Some(self.child(ix, cur, &part)?);
        }
        cur
    }
}

/// `C:\Users\x` → `["C:", "Users", "x"]`; `None` for a relative path or a
/// UNC share (not indexed).
fn components(path: &Path) -> Option<Vec<String>> {
    let mut out = Vec::new();
    for c in path.components() {
        match c {
            Component::Prefix(p) => match p.kind() {
                Prefix::Disk(d) | Prefix::VerbatimDisk(d) => {
                    out.push(format!("{}:", (d as char).to_ascii_uppercase()))
                }
                _ => return None,
            },
            Component::RootDir | Component::CurDir => {}
            Component::ParentDir => return None,
            Component::Normal(n) => out.push(n.to_string_lossy().into_owned()),
        }
    }
    (!out.is_empty() && out[0].ends_with(':')).then_some(out)
}

fn modified_secs(md: &std::fs::Metadata) -> u32 {
    md.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs().min(u32::MAX as u64) as u32)
}

/// The folder bit as Windows reports it: a junction or folder symlink is
/// a folder (listed as one) even though Rust's file type calls it a link.
#[cfg(windows)]
fn is_dir(md: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    md.file_attributes() & 0x10 != 0
}
#[cfg(not(windows))]
fn is_dir(md: &std::fs::Metadata) -> bool {
    md.is_dir()
}

/// A link to another place: a junction or symlink. On Windows this is a
/// reparse point whose tag is a name surrogate; a reparse point of any
/// other kind (a OneDrive cloud-file folder, a deduplicated file) is the
/// real item and is walked like any folder.
fn is_link(md: &std::fs::Metadata) -> bool {
    md.file_type().is_symlink()
}

/// Inserts (or finds) the folder chain from the drive root down to `dir`
/// and returns `dir`'s entry.
pub fn ensure_dir_chain(ix: &mut Index, map: &mut PathMap, dir: &Path) -> Option<EntryId> {
    let mut cur: Option<EntryId> = None;
    for part in components(dir)? {
        cur = Some(match map.child(ix, cur, &part) {
            Some(id) => id,
            None => {
                let id = ix.insert(EntryInput {
                    name: &part,
                    parent: cur,
                    is_dir: true,
                    last_change: 0,
                });
                map.bind(ix, id);
                id
            }
        });
    }
    cur
}

/// Folders a walk lists but does not enter, and whose watch events are
/// dropped, except below a root that lies inside one of them (the Start
/// menu inside a skipped AppData). Compared case-insensitively, like
/// Windows.
#[derive(Clone, Debug, Default)]
pub struct Skip {
    dirs: Vec<String>,
    keep: Vec<String>,
}

fn path_key(p: &Path) -> String {
    p.to_string_lossy().trim_end_matches('\\').to_lowercase()
}

/// `k` is `base` or lies below it (keys from [`path_key`]).
fn key_within(k: &str, base: &str) -> bool {
    k.strip_prefix(base)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('\\'))
}

/// `p` is `base` or lies below it, case-insensitively.
pub(crate) fn is_within(p: &Path, base: &Path) -> bool {
    key_within(&path_key(p), &path_key(base))
}

impl Skip {
    /// Skips `dirs`, except inside those of `roots` that lie in one.
    pub fn new(dirs: &[PathBuf], roots: &[PathBuf]) -> Skip {
        let dirs: Vec<String> = dirs.iter().map(|d| path_key(d)).collect();
        let keep = roots
            .iter()
            .map(|r| path_key(r))
            .filter(|r| dirs.iter().any(|d| key_within(r, d)))
            .collect();
        Skip { dirs, keep }
    }

    /// A folder the walk lists but does not enter.
    fn is_skipped_dir(&self, p: &Path) -> bool {
        if self.dirs.is_empty() {
            return false;
        }
        let k = path_key(p);
        self.dirs.contains(&k)
    }

    /// `p` lies inside a skipped folder (not the folder itself) and not
    /// inside a root kept there.
    fn hides(&self, p: &Path) -> bool {
        if self.dirs.is_empty() {
            return false;
        }
        let k = path_key(p);
        self.dirs
            .iter()
            .any(|d| k.len() > d.len() && key_within(&k, d))
            && !self.keep.iter().any(|r| key_within(&k, r))
    }

    /// The event to apply, if any: a move across the edge of a skipped
    /// folder becomes an add or a removal.
    pub fn filter(&self, ev: FsEvent) -> Option<FsEvent> {
        match ev {
            FsEvent::Added(ref p) | FsEvent::Removed(ref p) | FsEvent::Modified(ref p) => {
                (!self.hides(p)).then_some(ev)
            }
            FsEvent::Renamed(a, b) => match (self.hides(&a), self.hides(&b)) {
                (true, true) => None,
                (true, false) => Some(FsEvent::Added(b)),
                (false, true) => Some(FsEvent::Removed(a)),
                (false, false) => Some(FsEvent::Renamed(a, b)),
            },
            FsEvent::Overflow => Some(ev),
        }
    }
}

/// Indexes everything below `root` (which is created with its parents if
/// needed). `on_progress` gets the running entry count every 10 000.
pub fn walk(
    ix: &mut Index,
    map: &mut PathMap,
    root: &Path,
    on_progress: impl FnMut(usize),
) -> WalkStats {
    walk_until(ix, map, root, &AtomicBool::new(false), on_progress)
}

/// [`walk`] that ends early once `stop` is set: checked at every entry,
/// so a large folder read slowly does not hold the stop until it is listed.
pub fn walk_until(
    ix: &mut Index,
    map: &mut PathMap,
    root: &Path,
    stop: &AtomicBool,
    on_progress: impl FnMut(usize),
) -> WalkStats {
    walk_scoped(ix, map, root, &Skip::default(), stop, on_progress)
}

/// [`walk_until`] that lists the folders in `skip` without entering them.
pub fn walk_scoped(
    ix: &mut Index,
    map: &mut PathMap,
    root: &Path,
    skip: &Skip,
    stop: &AtomicBool,
    mut on_progress: impl FnMut(usize),
) -> WalkStats {
    let mut stats = WalkStats::default();
    let Some(rid) = ensure_dir_chain(ix, map, root) else {
        stats.unreadable_dirs += 1;
        return stats;
    };
    walk_below(
        ix,
        map,
        (root.to_path_buf(), rid),
        &mut stats,
        &mut on_progress,
        stop,
        skip,
    );
    stats
}

fn walk_below(
    ix: &mut Index,
    map: &mut PathMap,
    start: (PathBuf, EntryId),
    stats: &mut WalkStats,
    on_progress: &mut impl FnMut(usize),
    stop: &AtomicBool,
    skip: &Skip,
) {
    let mut stack = vec![start];
    while let Some((dir, id)) = stack.pop() {
        if stop.load(Ordering::Relaxed) {
            stats.stopped = true;
            return;
        }
        let Ok(rd) = std::fs::read_dir(&dir) else {
            stats.unreadable_dirs += 1;
            continue;
        };
        for e in rd.flatten() {
            if stop.load(Ordering::Relaxed) {
                stats.stopped = true;
                return;
            }
            // DirEntry::metadata comes from the directory listing on Windows
            let Ok(md) = e.metadata() else { continue };
            let name = e.file_name().to_string_lossy().into_owned();
            let is_dir = is_dir(&md);
            let child = upsert(ix, map, id, &name, is_dir, modified_secs(&md));
            stats.entries += 1;
            if stats.entries.is_multiple_of(10_000) {
                on_progress(stats.entries);
            }
            if is_dir {
                let path = e.path();
                if is_link(&md) {
                    stats.links_skipped += 1;
                } else if skip.is_skipped_dir(&path) {
                    stats.skipped_dirs += 1;
                } else {
                    stack.push((path, child));
                }
            }
        }
    }
}

/// A change seen by the directory watch, with absolute paths.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FsEvent {
    Added(PathBuf),
    Removed(PathBuf),
    Renamed(PathBuf, PathBuf),
    Modified(PathBuf),
    /// Changes were lost (buffer overrun): walk the root again.
    Overflow,
}

/// The entry for `name` in `parent`: the existing one when it is still
/// the same kind, else a new one (a file replaced by a folder, or back,
/// gets a fresh entry; the stale one goes with everything below it).
fn upsert(
    ix: &mut Index,
    map: &mut PathMap,
    parent: EntryId,
    name: &str,
    is_dir: bool,
    mtime: u32,
) -> EntryId {
    if let Some(c) = map.child(ix, Some(parent), name) {
        if ix.is_dir(c) == is_dir {
            ix.touch(c, mtime);
            return c;
        }
        remove_tree(ix, map, c);
    }
    let c = ix.insert(EntryInput {
        name,
        parent: Some(parent),
        is_dir,
        last_change: mtime,
    });
    map.bind(ix, c);
    c
}

/// Removes `id` and, for a folder, everything still below it.
fn remove_tree(ix: &mut Index, map: &mut PathMap, id: EntryId) {
    if ix.is_dir(id) && ix.child_count(id) > 0 {
        let inside: Vec<EntryId> = (0..ix.raw_len() as u32)
            .map(EntryId)
            .filter(|&x| ix.is_live(x) && ix.ancestors(x).any(|a| a == id))
            .collect();
        for x in inside {
            map.unbind(ix, x);
            ix.remove(x);
        }
    }
    map.unbind(ix, id);
    ix.remove(id);
}

/// Applies one watch event. Returns `true` when the caller must walk the
/// watched root again (`Overflow`).
pub fn apply_fs_event(ix: &mut Index, map: &mut PathMap, ev: &FsEvent) -> bool {
    match ev {
        FsEvent::Overflow => return true,
        FsEvent::Added(p) => add_path(ix, map, p),
        FsEvent::Modified(p) => {
            if let (Some(id), Ok(md)) = (map.resolve(ix, p), std::fs::symlink_metadata(p)) {
                ix.touch(id, modified_secs(&md));
            }
        }
        FsEvent::Removed(p) => {
            if let Some(id) = map.resolve(ix, p) {
                remove_tree(ix, map, id);
            }
        }
        FsEvent::Renamed(old, new) => {
            let parent = new.parent().and_then(|d| map.resolve(ix, d));
            let name = new.file_name().map(|n| n.to_string_lossy().into_owned());
            match (map.resolve(ix, old), parent, name) {
                (Some(id), Some(parent), Some(name)) => {
                    if let Some(other) = map.child(ix, Some(parent), &name)
                        && other != id
                    {
                        remove_tree(ix, map, other); // replaced an existing entry
                    }
                    map.unbind(ix, id);
                    ix.rename(id, &name, Some(parent));
                    map.bind(ix, id);
                }
                // moved in from an unwatched place, or the old name was never seen
                (None, _, _) => add_path(ix, map, new),
                // moved out of the indexed tree
                (Some(id), _, _) => remove_tree(ix, map, id),
            }
        }
    }
    false
}

/// Indexes `p` (and, for a folder, everything inside: a folder moved in
/// arrives as one event).
fn add_path(ix: &mut Index, map: &mut PathMap, p: &Path) {
    let Ok(md) = std::fs::symlink_metadata(p) else {
        return;
    }; // gone again already
    let (Some(dir), Some(name)) = (p.parent(), p.file_name()) else {
        return;
    };
    let Some(parent) = map.resolve(ix, dir) else {
        return;
    };
    let name = name.to_string_lossy().into_owned();
    let id = upsert(ix, map, parent, &name, is_dir(&md), modified_secs(&md));
    if is_dir(&md) && !is_link(&md) {
        let mut stats = WalkStats::default();
        let never = AtomicBool::new(false);
        walk_below(
            ix,
            map,
            (p.to_path_buf(), id),
            &mut stats,
            &mut |_| {},
            &never,
            &Skip::default(),
        );
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("flint-walk-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d.canonicalize()
            .map(|p| PathBuf::from(p.to_string_lossy().trim_start_matches(r"\\?\")))
            .unwrap()
    }

    /// Every live entry's path (with the folder bit), sorted.
    pub(crate) fn paths_under(ix: &Index, root: &Path) -> Vec<String> {
        let prefix = format!("{}\\", root.display()).to_lowercase();
        let mut v: Vec<String> = (0..ix.raw_len() as u32)
            .map(EntryId)
            .filter(|&id| ix.is_live(id))
            .map(|id| format!("{}{}", ix.path(id), if ix.is_dir(id) { "\\" } else { "" }))
            .filter(|p| p.to_lowercase().starts_with(&prefix) && p.to_lowercase() != prefix)
            .collect();
        v.sort();
        v
    }

    #[test]
    fn walk_stops_inside_a_large_folder() {
        // one folder read slowly (a cold or network disk) must not hold a
        // stop until the whole folder is listed
        let d = tmp("big-folder");
        for i in 0..10_500 {
            std::fs::File::create(d.join(format!("f{i}"))).unwrap();
        }
        let (mut ix, mut map) = (Index::new(), PathMap::new());
        let stop = AtomicBool::new(false);
        let s = walk_until(&mut ix, &mut map, &d, &stop, |_| {
            stop.store(true, Ordering::Relaxed)
        });
        assert!(s.stopped);
        assert_eq!(s.entries, 10_000, "entries listed after the stop");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn components_of_paths() {
        assert_eq!(
            components(Path::new(r"c:\Users\x")).unwrap(),
            vec!["C:", "Users", "x"]
        );
        assert_eq!(components(Path::new(r"\\?\C:\a")).unwrap(), vec!["C:", "a"]);
        assert_eq!(components(Path::new(r"C:\")).unwrap(), vec!["C:"]);
        assert!(components(Path::new(r"\\server\share\a")).is_none());
        assert!(components(Path::new(r"rel\a")).is_none());
    }

    #[test]
    fn walk_finds_names_and_skips_links() {
        let d = tmp("names");
        std::fs::create_dir_all(d.join(r"a\b")).unwrap();
        std::fs::write(d.join(r"a\b\c.txt"), b"x").unwrap();
        std::fs::write(d.join("Bütçe.xlsx"), b"x").unwrap();
        std::fs::create_dir_all(d.join("target")).unwrap();
        std::fs::write(
            d.join(r"target\CACHEDIR.TAG"),
            b"Signature: 8a477f597d28d172789f06886806bc55",
        )
        .unwrap();
        std::fs::write(d.join(r"target\out.exe"), b"x").unwrap();
        // a junction back to the root: a loop if followed (mklink /J needs no admin)
        let made = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(d.join(r"a\loop"))
            .arg(&d)
            .output()
            .is_ok_and(|o| o.status.success());
        let mut ix = Index::new();
        let mut map = PathMap::new();
        let s = walk(&mut ix, &mut map, &d, |_| {});
        let p = paths_under(&ix, &d);
        let base = d.display().to_string();
        let mut want = vec![
            format!(r"{base}\Bütçe.xlsx"),
            format!(r"{base}\a\"),
            format!(r"{base}\a\b\"),
            format!(r"{base}\a\b\c.txt"),
            format!(r"{base}\target\"),
            format!(r"{base}\target\CACHEDIR.TAG"),
            format!(r"{base}\target\out.exe"),
        ];
        if made {
            want.push(format!(r"{base}\a\loop\"));
            assert_eq!(s.links_skipped, 1);
        }
        want.sort();
        assert_eq!(p, want);
        assert_eq!(s.entries, want.len());
        let exe = map
            .resolve(&ix, &d.join(r"TARGET\OUT.EXE"))
            .expect("case-insensitive lookup");
        assert_eq!(ix.path_class(exe), -1, "CACHEDIR.TAG folder is noise");
        // walking again finds the same entries, no duplicates
        walk(&mut ix, &mut map, &d, |_| {});
        assert_eq!(paths_under(&ix, &d), p);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Marks folder `d` as a reparse point with a third-party tag that is
    /// not a name surrogate: the kind OneDrive's cloud-file folders are.
    /// No admin rights needed. `false` when the volume refused it.
    fn mark_reparse_not_link(d: &Path) -> bool {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Foundation::{CloseHandle, GENERIC_WRITE, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::Storage::FileSystem::{
            CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ,
            OPEN_EXISTING,
        };
        use windows_sys::Win32::System::IO::DeviceIoControl;
        use windows_sys::Win32::System::Ioctl::FSCTL_SET_REPARSE_POINT;
        let w: Vec<u16> = d.as_os_str().encode_wide().chain(Some(0)).collect();
        // SAFETY: w is a NUL-terminated path; the handle is closed below.
        let h = unsafe {
            CreateFileW(
                w.as_ptr(),
                GENERIC_WRITE,
                FILE_SHARE_READ,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                std::ptr::null_mut(),
            )
        };
        if h == INVALID_HANDLE_VALUE {
            return false;
        }
        // REPARSE_GUID_DATA_BUFFER: tag 0x99 (not Microsoft's, not a name
        // surrogate), 4 data bytes, any GUID
        let mut buf = Vec::new();
        buf.extend(0x99u32.to_le_bytes());
        buf.extend(4u16.to_le_bytes());
        buf.extend(0u16.to_le_bytes());
        buf.extend([7u8; 16]);
        buf.extend([1u8, 2, 3, 4]);
        let mut n = 0u32;
        // SAFETY: buf is a complete reparse buffer of the given length.
        let ok = unsafe {
            DeviceIoControl(
                h,
                FSCTL_SET_REPARSE_POINT,
                buf.as_ptr().cast(),
                buf.len() as u32,
                std::ptr::null_mut(),
                0,
                &mut n,
                std::ptr::null_mut(),
            )
        } != 0;
        // SAFETY: h is our open handle.
        unsafe { CloseHandle(h) };
        ok
    }

    /// OneDrive's folders are reparse points (cloud files) but real
    /// folders: skipping every reparse point left all of OneDrive out.
    #[test]
    fn walk_enters_a_reparse_folder_that_is_not_a_link() {
        let d = tmp("cloud");
        std::fs::create_dir_all(d.join("cloud")).unwrap();
        assert!(mark_reparse_not_link(&d.join("cloud")), "reparse tag set");
        let mut ix = Index::new();
        let mut map = PathMap::new();
        let s = walk(&mut ix, &mut map, &d, |_| {});
        // no filter handles tag 0x99 here, so opening it fails: what
        // matters is that the walk tried, instead of calling it a link
        assert_eq!(s.links_skipped, 0, "not a link");
        assert_eq!(s.unreadable_dirs, 1, "walked into");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// An entry whose kind changed on disk (file replaced
    /// by a folder, or back) is replaced, not reused: children of a stale
    /// file entry broke the privacy filter and the folder tree.
    #[test]
    fn a_changed_kind_replaces_the_entry() {
        let d = tmp("kind");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("x"), b"x").unwrap();
        std::fs::create_dir_all(d.join("y")).unwrap();
        std::fs::write(d.join(r"y\eski.txt"), b"x").unwrap();
        let mut ix = Index::new();
        let mut map = PathMap::new();
        walk(&mut ix, &mut map, &d, |_| {});
        std::fs::remove_file(d.join("x")).unwrap();
        std::fs::create_dir_all(d.join(r"x\alt")).unwrap();
        std::fs::write(d.join(r"x\ic.txt"), b"x").unwrap();
        std::fs::remove_dir_all(d.join("y")).unwrap();
        std::fs::write(d.join("y"), b"x").unwrap();
        walk(&mut ix, &mut map, &d, |_| {}); // what an Overflow triggers
        let base = d.display().to_string();
        assert_eq!(
            paths_under(&ix, &d),
            vec![
                format!(r"{base}\x\"),
                format!(r"{base}\x\alt\"),
                format!(r"{base}\x\ic.txt"),
                format!(r"{base}\y")
            ]
        );
        // the same through a watch event
        std::fs::remove_dir_all(d.join("x")).unwrap();
        std::fs::write(d.join("x"), b"x").unwrap();
        apply_fs_event(&mut ix, &mut map, &FsEvent::Added(d.join("x")));
        assert_eq!(
            paths_under(&ix, &d),
            vec![format!(r"{base}\x"), format!(r"{base}\y")]
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn apply_events_keeps_paths() {
        let d = tmp("apply");
        std::fs::create_dir_all(d.join(r"proje\alt")).unwrap();
        std::fs::write(d.join(r"proje\alt\rapor.docx"), b"x").unwrap();
        std::fs::write(d.join("eski.txt"), b"x").unwrap();
        let mut ix = Index::new();
        let mut map = PathMap::new();
        walk(&mut ix, &mut map, &d, |_| {});
        // change the disk, then apply the events Windows would send
        std::fs::create_dir_all(d.join("arsiv")).unwrap();
        std::fs::rename(d.join("proje"), d.join(r"arsiv\proje-2025")).unwrap();
        std::fs::rename(d.join("eski.txt"), d.join("yeni.txt")).unwrap();
        let outside = tmp("apply-outside");
        std::fs::create_dir_all(outside.join(r"gelen\ic")).unwrap();
        std::fs::write(outside.join(r"gelen\ic\not.md"), b"x").unwrap();
        std::fs::rename(outside.join("gelen"), d.join("gelen")).unwrap();
        for ev in [
            FsEvent::Added(d.join("arsiv")),
            FsEvent::Renamed(d.join("proje"), d.join(r"arsiv\proje-2025")),
            FsEvent::Renamed(d.join("eski.txt"), d.join("yeni.txt")),
            FsEvent::Added(d.join("gelen")),
            FsEvent::Added(d.join("gelen")), // repeated: no duplicate
        ] {
            assert!(!apply_fs_event(&mut ix, &mut map, &ev));
        }
        let mut fresh = Index::new();
        let mut fresh_map = PathMap::new();
        walk(&mut fresh, &mut fresh_map, &d, |_| {});
        assert_eq!(paths_under(&ix, &d), paths_under(&fresh, &d));
        std::fs::remove_dir_all(d.join("arsiv")).unwrap();
        apply_fs_event(&mut ix, &mut map, &FsEvent::Removed(d.join("arsiv")));
        assert!(
            map.resolve(&ix, &d.join(r"arsiv\proje-2025\alt\rapor.docx"))
                .is_none()
        );
        assert!(!paths_under(&ix, &d).iter().any(|p| p.contains("arsiv")));
        assert!(apply_fs_event(&mut ix, &mut map, &FsEvent::Overflow));
        let _ = std::fs::remove_dir_all(&d);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn walk_does_not_enter_a_skipped_folder() {
        let d = tmp("skip");
        std::fs::create_dir_all(d.join(r"AppData\Local\Temp")).unwrap();
        std::fs::write(d.join(r"AppData\Local\Temp\x.tmp"), b"x").unwrap();
        std::fs::write(d.join("belge.txt"), b"x").unwrap();
        let (mut ix, mut map) = (Index::new(), PathMap::new());
        let skip = Skip::new(&[d.join(r"appdata\LOCAL")], std::slice::from_ref(&d));
        let s = walk_scoped(
            &mut ix,
            &mut map,
            &d,
            &skip,
            &AtomicBool::new(false),
            |_| {},
        );
        assert_eq!(s.skipped_dirs, 1);
        assert!(map.resolve(&ix, &d.join("belge.txt")).is_some());
        assert!(
            map.resolve(&ix, &d.join(r"AppData\Local")).is_some(),
            "the skipped folder itself is listed"
        );
        assert!(map.resolve(&ix, &d.join(r"AppData\Local\Temp")).is_none());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn skip_drops_events_inside_a_skipped_folder_but_not_inside_a_root_there() {
        let home = PathBuf::from(r"C:\Users\Deniz");
        let app = home.join(r"AppData\Roaming");
        let menu = app.join(r"Microsoft\Windows\Start Menu\Programs");
        let skip = Skip::new(std::slice::from_ref(&app), &[home.clone(), menu.clone()]);
        let inside = app.join(r"Oyun\kayit.sav");
        let outside = home.join(r"Documents\rapor.txt");
        let lnk = menu.join("Discord.lnk");
        assert_eq!(skip.filter(FsEvent::Added(inside.clone())), None);
        assert_eq!(skip.filter(FsEvent::Modified(app.join("X"))), None);
        assert_eq!(
            skip.filter(FsEvent::Added(outside.clone())),
            Some(FsEvent::Added(outside.clone()))
        );
        assert_eq!(
            skip.filter(FsEvent::Added(lnk.clone())),
            Some(FsEvent::Added(lnk.clone()))
        );
        // case-insensitive, like Windows
        let upper = PathBuf::from(r"C:\USERS\DENIZ\APPDATA\ROAMING\Y");
        assert_eq!(skip.filter(FsEvent::Removed(upper)), None);
        // the folder itself is an entry: its own events still count
        assert_eq!(
            skip.filter(FsEvent::Removed(app.clone())),
            Some(FsEvent::Removed(app.clone()))
        );
        // moves across the edge become an add or a removal
        assert_eq!(
            skip.filter(FsEvent::Renamed(inside.clone(), outside.clone())),
            Some(FsEvent::Added(outside.clone()))
        );
        assert_eq!(
            skip.filter(FsEvent::Renamed(outside.clone(), inside.clone())),
            Some(FsEvent::Removed(outside.clone()))
        );
        assert_eq!(skip.filter(FsEvent::Overflow), Some(FsEvent::Overflow));
        // a name that only starts like the skipped folder is not inside it
        let near = home.join(r"AppData\RoamingNotes\a.txt");
        assert_eq!(
            skip.filter(FsEvent::Added(near.clone())),
            Some(FsEvent::Added(near))
        );
        // nothing skipped: everything passes
        let none = Skip::default();
        assert_eq!(
            none.filter(FsEvent::Added(inside.clone())),
            Some(FsEvent::Added(inside))
        );
    }
}
