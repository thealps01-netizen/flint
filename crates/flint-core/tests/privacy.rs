//! Per-user privacy rule: entries under another user's
//! profile, recycle bin or a system-only folder never reach a viewer, in
//! hits or in counts, whichever tier would have matched them, including
//! folders created after the rules were set.

use flint_core::{
    Boosts, EntryId, EntryInput, Index, OWNER_NOBODY, OWNER_NONE, Owner, OwnerRule, Restriction,
    SearchOptions, parse, search,
};
use std::sync::atomic::AtomicBool;

const NOW: u32 = 1_800_000_000;
const ALICE: Owner = 1;
const BOB: Owner = 2;
const BOB_SID: &str = "S-1-5-21-B";

struct Fx {
    ix: Index,
    users: EntryId,
    bob_dir: EntryId,
    shared_file: EntryId,
}

fn dir(ix: &mut Index, name: &str, parent: Option<EntryId>) -> EntryId {
    ix.insert(EntryInput {
        name,
        parent,
        is_dir: true,
        last_change: 0,
    })
}
fn file(ix: &mut Index, name: &str, parent: EntryId) -> EntryId {
    ix.insert(EntryInput {
        name,
        parent: Some(parent),
        is_dir: false,
        last_change: 0,
    })
}

fn rule(path: &[&str], children: &[(&str, Owner)], default: Owner) -> OwnerRule {
    OwnerRule {
        path: path.iter().map(|s| s.to_string()).collect(),
        children: children.iter().map(|(n, o)| (n.to_string(), *o)).collect(),
        default,
    }
}

fn rules() -> Vec<OwnerRule> {
    vec![
        rule(
            &["C:", "Users"],
            &[("alice", ALICE), ("bob", BOB), ("Public", OWNER_NONE)],
            OWNER_NOBODY,
        ),
        rule(&["*:", "$Recycle.Bin"], &[(BOB_SID, BOB)], OWNER_NOBODY),
        rule(
            &["*:"],
            &[("System Volume Information", OWNER_NOBODY)],
            OWNER_NONE,
        ),
    ]
}

impl Fx {
    fn new() -> Self {
        let mut ix = Index::new();
        let c = dir(&mut ix, "C:", None);
        let users = dir(&mut ix, "Users", Some(c));
        let alice = dir(&mut ix, "alice", Some(users));
        let bob = dir(&mut ix, "bob", Some(users));
        let public = dir(&mut ix, "Public", Some(users));
        let finans = dir(&mut ix, "Finans", Some(bob));
        let rb = dir(&mut ix, "$Recycle.Bin", Some(c));
        let rb_bob = dir(&mut ix, BOB_SID, Some(rb));
        let svi = dir(&mut ix, "System Volume Information", Some(c));
        let shared = dir(&mut ix, "Shared", Some(c));
        file(&mut ix, "rapor.docx", alice);
        file(&mut ix, "rapor.docx", bob);
        file(&mut ix, "butce.xlsx", finans);
        file(&mut ix, "rapor.docx", rb_bob);
        file(&mut ix, "rapor.dat", svi);
        file(&mut ix, "ortak-rapor.txt", public);
        let shared_file = file(&mut ix, "rapor.pdf", shared);
        ix.set_owner_rules(&rules());
        Fx {
            ix,
            users,
            bob_dir: bob,
            shared_file,
        }
    }

    fn paths(&self, q: &str, viewer: Owner) -> (Vec<String>, u32) {
        let cancel = AtomicBool::new(false);
        let b = Boosts::new();
        let opt = SearchOptions {
            limit: 50,
            now: NOW,
            boosts: &b,
            cancel: &cancel,
            viewer,
        };
        let r = search(&self.ix, &parse(q), &opt);
        let mut p: Vec<String> = r.hits.iter().map(|h| self.ix.path(h.id)).collect();
        p.sort();
        (p, r.total_matches)
    }
}

