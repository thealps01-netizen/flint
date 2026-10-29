//! Ordering tests for `search()` over a small fixed corpus.

use flint_core::{
    Boosts, EntryId, EntryInput, Hit, Index, SearchOptions, SearchResult, parse, search,
};
use std::collections::HashMap;
use std::sync::atomic::AtomicBool;

const NOW: u32 = 1_800_000_000;

const PATHS: &[&str] = &[
    r"C:\Users\Deniz\Documents\Finans\2026\Bütçe_2026_Q3.xlsx",
    r"C:\Users\Deniz\Documents\Finans_Sunum.pptx",
    r"C:\Users\Deniz\Documents\aile-butcesi.ods",
    r"C:\Users\Deniz\Documents\butce_notlar.txt",
    r"C:\Users\Deniz\AppData\Local\Cache\butce_cache.json",
    r"C:\Users\Deniz\Documents\Raporlar\rapor.pdf",
    r"C:\Users\Deniz\Desktop\rapor.pdf",
    r"C:\Users\Deniz\Documents\rapor_2026.xlsx",
    r"C:\Users\Deniz\Documents\rapor_proje.docx",
    r"C:\Users\Deniz\Documents\Işık_Raporu.docx",
    r"C:\Users\Deniz\Downloads\butce_ozet.pdf",
    r"C:\Users\Deniz\Downloads\fatura_ekim.pdf",
    r"C:\Users\Deniz\Desktop\sunum_taslak.pptx",
    r"C:\Users\Deniz\Desktop\notlar.txt",
    r"C:\Users\Deniz\Pictures\deniz_tatil.jpg",
    r"C:\Users\Deniz\Pictures\🎵 müzik.mp3",
    r"C:\Code\app\node_modules\butce-lib\index.js",
    r"C:\Code\app\src\main.rs",
    r"C:\Windows\System32\drivers\etc\hosts",
    r"D:\Yedek\butce_eski.xlsx",
    r"D:\Yedek\Butce Arsiv\",
    r"D:\Yedek\Butce Arsiv\eski_rapor.pdf",
    r"E:\Muzik\sezen_aksu.mp3",
    r"C:\Program Files\Rapid\rapid-alpha-photo-renderer.exe",
    r"C:\Users\Deniz\Documents\eski_notlar.txt",
    r"C:\Users\Deniz\Documents\eski_not_listesi_ar.txt",
];

/// Entries changed right now; every other entry is years old.
const RECENT: &[&str] = &["eski_not_listesi_ar.txt"];

struct Fx {
    ix: Index,
}

impl Fx {
    fn new() -> Self {
        let mut ix = Index::new();
        let mut dirs: HashMap<String, EntryId> = HashMap::new();
        for p in PATHS {
            let is_dir = p.ends_with('\\');
            let parts: Vec<&str> = p.trim_end_matches('\\').split('\\').collect();
            let mut parent: Option<EntryId> = None;
            let mut key = String::new();
            for (i, part) in parts.iter().enumerate() {
                key.push_str(part);
                key.push('\\');
                let last = i == parts.len() - 1;
                if last && !is_dir {
                    let recent = RECENT.contains(part);
                    ix.insert(EntryInput {
                        name: part,
                        parent,
                        is_dir: false,
                        last_change: if recent { NOW } else { 0 },
                    });
                } else {
                    let id = *dirs.entry(key.clone()).or_insert_with(|| {
                        ix.insert(EntryInput {
                            name: part,
                            parent,
                            is_dir: true,
                            last_change: 0,
                        })
                    });
                    parent = Some(id);
                }
            }
        }
        Fx { ix }
    }

    fn run(&self, q: &str, boosts: &Boosts) -> SearchResult {
        self.run_limit(q, boosts, 50)
    }

    fn run_limit(&self, q: &str, boosts: &Boosts, limit: usize) -> SearchResult {
        let cancel = AtomicBool::new(false);
        let opt = SearchOptions {
            limit,
            now: NOW,
            boosts,
            cancel: &cancel,
            viewer: flint_core::OWNER_NONE,
        };
        search(&self.ix, &parse(q), &opt)
    }

    fn top(&self, q: &str) -> Vec<String> {
        self.names(&self.run(q, &Boosts::new()).hits)
    }

    fn names(&self, hits: &[Hit]) -> Vec<String> {
        hits.iter()
            .map(|h| self.ix.name(h.id).to_string())
            .collect()
    }
}

