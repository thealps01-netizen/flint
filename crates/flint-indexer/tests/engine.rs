//! The engine in user mode (no administrator rights needed): walk, watch,
//! snapshot save, warm start and a prompt stop.
#![cfg(windows)]

use flint_indexer::engine::{Config, Engine, StartKind};
use flint_indexer::pipe::{Client, current_user_sid};
use flint_indexer::protocol::ServerMsg;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

fn tmp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("flint-engine-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let d = d.canonicalize().unwrap();
    PathBuf::from(d.to_string_lossy().trim_start_matches(r"\\?\"))
}

fn pipe(tag: &str) -> String {
    format!(r"\\.\pipe\flint-test-engine-{tag}-{}", std::process::id())
}

fn tree(root: &Path) {
    std::fs::create_dir_all(root.join("a")).unwrap();
    std::fs::create_dir_all(root.join("b")).unwrap();
    std::fs::write(root.join(r"a\bütçe-2026.xlsx"), b"x").unwrap();
    std::fs::write(root.join(r"b\rapor.txt"), b"x").unwrap();
    std::fs::write(root.join("silinecek.txt"), b"x").unwrap();
}

fn hits(name: &str, q: &str) -> Vec<String> {
    let c = Client::connect(name, &current_user_sid().unwrap()).unwrap();
    match c.query(q, 20, vec![]).unwrap() {
        ServerMsg::Results { hits, .. } => hits.into_iter().map(|h| h.path).collect(),
        m => panic!("{m:?}"),
    }
}

fn eventually(what: &str, mut f: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(20);
    while !f() {
        assert!(Instant::now() < end, "timed out: {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn engine_warm_start_from_snapshot() {
    let root = tmp("warm");
    let data = tmp("warm-data");
    tree(&root);
    let name = pipe("warm1");
    let e = Engine::start(Config::user(vec![root.clone()], data.clone(), name.clone()).unwrap())
        .unwrap();
    assert_eq!(e.start_kind(), StartKind::Cold);
    assert!(e.wait_live(Duration::from_secs(20)), "first walk finished");
    // saved as soon as the first walk is done: a killed process does not
    // walk again from scratch on its next start
    eventually("saved after the first walk", || {
        data.join("index-user.bin").is_file()
    });
    let n = e.shared().index.read().unwrap().live_len();
    assert!(hits(&name, "silinecek").len() == 1);
    e.stop();
    assert!(data.join("index-user.bin").is_file(), "saved on stop");

    // changed while stopped
    std::fs::remove_file(root.join("silinecek.txt")).unwrap();
    let name = pipe("warm2");
    let e = Engine::start(Config::user(vec![root.clone()], data.clone(), name.clone()).unwrap())
        .unwrap();
    assert_eq!(e.start_kind(), StartKind::Warm);
    assert_eq!(
        e.loaded_from_snapshot(),
        n,
        "the snapshot holds what was indexed"
    );
    // the walk brings it up to date
    assert!(e.wait_live(Duration::from_secs(20)));
    assert_eq!(
        hits(&name, "bütçe 2026"),
        vec![root.join(r"a\bütçe-2026.xlsx").display().to_string()]
    );
    assert!(hits(&name, "silinecek").is_empty(), "deleted while stopped");
    e.stop();
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&data);
}

#[test]
fn a_damaged_snapshot_is_discarded() {
    let root = tmp("bad");
    let data = tmp("bad-data");
    tree(&root);
    std::fs::write(data.join("index-user.bin"), b"FLNTIDX\0garbage").unwrap();
    let name = pipe("bad");
    let e = Engine::start(Config::user(vec![root.clone()], data.clone(), name.clone()).unwrap())
        .unwrap();
    assert_eq!(e.start_kind(), StartKind::Cold);
    assert!(e.wait_live(Duration::from_secs(20)));
    assert_eq!(hits(&name, "rapor").len(), 1);
    e.stop();
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&data);
}

#[test]
fn engine_follows_changes_and_saves_while_running() {
    let root = tmp("live");
    let data = tmp("live-data");
    tree(&root);
    let name = pipe("live");
    let mut cfg = Config::user(vec![root.clone()], data.clone(), name.clone()).unwrap();
    cfg.save_every = Duration::from_millis(200);
    let e = Engine::start(cfg).unwrap();
    assert!(e.wait_live(Duration::from_secs(20)));
    std::fs::write(root.join(r"b\yeni-sunum.pptx"), b"x").unwrap();
    std::fs::rename(root.join(r"b\rapor.txt"), root.join(r"a\rapor-son.txt")).unwrap();
    eventually("new file indexed", || hits(&name, "yeni sunum").len() == 1);
    eventually("move indexed", || {
        hits(&name, "rapor") == vec![root.join(r"a\rapor-son.txt").display().to_string()]
    });
    eventually("saved while running", || {
        data.join("index-user.bin").is_file()
    });
    // renames leave old names behind; past the threshold a save compacts,
    // and the index keeps following changes afterwards
    let mut cur = root.join(r"a\rapor-son.txt");
    for i in 0..60 {
        let next = root.join(format!(r"a\uzun-bir-yeniden-adlandirma-{i:03}.txt"));
        std::fs::rename(&cur, &next).unwrap();
        cur = next;
    }
    eventually("renames indexed", || {
        hits(&name, "adlandirma 059").len() == 1
    });
    eventually("compacted", || {
        let ix = e.shared().index.read().unwrap();
        ix.garbage_ratio() < 0.3 && ix.raw_len() == ix.live_len()
    });
    std::fs::write(root.join(r"b\sonra.txt"), b"x").unwrap();
    eventually("followed after compaction", || {
        hits(&name, "sonra").len() == 1
    });
    e.stop();
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&data);
}

#[test]
fn engine_stop_is_prompt() {
    // idle: the watch is blocked in the kernel
    let root = tmp("stop");
    let data = tmp("stop-data");
    tree(&root);
    let e = Engine::start(Config::user(vec![root.clone()], data.clone(), pipe("stop")).unwrap())
        .unwrap();
    assert!(e.wait_live(Duration::from_secs(20)));
    let t = Instant::now();
    e.stop();
    assert!(
        t.elapsed() < Duration::from_secs(5),
        "idle stop took {:?}",
        t.elapsed()
    );
    // busy: in the middle of walking a large folder (read only: names
    // and dates)
    let windir = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
    let e = Engine::start(
        Config::user(vec![PathBuf::from(windir)], data.clone(), pipe("stop2")).unwrap(),
    )
    .unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let t = Instant::now();
    e.stop();
    assert!(
        t.elapsed() < Duration::from_secs(5),
        "busy stop took {:?}",
        t.elapsed()
    );
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&data);
}

