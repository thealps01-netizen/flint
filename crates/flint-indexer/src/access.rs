//! Folder restrictions from security descriptors: which folders of an
//! NTFS volume an ordinary user may not list, and which recycle-bin folders do not belong to the account they
//! are named after. Pure: the OS check is passed in, so the rules are
//! tested without administrator rights.

use crate::usn::{
    FILE_ATTRIBUTE_DIRECTORY, USN_REASON_FILE_CREATE, USN_REASON_RENAME_NEW_NAME,
    USN_REASON_SECURITY_CHANGE, UsnRecord,
};
use crate::volindex::VolumeIndex;
use flint_core::{EntryId, Index, OWNER_NOBODY, OWNER_NONE, Restriction};
use std::io;

/// What the OS check learned about one folder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verdict {
    /// An ordinary user may list it.
    pub listable: bool,
    /// Its owner's SID string.
    pub owner: String,
}

/// `$Recycle.Bin\<SID>` directly below a drive root: the SID it claims.
fn recycle_sid(ix: &Index, id: EntryId) -> Option<&str> {
    let p = ix.parent(id)?;
    (ix.parent(p).and_then(|r| ix.parent(r)).is_none()
        && ix.parent(p).is_some()
        && ix.name(p).eq_ignore_ascii_case("$Recycle.Bin")
        && ix.name(id).to_ascii_uppercase().starts_with("S-1-"))
    .then(|| ix.name(id))
}

/// The restriction folder `id` gets from its check (an error fails closed).
fn restriction_of(ix: &Index, id: EntryId, v: &io::Result<Verdict>) -> Option<Restriction> {
    match (recycle_sid(ix, id), v) {
        (Some(sid), Ok(v)) if v.owner.eq_ignore_ascii_case(sid) => None,
        (Some(_), _) => Some(Restriction::Untrusted),
        (None, Ok(v)) if v.listable => None,
        (None, _) => Some(Restriction::NoList),
    }
}

/// Counts of a first pass, for the log and the baseline.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct PassStats {
    pub checked: usize,
    pub restricted: usize,
    pub failed: usize,
}

/// First pass over a freshly built volume (owner rules already set):
/// checks every folder no rule gave an owner, and every recycle-bin
/// folder, and sets their restrictions. Folders inside a profile are not
/// checked; a profile owner already limits them to that account.
pub fn first_pass(
    ix: &mut Index,
    v: &VolumeIndex,
    mut check: impl FnMut(u64) -> io::Result<Verdict>,
) -> PassStats {
    first_pass_batch(ix, v, |frns| frns.iter().map(|&f| check(f)).collect())
}

/// [`first_pass`] with all checks handed over at once (`check_all`
/// returns one verdict per FRN, in order), so they can run in parallel or
/// share verdicts per security id.
pub fn first_pass_batch(
    ix: &mut Index,
    v: &VolumeIndex,
    check_all: impl FnOnce(&[u64]) -> Vec<io::Result<Verdict>>,
) -> PassStats {
    let mut stats = PassStats::default();
    let todo: Vec<(u64, EntryId)> = v
        .entries(ix)
        .map(|(frn, _, id)| (frn, id))
        .filter(|&(frn, id)| {
            frn != v.root_frn()
                && frn != crate::volindex::ORPHAN_FRN
                && ix.is_dir(id)
                && (ix.owner(id) == OWNER_NONE || recycle_sid(ix, id).is_some())
        })
        .collect();
    let frns: Vec<u64> = todo.iter().map(|&(f, _)| f).collect();
    let verdicts = check_all(&frns);
    assert_eq!(verdicts.len(), todo.len(), "one verdict per folder");
    let mut marks = Vec::new();
    for ((_, id), verdict) in todo.into_iter().zip(verdicts) {
        stats.checked += 1;
        stats.failed += usize::from(verdict.is_err());
        if let Some(r) = restriction_of(ix, id, &verdict) {
            stats.restricted += 1;
            marks.push((id, r));
        }
    }
    for (id, r) in marks {
        ix.set_restriction(id, Some(r));
    }
    stats
}