fn assert_before(list: &[String], a: &str, b: &str) {
    let ia = list
        .iter()
        .position(|n| n == a)
        .unwrap_or_else(|| panic!("{a} missing in {list:?}"));
    let ib = list
        .iter()
        .position(|n| n == b)
        .unwrap_or_else(|| panic!("{b} missing in {list:?}"));
    assert!(ia < ib, "{a} should rank before {b}: {list:?}");
}

#[test]
fn prefix_beats_midword() {
    assert_before(
        &Fx::new().top("butce"),
        "Bütçe_2026_Q3.xlsx",
        "aile-butcesi.ods",
    );
}

#[test]
fn single_term_matches_names_only() {
    let r = Fx::new().top("finans");
    assert!(r.iter().any(|n| n == "Finans_Sunum.pptx"), "{r:?}");
    // under \Finans\ but "finans" is not in its name
    assert!(!r.iter().any(|n| n == "Bütçe_2026_Q3.xlsx"), "{r:?}");
}

#[test]
fn path_only_needs_a_name_term() {
    // "deniz" and "finans" are both only folder names for Bütçe_2026_Q3.xlsx
    let r = Fx::new().top("finans deniz");
    assert!(r.iter().any(|n| n == "Finans_Sunum.pptx"), "{r:?}");
    assert!(!r.iter().any(|n| n == "Bütçe_2026_Q3.xlsx"), "{r:?}");
}

#[test]
fn multi_term_and() {
    let r = Fx::new().top("finans butce");
    assert_eq!(r[0], "Bütçe_2026_Q3.xlsx");
    assert!(!r.iter().any(|n| n == "aile-butcesi.ods"), "{r:?}");
}

#[test]
fn frecency_lifts_equal_match() {
    let fx = Fx::new();
    let target = r"C:\Users\Deniz\Desktop\rapor.pdf";
    let plain = fx.run("rapor.pdf", &Boosts::new());
    let mut boosts = Boosts::new();
    boosts.add(target, 1.0);
    let boosted = fx.run("rapor.pdf", &boosts);
    assert_eq!(fx.ix.path(boosted.hits[0].id), target);
    // the boost changed the order, so the test does not pass by accident
    assert_ne!(fx.ix.path(plain.hits[0].id), target);
}

#[test]
fn appdata_demoted() {
    assert_before(
        &Fx::new().top("butce"),
        "butce_notlar.txt",
        "butce_cache.json",
    );
}

#[test]
fn typo_hits_are_flagged_and_highlighted() {
    let fx = Fx::new();
    let r = fx.run("rapro", &Boosts::new());
    let h = r
        .hits
        .iter()
        .find(|h| fx.ix.name(h.id) == "rapor.pdf")
        .unwrap_or_else(|| panic!("rapor.pdf missing: {:?}", fx.names(&r.hits)));
    assert!(h.typo);
    assert_eq!(h.highlights, vec![(0, 5)]);
}

#[test]
fn typo_tier_runs_even_when_fuzzy_fills_the_limit() {
    // The limit equals the number of fuzzy hits, so fuzzy alone could fill
    // the list; the typo hit at the start of a name must still get in.
    let fx = Fx::new();
    let all = fx.run_limit("rapro", &Boosts::new(), 1000);
    let fuzzy = all.hits.iter().filter(|h| !h.typo).count();
    assert!(fuzzy >= 1, "no fuzzy hits; the test would be vacuous");
    let r = fx.run_limit("rapro", &Boosts::new(), fuzzy);
    let names = fx.names(&r.hits);
    assert!(names.iter().any(|n| n == "rapor.pdf"), "{names:?}");
}

#[test]
fn scattered_letters_do_not_match() {
    // r-a-p-o-r occur in order, but spread over 19 characters
    let r = Fx::new().top("rapor");
    assert!(
        !r.iter().any(|n| n == "rapid-alpha-photo-renderer.exe"),
        "{r:?}"
    );
}

#[test]
fn match_quality_beats_recency() {
    // the scattered-but-compact match was changed just now; the clean
    // substring match is years old
    assert_before(
        &Fx::new().top("notlar"),
        "eski_notlar.txt",
        "eski_not_listesi_ar.txt",
    );
}

