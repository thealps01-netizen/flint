//! Index images (warm start): what goes in comes back, and a damaged
//! image is refused.

use flint_core::{EntryId, EntryInput, ImageError, Index, Restriction};

fn dir(ix: &mut Index, name: &str, parent: Option<EntryId>) -> EntryId {
    ix.insert(EntryInput {
        name,
        parent,
        is_dir: true,
        last_change: 7,
    })
}

fn file(ix: &mut Index, name: &str, parent: EntryId) -> EntryId {
    ix.insert(EntryInput {
        name,
        parent: Some(parent),
        is_dir: false,
        last_change: 9,
    })
}

fn sample() -> Index {
    let mut ix = Index::new();
    let c = dir(&mut ix, "C:", None);
    let u = dir(&mut ix, "Users", Some(c));
    let b = dir(&mut ix, "Bütçe Klasörü", Some(u));
    let n = dir(&mut ix, "node_modules", Some(b));
    file(&mut ix, "CACHEDIR.TAG", n);
    let x = file(&mut ix, "RaporTaslağı.docx", b);
    file(&mut ix, "silinen.txt", b);
    let gone = file(&mut ix, "gone.txt", u);
    ix.remove(gone);
    ix.rename(x, "RaporSon.docx", Some(u));
    let pf = dir(&mut ix, "Prefetch", Some(c));
    ix.set_restriction(pf, Some(Restriction::NoList));
    let d = dir(&mut ix, "D:", None);
    dir(&mut ix, "veri", Some(d));
    ix
}

fn same(a: &Index, b: &Index) {
    assert_eq!(a.raw_len(), b.raw_len());
    assert_eq!(a.live_len(), b.live_len());
    for id in (0..a.raw_len() as u32).map(EntryId) {
        assert_eq!(a.is_live(id), b.is_live(id));
        assert_eq!(a.name(id), b.name(id));
        assert_eq!(a.folded(id), b.folded(id));
        assert_eq!(a.parent(id), b.parent(id));
        assert_eq!(a.is_dir(id), b.is_dir(id));
        assert_eq!(a.last_change(id), b.last_change(id));
        assert_eq!(a.child_count(id), b.child_count(id), "{}", a.path(id));
        assert_eq!(a.restriction(id), b.restriction(id));
        assert_eq!(a.camel_source(id), b.camel_source(id));
        if a.is_live(id) {
            assert_eq!(a.path_class(id), b.path_class(id), "{}", a.path(id));
            assert_eq!(a.owner(id), b.owner(id));
        }
    }
}

#[test]
fn an_image_roundtrips() {
    let a = sample();
    let mut img = vec![1, 2, 3]; // appended after what is already there
    a.to_image(&mut img);
    img.extend_from_slice(b"tail");
    let (b, used) = Index::from_image(&img[3..]).unwrap();
    assert_eq!(&img[3 + used..], b"tail");
    same(&a, &b);
    b.check_tree().unwrap();
    // and keeps working: the restored index takes changes like the first
    let (mut b, _) = Index::from_image(&img[3..]).unwrap();
    let u = EntryId(1);
    file(&mut b, "yeni.txt", u);
    assert_eq!(b.live_len(), a.live_len() + 1);
}

#[test]
fn a_damaged_image_is_refused() {
    let a = sample();
    let mut img = Vec::new();
    a.to_image(&mut img);
    for cut in [0, 7, 8, 30, img.len() / 2, img.len() - 1] {
        assert!(Index::from_image(&img[..cut]).is_err(), "cut at {cut}");
    }
    let n = a.raw_len();
    let text_len = u64::from_le_bytes(img[8..16].try_into().unwrap()) as usize;
    let cols = 16 + text_len;
    // name offset past the text
    let mut bad = img.clone();
    bad[cols..cols + 4].copy_from_slice(&u32::MAX.to_le_bytes());
    assert_eq!(
        Index::from_image(&bad).err().unwrap(),
        ImageError::Invalid("name outside the text")
    );
    // parent column: text_off 4n, name_len 2n, fold_len 2n
    let parents = cols + 8 * n;
    let set_parent = |img: &mut Vec<u8>, i: usize, p: u32| {
        img[parents + 4 * i..parents + 4 * i + 4].copy_from_slice(&p.to_le_bytes());
    };
    let mut bad = img.clone();
    set_parent(&mut bad, 0, 2); // C: under its own grandchild
    assert_eq!(
        Index::from_image(&bad).err().unwrap(),
        ImageError::Invalid("parent cycle")
    );
    let mut bad = img.clone();
    set_parent(&mut bad, 2, 5); // under a file
    assert_eq!(
        Index::from_image(&bad).err().unwrap(),
        ImageError::Invalid("parent")
    );
    let mut bad = img.clone();
    set_parent(&mut bad, 2, n as u32 + 3);
    assert_eq!(
        Index::from_image(&bad).err().unwrap(),
        ImageError::Invalid("parent")
    );
}

/// An image saved by Flint 0.1.3 (names and folded names apart) still
/// loads, so an update starts warm; equal names come back stored once.
#[test]
fn an_image_from_0_1_3_loads() {
    let a = sample();
    let mut img = vec![1, 2, 3];
    a.to_image_v2(&mut img);
    img.extend_from_slice(b"tail");
    // its layout: n, names, folded names, name_off 4n, name_len 2n,
    // fold_off 4n, fold_len 2n, parents 4n, then one flag byte an entry,
    // which never held the "stored once" bit (8)
    let at = |o: usize| u64::from_le_bytes(img[3 + o..11 + o].try_into().unwrap()) as usize;
    let n = at(0);
    let folded = 8 + 8 + at(8);
    let flags = folded + 8 + at(folded) + 16 * n;
    assert!(img[3 + flags..3 + flags + n].iter().all(|f| f & 8 == 0));
    assert!(img[3 + flags..3 + flags + n].iter().any(|f| f & 1 != 0));
    let (b, used) = Index::from_image_v2(&img[3..]).unwrap();
    assert_eq!(&img[3 + used..], b"tail");
    same(&a, &b);
    b.check_tree().unwrap();
    // every entry's current name (dead ones keep theirs), each equal
    // pair once; the text the rename left behind is dropped
    let live: Vec<EntryId> = (0..b.raw_len() as u32).map(EntryId).collect();
    let once: usize = live
        .iter()
        .map(|&id| {
            let (n, f) = (b.name(id), b.folded(id));
            n.len() + if n == f { 0 } else { f.len() }
        })
        .sum();
    assert_eq!(b.text_bytes(), once);
}

#[test]
fn garbage_ratio_counts_left_over_names() {
    let mut ix = Index::new();
    let c = dir(&mut ix, "C:", None);
    let f = file(&mut ix, "aaaaaaaa", c);
    assert!(ix.garbage_ratio() < 0.01);
    ix.rename(f, "bbbbbbbb", None);
    // "C:" + "c:" + "aaaaaaaa" (stored once) left behind by the rename:
    // 8 of 20 bytes
    let g = ix.garbage_ratio();
    assert!((g - 0.4).abs() < 0.01, "{g}");
}