/// Folders a batch of journal records needs checked, read before the
/// batch is applied: new folders, folders whose security changed, and a
/// moved folder with everything below it (its ACL moves with it, and
/// folders inside a profile or a hidden folder were never checked).
/// Folders that land inside a profile are not checked: the profile owner
/// already limits them to that account, and a busy profile creates
/// folders all the time.
pub fn checks_for(ix: &Index, v: &VolumeIndex, recs: &[UsnRecord]) -> Vec<u64> {
    let mut out = Vec::new();
    for r in recs {
        if r.attrs & FILE_ATTRIBUTE_DIRECTORY == 0 {
            continue;
        }
        let in_profile = v.id(r.parent).is_some_and(|p| {
            let o = ix.owner_uncached(p);
            o != OWNER_NONE && o != OWNER_NOBODY
        });
        if in_profile {
            continue;
        }
        // a folder the index does not know yet is inserted by whatever
        // record names it (an attribute or stream change, not only a
        // create): it is checked like a new one
        if r.reason & (USN_REASON_FILE_CREATE | USN_REASON_SECURITY_CHANGE) != 0
            || v.id(r.frn).is_none()
        {
            out.push(r.frn);
        }
        if r.reason & USN_REASON_RENAME_NEW_NAME != 0 {
            out.push(r.frn);
            if let Some(moved) = v.id(r.frn).filter(|&m| ix.child_count(m) > 0) {
                out.extend(
                    ix.descendant_dirs(moved)
                        .into_iter()
                        .filter_map(|d| v.frn(d)),
                );
            }
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// Sets the restrictions from `verdicts` (after the batch was applied, so
/// names and parents are current).
pub fn apply_verdicts(ix: &mut Index, v: &VolumeIndex, verdicts: &[(u64, io::Result<Verdict>)]) {
    for (frn, verdict) in verdicts {
        if let Some(id) = v.id(*frn).filter(|&id| ix.is_live(id) && ix.is_dir(id)) {
            let r = restriction_of(ix, id, verdict);
            ix.set_restriction(id, r);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::privacy::{Machine, OwnerRegistry, Profile, apply};
    use crate::usn::USN_REASON_CLOSE;
    use flint_core::{Boosts, SearchOptions, parse, search};
    use std::collections::HashMap;

    const ROOT: u64 = 5;
    const BOB: &str = "S-1-5-21-1-2-3-1002";
    const ALICE: &str = "S-1-5-21-1-2-3-1001";

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

    /// C:\Users\{alice,bob}, C:\Windows\Prefetch, C:\$Recycle.Bin\{bob,
    /// planted}, C:\Program Files\app.
    fn volume() -> Vec<UsnRecord> {
        vec![
            rec(10, ROOT, "Users", true, 0),
            rec(11, 10, "alice", true, 0),
            rec(12, 10, "bob", true, 0),
            rec(13, 12, "Gizli", true, 0),
            rec(14, 13, "bob-rapor.txt", false, 0),
            rec(15, 13, "Alt", true, 0),
            rec(20, ROOT, "Windows", true, 0),
            rec(21, 20, "Prefetch", true, 0),
            rec(22, 21, "PROGRAM-rapor.pf", false, 0),
            rec(23, 21, "Alt", true, 0),
            rec(24, 23, "alt-rapor.pf", false, 0),
            rec(30, ROOT, "$Recycle.Bin", true, 0),
            rec(31, 30, BOB, true, 0),
            rec(32, 31, "silinen-rapor.txt", false, 0),
            rec(33, 30, ALICE, true, 0),
            rec(34, 33, "sahte-rapor.txt", false, 0),
            rec(40, ROOT, "Program Files", true, 0),
            rec(41, 40, "app-rapor.txt", false, 0),
        ]
    }

    fn machine() -> Machine {
        Machine {
            profiles: vec![
                Profile {
                    sid: ALICE.into(),
                    path: r"C:\Users\alice".into(),
                },
                Profile {
                    sid: BOB.into(),
                    path: r"C:\Users\bob".into(),
                },
            ],
            profiles_dir: r"C:\Users".into(),
            profiles_dir_aliases: vec![],
            public_dir: None,
            public_dir_aliases: vec![],
            windows_dir: r"C:\Windows".into(),
        }
    }

    /// The disk's verdicts: Prefetch and the planted recycle folder.
    fn disk() -> HashMap<u64, Verdict> {
        let v = |listable, owner: &str| Verdict {
            listable,
            owner: owner.into(),
        };
        HashMap::from([
            (10, v(true, "S-1-5-18")),
            (11, v(false, ALICE)),
            (12, v(false, BOB)),
            (13, v(false, BOB)),
            (20, v(true, "S-1-5-80-1")),
            (21, v(false, "S-1-5-18")),
            (23, v(false, "S-1-5-18")),
            (30, v(true, "S-1-5-18")),
            (31, v(false, BOB)),
            // made by alice and named after her: trusted
            (33, v(false, ALICE)),
            (40, v(true, "S-1-5-80-1")),
        ])
    }

    fn hits(ix: &Index, reg: &OwnerRegistry, sid: &str) -> Vec<String> {
        let b = Boosts::new();
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let opts = SearchOptions {
            limit: 50,
            now: 0,
            boosts: &b,
            cancel: &cancel,
            viewer: reg.viewer(sid),
        };
        let mut p: Vec<String> = search(ix, &parse("rapor"), &opts)
            .hits
            .iter()
            .map(|h| ix.path(h.id))
            .collect();
        p.sort();
        p
    }

    fn setup(
        d: &HashMap<u64, Verdict>,
    ) -> (Index, VolumeIndex, OwnerRegistry, Vec<u64>, PassStats) {
        let mut ix = Index::new();
        let v = VolumeIndex::build(&mut ix, "C:", ROOT, volume());
        let mut reg = OwnerRegistry::new();
        apply(&mut ix, &machine(), &mut reg);
        let mut asked = Vec::new();
        let stats = first_pass(&mut ix, &v, |frn| {
            asked.push(frn);
            d.get(&frn).cloned().ok_or_else(|| io::Error::other("gone"))
        });
        asked.sort_unstable();
        (ix, v, reg, asked, stats)
    }

    #[test]
    fn first_pass_hides_unlistable_folders_and_untrusted_recycle_folders() {
        let (ix, _, reg, asked, stats) = setup(&disk());
        // profiles are not asked about (their owner rule decides); the
        // recycle folders are, whatever their rule
        assert_eq!(
            asked,
            vec![10, 20, 21, 23, 30, 31, 33, 40],
            "asked {asked:?}"
        );
        assert_eq!(
            stats,
            PassStats {
                checked: 8,
                restricted: 2,
                failed: 0
            }
        );
        assert_eq!(
            hits(&ix, &reg, BOB),
            vec![
                r"C:\$Recycle.Bin\S-1-5-21-1-2-3-1002\silinen-rapor.txt",
                r"C:\Program Files\app-rapor.txt",
                r"C:\Users\bob\Gizli\bob-rapor.txt",
            ]
        );
        assert_eq!(
            hits(&ix, &reg, ALICE),
            vec![
                r"C:\$Recycle.Bin\S-1-5-21-1-2-3-1001\sahte-rapor.txt",
                r"C:\Program Files\app-rapor.txt",
            ]
        );
    }

    /// L3: a recycle folder named after bob but owned by alice (she made it
    /// first) shows to no one; a failed check fails closed.
    #[test]
    fn a_recycle_folder_owned_by_someone_else_is_nobodys() {
        let mut d = disk();
        d.insert(
            31,
            Verdict {
                listable: false,
                owner: ALICE.into(),
            },
        );
        d.remove(&40);
        let (ix, _, reg, _, stats) = setup(&d);
        assert_eq!(stats.failed, 1);
        assert_eq!(
            hits(&ix, &reg, BOB),
            vec![r"C:\Users\bob\Gizli\bob-rapor.txt"]
        );
        let a = hits(&ix, &reg, ALICE);
        assert_eq!(
            a,
            vec![r"C:\$Recycle.Bin\S-1-5-21-1-2-3-1001\sahte-rapor.txt"]
        );
    }

    /// Journal: a new folder, a security change and a folder moved out of a
    /// profile are checked before the batch is shown.
    #[test]
    fn journal_changes_are_checked() {
        let (mut ix, mut v, reg, _, _) = setup(&disk());
        let new_dir = rec(
            50,
            40,
            "Kilitli",
            true,
            USN_REASON_FILE_CREATE | USN_REASON_CLOSE,
        );
        let new_file = rec(51, 50, "kilitli-rapor.txt", false, USN_REASON_FILE_CREATE);
        let sec = rec(40, ROOT, "Program Files", true, USN_REASON_SECURITY_CHANGE);
        let moved = rec(13, ROOT, "Gizli", true, USN_REASON_RENAME_NEW_NAME);
        let batch = vec![new_dir, new_file, sec, moved];
        let todo = checks_for(&ix, &v, &batch);
        // the moved folder brings its subfolder (15) along
        assert_eq!(todo, vec![13, 15, 40, 50]);
        let verdicts: Vec<(u64, io::Result<Verdict>)> = todo
            .iter()
            .map(|&f| {
                let listable = f == 40;
                (
                    f,
                    Ok(Verdict {
                        listable,
                        owner: BOB.into(),
                    }),
                )
            })
            .collect();
        for r in &batch {
            v.apply(&mut ix, r);
        }
        apply_verdicts(&mut ix, &v, &verdicts);
        // bob's folder moved to C:\Gizli, unlistable: nobody's now (fail
        // closed: no rule names its owner)
        assert_eq!(
            hits(&ix, &reg, BOB),
            vec![
                r"C:\$Recycle.Bin\S-1-5-21-1-2-3-1002\silinen-rapor.txt",
                r"C:\Program Files\app-rapor.txt",
            ]
        );
        // the security of Program Files changed to unlistable: hidden
        let sec2 = rec(40, ROOT, "Program Files", true, USN_REASON_SECURITY_CHANGE);
        assert_eq!(checks_for(&ix, &v, std::slice::from_ref(&sec2)), vec![40]);
        apply_verdicts(
            &mut ix,
            &v,
            &[(
                40,
                Ok(Verdict {
                    listable: false,
                    owner: BOB.into(),
                }),
            )],
        );
        assert!(
            !hits(&ix, &reg, BOB)
                .iter()
                .any(|p| p.contains("Program Files"))
        );
        // files never need a check
        let f = rec(52, 40, "x.txt", false, USN_REASON_SECURITY_CHANGE);
        assert!(checks_for(&ix, &v, &[f]).is_empty());
        // nor folders made inside a profile; a recycle folder still is
        let in_profile = rec(53, 12, "Yeni", true, USN_REASON_FILE_CREATE);
        let in_recycle = rec(54, 30, "S-1-5-21-9", true, USN_REASON_FILE_CREATE);
        assert_eq!(checks_for(&ix, &v, &[in_profile, in_recycle]), vec![54]);
        // a folder the index has never seen, named by a record that is not
        // a create (a stream or attribute change), is checked too
        let unseen = rec(55, 40, "Gorunmeyen", true, 0x4 /* DATA_EXTEND */);
        let known = rec(40, ROOT, "Program Files", true, 0x4);
        assert_eq!(checks_for(&ix, &v, &[unseen, known]), vec![55]);
    }
}