#[test]
fn engine_searches_in_process_like_its_pipe() {
    let root = tmp("inproc");
    let data = tmp("inproc-data");
    tree(&root);
    let name = pipe("inproc");
    let e = Engine::start(Config::user(vec![root.clone()], data, name.clone()).unwrap()).unwrap();
    assert!(e.wait_live(Duration::from_secs(20)));
    let boosts = vec![(
        root.join(r"b\rapor.txt").to_string_lossy().into_owned(),
        1.0,
    )];
    let ServerMsg::Results {
        hits: local, total, ..
    } = e.search("rapor", 10, &boosts)
    else {
        panic!("in-process search failed")
    };
    assert_eq!(total, 1);
    assert_eq!(local[0].path, root.join(r"b\rapor.txt").to_string_lossy());
    let c = Client::connect(&name, &current_user_sid().unwrap()).unwrap();
    let ServerMsg::Results { hits: piped, .. } = c.query("rapor", 10, boosts).unwrap() else {
        panic!()
    };
    // the score's recency part counts seconds from now: a second boundary
    // between the two searches may move it by one point
    let strip = |h: &[flint_indexer::protocol::WireHit]| {
        h.iter()
            .map(|h| (h.path.clone(), h.typo, h.highlights.clone(), h.last_change))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        strip(&local),
        strip(&piped),
        "same answer in-process and over the pipe"
    );
    for (a, b) in local.iter().zip(&piped) {
        assert!((a.score - b.score).abs() <= 1, "{} vs {}", a.score, b.score);
    }
    e.stop();
}

#[test]
fn a_skipped_folder_is_not_indexed_but_a_root_inside_it_is() {
    let root = tmp("skip");
    let data = tmp("skip-data");
    let app = root.join(r"AppData\Roaming");
    let menu = app.join(r"Microsoft\Windows\Start Menu\Programs");
    std::fs::create_dir_all(&menu).unwrap();
    std::fs::write(app.join("oyun-kaydi.sav"), b"x").unwrap();
    std::fs::write(menu.join("Discord.lnk"), b"x").unwrap();
    std::fs::write(root.join("rapor.txt"), b"x").unwrap();
    let name = pipe("skip");
    let cfg = Config::user(vec![root.clone(), menu.clone()], data.clone(), name.clone())
        .unwrap()
        .skipping(vec![app.clone()]);
    let e = Engine::start(cfg).unwrap();
    assert!(e.wait_live(Duration::from_secs(20)));
    assert_eq!(hits(&name, "rapor").len(), 1);
    assert!(
        hits(&name, "oyun kaydi").is_empty(),
        "inside the skipped folder"
    );
    assert_eq!(
        hits(&name, "roaming"),
        vec![app.display().to_string()],
        "the skipped folder itself is still found"
    );
    assert_eq!(
        hits(&name, "discord"),
        vec![menu.join("Discord.lnk").display().to_string()],
        "a root inside the skipped folder is walked"
    );

    // changes made while running follow the same rule
    std::fs::write(app.join("yeni-kayit.sav"), b"x").unwrap();
    std::fs::write(menu.join("Spotify.lnk"), b"x").unwrap();
    std::fs::write(root.join("yeni-rapor.txt"), b"x").unwrap();
    eventually("a new file outside", || {
        hits(&name, "yeni rapor").len() == 1
    });
    eventually("a new shortcut", || hits(&name, "spotify").len() == 1);
    assert!(
        hits(&name, "yeni kayit").is_empty(),
        "a change inside the skipped folder"
    );
    e.stop();
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&data);
}
