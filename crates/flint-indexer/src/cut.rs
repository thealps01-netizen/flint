//! Folders whose contents a volume leaves out: each profile's hidden app
//! data folders (`AppData\Local`, `\Roaming`, `\LocalLow`). They hold
//! program caches, settings and temp files rather than files people look
//! for, and most of a disk's entries; Windows' own search leaves them out
//! by default too. The folders themselves stay listed, and so does the
//! Start menu kept in Roaming, so apps are still found by name. Pure: no
//! OS calls.

use crate::privacy::Machine;
use crate::usn::UsnRecord;
use crate::volindex::FrnHasher;
use std::collections::{HashMap, HashSet};
use std::hash::BuildHasherDefault;

type FrnMap<V> = HashMap<u64, V, BuildHasherDefault<FrnHasher>>;
type FrnSet = HashSet<u64, BuildHasherDefault<FrnHasher>>;

/// The one path kept inside a skipped Roaming folder, a name per level
/// (compared without case): the Start menu.
pub const KEEP: [&str; 3] = ["microsoft", "windows", "start menu"];

/// How far into [`KEEP`] a skipped folder is: its child named
/// `KEEP[depth]` stays listed; at `KEEP.len()` no child does.
pub type Depth = u8;

const NONE_KEPT: Depth = KEEP.len() as Depth;

/// A folder to skip, by its path below the volume root: lowercase names,
/// `*` for any one name.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Rule {
    pub path: Vec<String>,
    pub depth: Depth,
}

/// Where an entry stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Place {
    /// Indexed as usual.
    Shown,
    /// Listed, but only its child on the [`KEEP`] path is.
    Cut(Depth),
    /// Not indexed.
    Hidden,
}