#[test]
fn other_owner_is_invisible_everywhere() {
    let fx = Fx::new();
    let (p, total) = fx.paths("rapor", ALICE);
    assert_eq!(
        p,
        vec![
            r"C:\Shared\rapor.pdf",
            r"C:\Users\Public\ortak-rapor.txt",
            r"C:\Users\alice\rapor.docx"
        ]
    );
    assert_eq!(total, 3, "counts leak nothing");
    let (p, _) = fx.paths("rapro", ALICE); // typo tier
    assert!(
        !p.iter().any(|x| x.contains("bob") || x.contains(BOB_SID)),
        "{p:?}"
    );
    let (p, total) = fx.paths("finans butce", ALICE); // folder-name term
    assert!(p.is_empty(), "{p:?}");
    assert_eq!(total, 0);
    let (p, _) = fx.paths("bob", ALICE); // the profile folder itself
    assert!(p.is_empty(), "{p:?}");
    // bob sees his own files, through every tier
    let (p, _) = fx.paths("finans butce", BOB);
    assert_eq!(p, vec![r"C:\Users\bob\Finans\butce.xlsx"]);
    let (p, _) = fx.paths("rapor", BOB);
    assert_eq!(p.len(), 4, "{p:?}"); // profile, recycle bin, public, shared
}

#[test]
fn nobody_is_hidden_from_all() {
    let fx = Fx::new();
    for v in [ALICE, BOB, OWNER_NOBODY] {
        let (p, _) = fx.paths("rapor", v);
        assert!(
            !p.iter().any(|x| x.contains("System Volume Information")),
            "{v}: {p:?}"
        );
    }
    // an unknown viewer (fail closed) sees only public entries
    let (p, _) = fx.paths("rapor", OWNER_NOBODY);
    assert_eq!(
        p,
        vec![r"C:\Shared\rapor.pdf", r"C:\Users\Public\ortak-rapor.txt"]
    );
}

#[test]
fn no_rules_means_everything_public() {
    let mut ix = Index::new();
    let c = dir(&mut ix, "C:", None);
    let u = dir(&mut ix, "Users", Some(c));
    let b = dir(&mut ix, "bob", Some(u));
    let f = file(&mut ix, "rapor.docx", b);
    assert_eq!(ix.owner(f), OWNER_NONE);
}

#[test]
fn moved_into_other_profile_becomes_hidden() {
    let mut fx = Fx::new();
    let bob = fx.bob_dir;
    let f = fx.shared_file;
    let (before, _) = fx.paths("rapor", ALICE); // builds the cached tree first
    assert_eq!(before.len(), 3);
    fx.ix.rename(f, "rapor.pdf", Some(bob));
    assert_eq!(fx.ix.owner(f), BOB);
    let (p, _) = fx.paths("rapor", ALICE);
    assert_eq!(
        p,
        vec![
            r"C:\Users\Public\ortak-rapor.txt",
            r"C:\Users\alice\rapor.docx"
        ]
    );
}

#[test]
fn renamed_profile_folder_stays_hidden() {
    let mut fx = Fx::new();
    let bob = fx.bob_dir;
    fx.ix.rename(bob, "robert", None);
    let (p, _) = fx.paths("finans butce", ALICE);
    assert!(p.is_empty(), "{p:?}");
    let (p, _) = fx.paths("robert", ALICE);
    assert!(
        p.is_empty(),
        "an unknown folder in Users is nobody's: {p:?}"
    );
}