#[test]
fn filters() {
    let fx = Fx::new();
    let r = fx.run("d: butce", &Boosts::new());
    assert!(!r.hits.is_empty());
    assert!(r.hits.iter().all(|h| fx.ix.drive(h.id) == 'd'));
    let r = fx.run("butce ext:pdf", &Boosts::new());
    assert!(!r.hits.is_empty());
    assert!(r.hits.iter().all(|h| fx.ix.name(h.id).ends_with(".pdf")));
    let r = fx.run("butce/", &Boosts::new());
    assert!(!r.hits.is_empty());
    assert!(r.hits.iter().all(|h| fx.ix.is_dir(h.id)));
}

#[test]
fn highlights_on_original() {
    let fx = Fx::new();
    let r = fx.run("butce", &Boosts::new());
    assert_eq!(r.hits[0].highlights, vec![(0, 5)]);
    let m = fx.run("muzik", &Boosts::new());
    let h = m
        .hits
        .iter()
        .find(|h| fx.ix.name(h.id) == "🎵 müzik.mp3")
        .unwrap();
    assert_eq!(h.highlights, vec![(3, 8)]);
}

#[test]
fn empty_query_no_hits() {
    let r = Fx::new().run("   ", &Boosts::new());
    assert!(r.hits.is_empty());
    assert_eq!(r.total_matches, 0);
}

#[test]
fn total_counts_all() {
    assert!(Fx::new().run("e", &Boosts::new()).total_matches > 10);
}

#[test]
fn roots_are_never_hits() {
    let fx = Fx::new();
    assert!(!fx.top("c").iter().any(|n| n == "C:"));
}

/// The typo tier treats a camelCase boundary as a word start
/// ("reprot" finds QuarterlyReport.xlsx), not a lowercase word interior.
#[test]
fn typo_matches_camel_case_word() {
    let mut ix = Index::new();
    let c = ix.insert(EntryInput {
        name: "C:",
        parent: None,
        is_dir: true,
        last_change: 0,
    });
    for n in [
        "QuarterlyReport.xlsx",
        "quarterly_report.xlsx",
        "Unreported.txt",
    ] {
        ix.insert(EntryInput {
            name: n,
            parent: Some(c),
            is_dir: false,
            last_change: 0,
        });
    }
    let cancel = AtomicBool::new(false);
    let b = Boosts::new();
    let opt = SearchOptions {
        limit: 50,
        now: NOW,
        boosts: &b,
        cancel: &cancel,
        viewer: flint_core::OWNER_NONE,
    };
    let r = search(&ix, &parse("reprot"), &opt);
    let names: Vec<&str> = r.hits.iter().map(|h| ix.name(h.id)).collect();
    assert!(names.contains(&"QuarterlyReport.xlsx"), "{names:?}");
    assert!(!names.contains(&"Unreported.txt"), "{names:?}");
    let camel = r
        .hits
        .iter()
        .find(|h| ix.name(h.id) == "QuarterlyReport.xlsx")
        .unwrap();
    assert_eq!(
        camel.highlights,
        vec![(9, 15)],
        "highlight on the typo'd word"
    );
}

/// Build output (a folder with CACHEDIR.TAG) ranks below the sources.
#[test]
fn build_output_below_source() {
    let mut ix = Index::new();
    let d = |ix: &mut Index, n: &str, p: Option<flint_core::EntryId>| {
        ix.insert(EntryInput {
            name: n,
            parent: p,
            is_dir: true,
            last_change: 0,
        })
    };
    let c = d(&mut ix, "C:", None);
    let proj = d(&mut ix, "proj", Some(c));
    let target = d(&mut ix, "target", Some(proj));
    let src = d(&mut ix, "src", Some(proj));
    for (n, p) in [
        ("CACHEDIR.TAG", target),
        ("flint.exe", target),
        ("flint.rs", src),
    ] {
        ix.insert(EntryInput {
            name: n,
            parent: Some(p),
            is_dir: false,
            last_change: 0,
        });
    }
    let cancel = AtomicBool::new(false);
    let b = Boosts::new();
    let opt = SearchOptions {
        limit: 50,
        now: NOW,
        boosts: &b,
        cancel: &cancel,
        viewer: flint_core::OWNER_NONE,
    };
    let r = search(&ix, &parse("flint"), &opt);
    let names: Vec<&str> = r.hits.iter().map(|h| ix.name(h.id)).collect();
    assert_eq!(names, vec!["flint.rs", "flint.exe"]);
}