fn names(path: &str) -> Vec<String> {
    path.split(['\\', '/'])
        .filter(|s| !s.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// The rules for `drive` (`"C:"`) on machine `m`: the app data folders of
/// every folder in the profiles folder (`C:\Users\*`), under each of its
/// spellings, and of every profile kept elsewhere. Sorted, so the same
/// machine always gives the same list (a saved volume is compared by it).
pub fn app_data_rules(m: &Machine, drive: &str) -> Vec<Rule> {
    let drive = drive.to_lowercase();
    let below = |path: &str| -> Option<Vec<String>> {
        let mut n = names(path);
        (!n.is_empty() && n[0] == drive).then(|| n.split_off(1))
    };
    let mut bases: Vec<Vec<String>> = Vec::new();
    let dirs: Vec<Vec<String>> = std::iter::once(&m.profiles_dir)
        .chain(&m.profiles_dir_aliases)
        .map(|d| names(d))
        .collect();
    for d in std::iter::once(&m.profiles_dir).chain(&m.profiles_dir_aliases) {
        if let Some(mut b) = below(d) {
            b.push("*".into());
            bases.push(b);
        }
    }
    for p in &m.profiles {
        let n = names(&p.path);
        let in_dir = n.len() > 1 && dirs.iter().any(|d| *d == n[..n.len() - 1]);
        if let Some(b) = below(&p.path)
            && !in_dir
            && !b.is_empty()
        {
            bases.push(b);
        }
    }
    let mut rules: Vec<Rule> = Vec::new();
    for b in bases {
        for (leaf, depth) in [
            ("local", NONE_KEPT),
            ("locallow", NONE_KEPT),
            ("roaming", 0),
        ] {
            let mut path = b.clone();
            path.extend(["appdata".to_string(), leaf.to_string()]);
            let r = Rule { path, depth };
            rules.push(r);
        }
    }
    rules.sort();
    rules.dedup();
    rules
}

/// What a volume leaves out, and how it got there.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Cut {
    rules: Vec<Rule>,
    /// Listed folders whose contents are left out (but for [`KEEP`]).
    cut: FrnMap<Depth>,
    /// Folders left out with everything in them.
    hidden: FrnSet,
    /// Folders that came out of hidden ones: what they hold was never
    /// read.
    surfaced: Vec<u64>,
    /// A skipped folder moved, or an indexed one became skipped: what the
    /// volume holds is not known.
    stale: bool,
}

fn matches(rule: &str, name: &str) -> bool {
    rule == "*" || rule == name.to_lowercase()
}

/// Where a child of a folder at `parent` stands.
fn child_place(parent: Place, name: &str, is_dir: bool) -> Place {
    match parent {
        Place::Shown => Place::Shown,
        Place::Hidden => Place::Hidden,
        Place::Cut(d) => match KEEP.get(d as usize) {
            Some(k) if is_dir && name.eq_ignore_ascii_case(k) => {
                if d as usize + 1 == KEEP.len() {
                    Place::Shown
                } else {
                    Place::Cut(d + 1)
                }
            }
            _ => Place::Hidden,
        },
    }
}

impl Cut {
    pub fn new(rules: Vec<Rule>) -> Cut {
        Cut {
            rules,
            ..Cut::default()
        }
    }

    /// The saved state of [`Cut::parts`], following `rules`.
    pub fn from_parts(rules: Vec<Rule>, cut: &[(u64, Depth)], hidden: &[u64]) -> Cut {
        Cut {
            rules,
            cut: cut.iter().copied().collect(),
            hidden: hidden.iter().copied().collect(),
            surfaced: Vec::new(),
            stale: false,
        }
    }

    /// The listed-but-skipped folders and the hidden ones, sorted.
    pub fn parts(&self) -> (Vec<(u64, Depth)>, Vec<u64>) {
        let mut cut: Vec<(u64, Depth)> = self.cut.iter().map(|(&f, &d)| (f, d)).collect();
        cut.sort_unstable();
        let mut hidden: Vec<u64> = self.hidden.iter().copied().collect();
        hidden.sort_unstable();
        (cut, hidden)
    }

    pub fn rules(&self) -> &[Rule] {
        &self.rules
    }

    /// The depth of a skipped folder.
    pub fn depth(&self, frn: u64) -> Option<Depth> {
        self.cut.get(&frn).copied()
    }

    /// `frn` is a folder left out with its contents.
    pub fn hides(&self, frn: u64) -> bool {
        self.hidden.contains(&frn)
    }

    /// Where a record whose parent is `parent` stands.
    pub fn place(&self, parent: u64, name: &str, is_dir: bool) -> Place {
        let p = if self.hidden.contains(&parent) {
            Place::Hidden
        } else if let Some(&d) = self.cut.get(&parent) {
            Place::Cut(d)
        } else {
            Place::Shown
        };
        child_place(p, name, is_dir)
    }

    /// The depth of a folder whose path below the volume root is `path`
    /// (lowercase) when a rule names it.
    pub fn rule_depth(&self, path: &[String]) -> Option<Depth> {
        self.rules
            .iter()
            .find(|r| {
                r.path.len() == path.len() && r.path.iter().zip(path).all(|(r, n)| matches(r, n))
            })
            .map(|r| r.depth)
    }

    /// `path` (lowercase, below the volume root) leads to a folder a rule
    /// names, without being that folder.
    pub fn rule_prefix(&self, path: &[String]) -> bool {
        self.rules.iter().any(|r| {
            r.path.len() > path.len() && r.path.iter().zip(path).all(|(r, n)| matches(r, n))
        })
    }

    /// The skipped folders.
    pub fn skipped(&self) -> impl Iterator<Item = u64> + '_ {
        self.cut.keys().copied()
    }

    /// A rule may name a folder called `name` (cheap test before a path
    /// is built).
    pub fn may_name(&self, name: &str) -> bool {
        self.rules
            .iter()
            .any(|r| r.path.last().is_some_and(|l| matches(l, name)))
    }

    pub fn set_cut(&mut self, frn: u64, depth: Depth) {
        self.cut.insert(frn, depth);
    }

    pub fn hide(&mut self, frn: u64) {
        self.hidden.insert(frn);
    }

    /// Forgets a deleted `frn`; true when it was hidden (not indexed).
    pub fn forget(&mut self, frn: u64) -> bool {
        self.cut.remove(&frn);
        self.hidden.remove(&frn)
    }

    /// `frn` leaves the hidden folders; true when it was in them (its
    /// contents are then to be read: [`Cut::take_surfaced`]).
    pub fn unhide(&mut self, frn: u64) -> bool {
        let was = self.hidden.remove(&frn);
        if was {
            self.surfaced.push(frn);
        }
        was
    }

    /// The folders that came out of hidden ones since the last call.
    pub fn take_surfaced(&mut self) -> Vec<u64> {
        std::mem::take(&mut self.surfaced)
    }

    pub fn mark_stale(&mut self) {
        self.stale = true;
    }

    /// Whether the volume became stale since the last call.
    pub fn take_stale(&mut self) -> bool {
        std::mem::take(&mut self.stale)
    }

    /// Drops from `records` (a volume read in any order) what the rules
    /// leave out, and remembers the skipped and hidden folders.
    pub fn filter(&mut self, root_frn: u64, records: &mut Vec<UsnRecord>) {
        if self.rules.is_empty() {
            return;
        }
        // the skipped folders, one level of the rules' paths per pass
        let deepest = self.rules.iter().map(|r| r.path.len()).max().unwrap_or(0);
        let mut front: FrnMap<Vec<usize>> = FrnMap::default();
        front.insert(root_frn, (0..self.rules.len()).collect());
        for level in 0..deepest {
            let mut next: FrnMap<Vec<usize>> = FrnMap::default();
            for r in records.iter().filter(|r| r.is_dir() && r.frn != root_frn) {
                let Some(rules) = front.get(&r.parent) else {
                    continue;
                };
                for &k in rules {
                    let rule = &self.rules[k];
                    if level < rule.path.len() && matches(&rule.path[level], &r.name) {
                        if level + 1 == rule.path.len() {
                            self.cut.insert(r.frn, rule.depth);
                        } else {
                            next.entry(r.frn).or_default().push(k);
                        }
                    }
                }
            }
            if next.is_empty() {
                break;
            }
            front = next;
        }
        if self.cut.is_empty() {
            return;
        }
        // the place of every folder, from its nearest known ancestor down
        let mut at: FrnMap<u32> = FrnMap::default();
        for (i, r) in records.iter().enumerate() {
            if r.is_dir() {
                at.insert(r.frn, i as u32);
            }
        }
        let mut memo: FrnMap<Place> = self.cut.iter().map(|(&f, &d)| (f, Place::Cut(d))).collect();
        let mut chain: Vec<u32> = Vec::new();
        let mut place_of = |frn: u64, memo: &mut FrnMap<Place>| -> Place {
            let mut cur = frn;
            let base = loop {
                if cur == root_frn {
                    break Place::Shown;
                }
                if let Some(&p) = memo.get(&cur) {
                    break p;
                }
                match at.get(&cur) {
                    // an unknown parent or a cycle: the volume puts it with
                    // the orphans, hidden from everyone anyway
                    Some(&i) if chain.len() < 1 << 16 => {
                        chain.push(i);
                        cur = records[i as usize].parent;
                    }
                    _ => break Place::Shown,
                }
            };
            let mut p = base;
            while let Some(i) = chain.pop() {
                let r = &records[i as usize];
                p = match memo.get(&r.frn) {
                    Some(&m) => m,
                    None => child_place(p, &r.name, true),
                };
                memo.insert(r.frn, p);
            }
            p
        };
        let places: Vec<Place> = records
            .iter()
            .map(|r| {
                if r.is_dir() {
                    place_of(r.frn, &mut memo)
                } else {
                    child_place(place_of(r.parent, &mut memo), &r.name, false)
                }
            })
            .collect();
        for (r, &p) in records.iter().zip(&places) {
            match p {
                Place::Hidden if r.is_dir() => {
                    self.hidden.insert(r.frn);
                }
                Place::Cut(d) => {
                    self.cut.insert(r.frn, d);
                }
                _ => {}
            }
        }
        let mut i = 0;
        records.retain(|_| {
            i += 1;
            places[i - 1] != Place::Hidden
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::privacy::Profile;
    use crate::usn::FILE_ATTRIBUTE_DIRECTORY;

    const ROOT: u64 = 5;

    fn rec(frn: u64, parent: u64, name: &str, dir: bool) -> UsnRecord {
        UsnRecord {
            frn,
            parent,
            usn: 0,
            time: 1000,
            reason: 0,
            attrs: if dir { FILE_ATTRIBUTE_DIRECTORY } else { 0x20 },
            security_id: 0,
            name: name.into(),
        }
    }

    fn machine() -> Machine {
        Machine {
            profiles: vec![
                Profile {
                    sid: "S-1-5-21-1-2-3-1001".into(),
                    path: r"C:\Users\deniz".into(),
                },
                Profile {
                    sid: "S-1-5-21-1-2-3-1002".into(),
                    path: r"D:\Home\ayse".into(),
                },
            ],
            profiles_dir: r"C:\Users".into(),
            profiles_dir_aliases: vec![r"C:\Kullanicilar".into()],
            public_dir: Some(r"C:\Users\Public".into()),
            public_dir_aliases: Vec::new(),
            windows_dir: r"C:\Windows".into(),
        }
    }

    fn paths(rules: &[Rule]) -> Vec<String> {
        rules
            .iter()
            .map(|r| format!("{} {}", r.path.join(r"\"), r.depth))
            .collect()
    }

    #[test]
    fn rules_cover_the_profiles_folder_and_profiles_kept_elsewhere() {
        let m = machine();
        assert_eq!(
            paths(&app_data_rules(&m, "C:")),
            [
                r"kullanicilar\*\appdata\local 3",
                r"kullanicilar\*\appdata\locallow 3",
                r"kullanicilar\*\appdata\roaming 0",
                r"users\*\appdata\local 3",
                r"users\*\appdata\locallow 3",
                r"users\*\appdata\roaming 0",
            ]
        );
        assert_eq!(
            paths(&app_data_rules(&m, "d:")),
            [
                r"home\ayse\appdata\local 3",
                r"home\ayse\appdata\locallow 3",
                r"home\ayse\appdata\roaming 0",
            ]
        );
        assert!(app_data_rules(&m, "E:").is_empty());
    }

    /// A volume in MFT order (children before parents).
    fn volume() -> Vec<UsnRecord> {
        vec![
            rec(27, 26, "rapor.txt", false),
            rec(17, 16, "x.tmp", false),
            rec(16, 13, "Temp", true),
            rec(22, 21, "Discord.lnk", false),
            rec(25, 14, "oyun.sav", false),
            rec(23, 18, "Credentials", true),
            rec(24, 19, "Themes", true),
            rec(21, 20, "Programs", true),
            rec(20, 19, "Start Menu", true),
            rec(19, 18, "Windows", true),
            rec(18, 14, "Microsoft", true),
            rec(13, 12, "Local", true),
            rec(14, 12, "Roaming", true),
            rec(15, 12, "LocalLow", true),
            rec(28, 15, "Unity", true),
            rec(12, 11, "APPDATA", true),
            rec(26, 11, "Documents", true),
            rec(11, 10, "deniz", true),
            rec(10, ROOT, "Users", true),
            // an AppData folder that is not a profile's
            rec(30, ROOT, "Proj", true),
            rec(31, 30, "AppData", true),
            rec(32, 31, "Local", true),
            rec(33, 32, "keep.txt", false),
        ]
    }

    #[test]
    fn a_scan_leaves_out_app_data_but_the_start_menu() {
        let mut cut = Cut::new(app_data_rules(&machine(), "C:"));
        let mut recs = volume();
        cut.filter(ROOT, &mut recs);
        let mut kept: Vec<u64> = recs.iter().map(|r| r.frn).collect();
        kept.sort_unstable();
        assert_eq!(
            kept,
            [
                10, 11, 12, 13, 14, 15, 18, 19, 20, 21, 22, 26, 27, 30, 31, 32, 33
            ]
        );
        assert_eq!(
            cut.parts(),
            (
                vec![(13, 3), (14, 0), (15, 3), (18, 1), (19, 2)],
                vec![16, 23, 24, 28]
            )
        );
        // journal records go the same way
        assert_eq!(cut.place(13, "Cache", true), Place::Hidden);
        assert_eq!(cut.place(16, "a.tmp", false), Place::Hidden);
        assert_eq!(
            cut.place(14, "Microsoft", false),
            Place::Hidden,
            "a file named like the path"
        );
        assert_eq!(cut.place(14, "MICROSOFT", true), Place::Cut(1));
        assert_eq!(cut.place(19, "Start Menu", true), Place::Shown);
        assert_eq!(cut.place(20, "x.lnk", false), Place::Shown);
        assert_eq!(cut.place(26, "b.txt", false), Place::Shown);
        assert!(cut.hides(16) && !cut.hides(13));
    }

    #[test]
    fn no_rules_or_no_match_keeps_everything() {
        let mut recs = volume();
        let mut cut = Cut::default();
        cut.filter(ROOT, &mut recs);
        assert_eq!(recs, volume());
        let mut cut = Cut::new(app_data_rules(&machine(), "D:"));
        cut.filter(ROOT, &mut recs);
        assert_eq!(recs, volume());
        assert_eq!(cut.parts(), (vec![], vec![]));
    }

    #[test]
    fn new_folders_are_named_by_the_rules() {
        let cut = Cut::new(app_data_rules(&machine(), "C:"));
        let p = |s: &str| names(s);
        assert_eq!(cut.rule_depth(&p(r"users\bob\appdata\roaming")), Some(0));
        assert_eq!(cut.rule_depth(&p(r"users\bob\appdata\local")), Some(3));
        assert_eq!(cut.rule_depth(&p(r"users\bob\appdata")), None);
        assert_eq!(cut.rule_depth(&p(r"proj\appdata\local")), None);
        assert!(cut.may_name("LocalLow") && !cut.may_name("Documents"));
        assert!(cut.rule_prefix(&p(r"users\bob")) && cut.rule_prefix(&p("users")));
        assert!(!cut.rule_prefix(&p(r"users\bob\appdata\local")) && !cut.rule_prefix(&p("proj")));
        // the same machine in another order gives the same rules
        let mut m = machine();
        m.profiles.reverse();
        m.profiles_dir_aliases.insert(0, r"C:\Users".into());
        assert_eq!(app_data_rules(&m, "C:"), app_data_rules(&machine(), "C:"));
    }

    #[test]
    fn hidden_folders_are_forgotten_and_come_out() {
        let mut cut = Cut::from_parts(Vec::new(), &[(13, 3)], &[16, 17]);
        assert!(cut.forget(16) && !cut.forget(16) && !cut.forget(13));
        assert_eq!(cut.parts(), (vec![], vec![17]));
        assert!(cut.unhide(17) && !cut.unhide(17));
        assert_eq!(cut.take_surfaced(), [17]);
        assert!(cut.take_surfaced().is_empty() && !cut.take_stale());
        cut.mark_stale();
        assert!(cut.take_stale() && !cut.take_stale());
    }
}