/// Rules bound to folder ids left folders created
/// later public. Rules match names, so new folders are covered at once.
#[test]
fn folders_created_after_the_rules_are_covered() {
    let mut fx = Fx::new();
    let _ = fx.paths("rapor", ALICE); // tree built, then the disk changes
    // a new account's first logon
    let carol = dir(&mut fx.ix, "carol", Some(fx.users));
    file(&mut fx.ix, "gizli-carol.txt", carol);
    // bob's profile folder deleted and created again (new entry id)
    fx.ix.remove(fx.bob_dir);
    let bob2 = dir(&mut fx.ix, "bob", Some(fx.users));
    file(&mut fx.ix, "gizli-bob.txt", bob2);
    // a drive added later, with bob's recycle bin and a restore folder
    let d = dir(&mut fx.ix, "D:", None);
    let rb = dir(&mut fx.ix, "$Recycle.Bin", Some(d));
    let rbb = dir(&mut fx.ix, BOB_SID, Some(rb));
    let deep = dir(&mut fx.ix, "$RAB12CD", Some(rbb));
    file(&mut fx.ix, "gizli-silinen.xlsx", deep);
    let other_rb = dir(&mut fx.ix, "S-1-5-21-GONE", Some(rb));
    file(&mut fx.ix, "gizli-eski-hesap.txt", other_rb);
    let svi = dir(&mut fx.ix, "System Volume Information", Some(d));
    file(&mut fx.ix, "gizli.dat", svi);
    let (p, total) = fx.paths("gizli", ALICE);
    assert!(p.is_empty(), "{p:?}");
    assert_eq!(total, 0);
    let (p, _) = fx.paths("gizli", BOB);
    assert_eq!(
        p,
        vec![
            r"C:\Users\bob\gizli-bob.txt",
            r"D:\$Recycle.Bin\S-1-5-21-B\$RAB12CD\gizli-silinen.xlsx"
        ]
    );
}

#[test]
fn rule_names_match_case_but_not_accents() {
    let mut fx = Fx::new();
    let bob_upper = dir(&mut fx.ix, "BÖB", Some(fx.users)); // another account, not bob
    file(&mut fx.ix, "gizli.txt", bob_upper);
    assert_eq!(fx.ix.owner(bob_upper), OWNER_NOBODY);
    let mut ix = Index::new();
    let c = dir(&mut ix, "c:", None);
    let u = dir(&mut ix, "USERS", Some(c));
    let b = dir(&mut ix, "Bob", Some(u));
    ix.set_owner_rules(&rules());
    assert_eq!(ix.owner(b), BOB, "Windows names are case-insensitive");
}

#[test]
fn exact_drive_rules_keep_the_any_drive_rules() {
    // C:\Users is an exact-drive rule; $Recycle.Bin and SVI are any-drive
    // rules: on C: both must apply
    let fx = Fx::new();
    let (p, _) = fx.paths("rapor", ALICE);
    assert!(
        !p.iter()
            .any(|x| x.contains("$Recycle.Bin") || x.contains("System Volume")),
        "{p:?}"
    );
}

#[test]
fn an_owned_subtree_never_becomes_more_visible() {
    // a folder named like a rule path inside bob's profile stays bob's
    let mut fx = Fx::new();
    let inner_users = dir(&mut fx.ix, "Users", Some(fx.bob_dir));
    let inner_pub = dir(&mut fx.ix, "Public", Some(inner_users));
    assert_eq!(fx.ix.owner(inner_pub), BOB);
}

/// A stale file entry that gets children (the file was
/// replaced by a folder) must not hand them owner "public", and must not
/// break search or the folder tree.
#[test]
fn children_of_a_file_entry_keep_the_owner_rules() {
    let mut fx = Fx::new();
    let x = file(&mut fx.ix, "x", fx.bob_dir);
    let _ = fx.paths("rapor", ALICE); // tree built while x is a file
    let inner = file(&mut fx.ix, "gizli-ic.txt", x);
    let sub = dir(&mut fx.ix, "alt", Some(x));
    file(&mut fx.ix, "gizli-alt.txt", sub);
    assert!(fx.ix.is_dir(x), "an entry with children is a folder");
    assert_eq!(fx.ix.owner(inner), BOB);
    let (p, total) = fx.paths("gizli", ALICE);
    assert!(p.is_empty(), "{p:?}");
    assert_eq!(total, 0);
    assert_eq!(fx.paths("gizli", BOB).0.len(), 2);
    let before = fx.ix.visible_len(ALICE);
    let f2 = file(&mut fx.ix, "y", fx.bob_dir);
    fx.ix.rename(sub, "alt", Some(f2)); // moved under another file entry
    assert!(fx.ix.is_dir(f2));
    assert_eq!(fx.ix.visible_len(ALICE), before);
}

/// Per-drive counts in a status reply are counted per
/// viewer too, or subtracting them from the total reveals other users'
/// files.
#[test]
fn per_drive_counts_follow_the_viewer() {
    let mut fx = Fx::new();
    let d = dir(&mut fx.ix, "D:", None);
    file(&mut fx.ix, "d.txt", d);
    let by = fx.ix.visible_len_by_drive(ALICE);
    let total: usize = by.iter().map(|&(_, n)| n).sum();
    assert_eq!(total, fx.ix.visible_len(ALICE));
    assert_eq!(
        by.iter().find(|&&(drv, _)| drv == b'd').map(|x| x.1),
        Some(2)
    );
    assert!(
        total < fx.ix.live_len(),
        "other users' entries are not counted"
    );
}

/// A folder an ordinary user may not list
/// (`C:\\Windows\\Prefetch`) is shown to no one, with everything in it, and
/// counted for no one. A profile owner keeps their own folder; a folder
/// whose name claims an owner it does not have (a recycle-bin folder made
/// by someone else) is shown to no one.
#[test]
fn restricted_folders_are_nobodys() {
    let mut fx = Fx::new();
    let c = fx.ix.parent(fx.users).unwrap();
    let pf = dir(&mut fx.ix, "Prefetch", Some(c));
    let sub = dir(&mut fx.ix, "alt", Some(pf));
    file(&mut fx.ix, "gizliprogram.pf", pf);
    file(&mut fx.ix, "gizlialt.pf", sub);
    let before = fx.ix.visible_len(ALICE);
    assert_eq!(fx.paths("gizli", ALICE).0.len(), 2);
    fx.ix.set_restriction(pf, Some(Restriction::NoList));
    assert_eq!(fx.ix.restriction(pf), Some(Restriction::NoList));
    let (p, total) = fx.paths("gizli", ALICE);
    assert!(p.is_empty(), "{p:?}");
    assert_eq!(total, 0);
    assert!(
        fx.paths("prefetch", ALICE).0.is_empty(),
        "the folder itself"
    );
    assert_eq!(fx.ix.visible_len(ALICE), before - 4);
    assert!(
        fx.paths("gizli", OWNER_NONE).0.is_empty(),
        "nor an unknown viewer"
    );
    // an owner rule wins over NoList: bob's profile is not listable by
    // ordinary users, and bob still sees it
    fx.ix.set_restriction(fx.bob_dir, Some(Restriction::NoList));
    assert_eq!(fx.paths("finans butce", BOB).0.len(), 1);
    // Untrusted beats the rule: bob's recycle folder made by someone else
    let (p, _) = fx.paths("rapor", BOB);
    let rb_hit = p.iter().any(|x| x.contains(BOB_SID));
    assert!(rb_hit, "{p:?}");
    let rb_bob = (0..fx.ix.raw_len() as u32)
        .map(EntryId)
        .find(|&id| fx.ix.name(id) == BOB_SID)
        .unwrap();
    fx.ix.set_restriction(rb_bob, Some(Restriction::Untrusted));
    let (p, _) = fx.paths("rapor", BOB);
    assert!(!p.iter().any(|x| x.contains(BOB_SID)), "{p:?}");
    // lifting it shows it again; a rename keeps the mark (the ACL moves
    // with the folder)
    fx.ix.set_restriction(pf, None);
    assert_eq!(fx.paths("gizli", ALICE).0.len(), 2);
    fx.ix.set_restriction(pf, Some(Restriction::NoList));
    fx.ix.rename(pf, "Prefetch2", None);
    assert!(fx.paths("gizli", ALICE).0.is_empty());
}

/// The indexer asks one entry's owner while it applies journal records
/// (a folder rename drops the cached tree, and rebuilding it per record is
/// too slow): `owner_uncached` must give the same answer as the tree.
#[test]
fn owner_uncached_matches_the_tree() {
    let mut fx = Fx::new();
    let c = fx.ix.parent(fx.users).unwrap();
    let pf = dir(&mut fx.ix, "Prefetch", Some(c));
    file(&mut fx.ix, "x.pf", pf);
    fx.ix.set_restriction(pf, Some(Restriction::NoList));
    let sub = dir(&mut fx.ix, "Masaüstü", Some(fx.bob_dir));
    fx.ix.rename(sub, "Masaüstü", Some(fx.users)); // now an unknown profile folder
    let check = |ix: &Index| {
        for id in (0..ix.raw_len() as u32)
            .map(EntryId)
            .filter(|&i| ix.is_live(i))
        {
            assert_eq!(ix.owner_uncached(id), ix.owner(id), "{}", ix.path(id));
        }
    };
    check(&fx.ix);
    let mut plain = Index::new();
    let r = dir(&mut plain, "C:", None);
    file(&mut plain, "a.txt", r);
    check(&plain);
}

/// The cached folder tree is updated in place on folder renames, moves,
/// restriction and CACHEDIR.TAG changes (a rebuild per change cost every
/// query ~100 ms on a busy 3 M volume). After every random step it must
/// equal a tree built from scratch.
#[test]
fn the_tree_kept_up_to_date_equals_a_rebuilt_one() {
    let mut fx = Fx::new();
    let root = fx.ix.parent(fx.users).unwrap();
    let mut dirs = vec![root, fx.users, fx.bob_dir];
    let names = [
        "alice",
        "bob",
        "Belgeler",
        "node_modules",
        "cache",
        "Prefetch",
        "x",
        "Users",
    ];
    let mut seed: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut next = |n: usize| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        (seed % n as u64) as usize
    };
    for step in 0..400 {
        fx.ix.dir_tree(); // a cached tree to keep up to date
        let d = dirs[next(dirs.len())];
        match next(6) {
            0 => {
                let n = names[next(names.len())];
                dirs.push(dir(&mut fx.ix, n, Some(d)));
            }
            1 => {
                file(&mut fx.ix, &format!("f{step}.txt"), d);
            }
            2 if d != root => {
                // rename, sometimes into another folder (not below itself)
                let target = dirs[next(dirs.len())];
                let below = target == d || fx.ix.ancestors(target).any(|a| a == d);
                let n = names[next(names.len())];
                let parent = if below { fx.ix.parent(d) } else { Some(target) };
                fx.ix.rename(d, n, parent);
            }
            3 => {
                let r = [
                    None,
                    Some(Restriction::NoList),
                    Some(Restriction::Untrusted),
                ][next(3)];
                fx.ix.set_restriction(d, r);
            }
            4 => {
                file(&mut fx.ix, "CACHEDIR.TAG", d);
            }
            _ => {
                let tags: Vec<EntryId> = (0..fx.ix.raw_len() as u32)
                    .map(EntryId)
                    .filter(|&i| fx.ix.is_live(i) && fx.ix.name(i) == "CACHEDIR.TAG")
                    .collect();
                if let Some(&t) = tags.get(next(tags.len().max(1))) {
                    fx.ix.remove(t);
                }
            }
        }
        if let Err(e) = fx.ix.check_tree() {
            panic!("step {step}: {e}");
        }
    }
}

/// `descendant_dirs` lists every folder below one, at any depth, and no
/// other (journal checks of a moved folder rely on it).
#[test]
fn descendant_dirs_lists_the_whole_subtree() {
    let mut fx = Fx::new();
    let a = dir(&mut fx.ix, "a", Some(fx.users));
    let b = dir(&mut fx.ix, "b", Some(a));
    let c = dir(&mut fx.ix, "c", Some(b));
    file(&mut fx.ix, "f.txt", c);
    let other = dir(&mut fx.ix, "other", Some(fx.users));
    let mut got = fx.ix.descendant_dirs(a);
    got.sort_by_key(|e| e.0);
    assert_eq!(got, vec![b, c]);
    // after a move the cached tree still answers right
    fx.ix.rename(c, "c", Some(other));
    assert_eq!(fx.ix.descendant_dirs(a), vec![b]);
    assert_eq!(fx.ix.descendant_dirs(other), vec![c]);
}
