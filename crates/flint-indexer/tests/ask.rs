//! Searching several running indexers at once (`ask`): merged, one hit
//! per path, hidden folders dropped, and a clear error when none answers.
#![cfg(windows)]

use flint_indexer::ask::{Found, folder_keys, hidden_from_assistants, search_pipes};
use flint_indexer::engine::{Config, Engine};
use flint_indexer::pipe::current_user_sid;
use std::path::PathBuf;
use std::time::Duration;

fn tmp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("flint-ask-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let d = d.canonicalize().unwrap();
    PathBuf::from(d.to_string_lossy().trim_start_matches(r"\\?\"))
}

fn pipe(tag: &str) -> String {
    format!(r"\\.\pipe\flint-test-ask-{tag}-{}", std::process::id())
}

fn start(roots: Vec<PathBuf>, tag: &str) -> (Engine, String) {
    let name = pipe(tag);
    let e = Engine::start(Config::user(roots, tmp(&format!("{tag}-data")), name.clone()).unwrap())
        .unwrap();
    assert!(e.wait_live(Duration::from_secs(20)), "{tag} walked");
    (e, name)
}

#[test]
fn every_running_indexer_is_asked_and_hidden_folders_are_dropped() {
    let (a, b, both) = (tmp("a"), tmp("b"), tmp("both"));
    std::fs::write(a.join("rapor-a.txt"), b"x").unwrap();
    std::fs::write(b.join("rapor-b.txt"), b"x").unwrap();
    std::fs::write(both.join("rapor-ortak.txt"), b"x").unwrap();
    let (ea, pa) = start(vec![a.clone(), both.clone()], "ea");
    let (eb, pb) = start(vec![b.clone(), both.clone()], "eb");
    let me = current_user_sid().unwrap();
    let pipes = vec![
        (pa, me.clone()),
        (pipe("not-running"), me.clone()),
        (pb, me.clone()),
    ];

    let f = search_pipes(&pipes, "rapor", 20, &[]).unwrap();
    let mut names: Vec<_> = f.hits.iter().map(|h| h.name.as_str()).collect();
    names.sort();
    // the shared file is indexed by both and listed once
    assert_eq!(names, ["rapor-a.txt", "rapor-b.txt", "rapor-ortak.txt"]);

    let hidden = folder_keys(std::slice::from_ref(&b));
    let f = search_pipes(&pipes, "rapor", 20, &hidden).unwrap();
    assert!(
        f.hits
            .iter()
            .all(|h| !h.path.starts_with(&*b.to_string_lossy()))
    );
    assert_eq!(f.hits.len(), 2);

    // the limit applies to the merged list
    assert_eq!(search_pipes(&pipes, "rapor", 1, &[]).unwrap().hits.len(), 1);

    ea.stop();
    eb.stop();
    let e = search_pipes(&pipes, "rapor", 20, &[]).expect_err("none answers");
    assert!(e.to_string().contains("Flint is not running"), "{e}");
}

#[test]
fn hidden_hits_do_not_shorten_the_list() {
    // the best matches all sit in a hidden folder; the limit still fills
    let (hid, open) = (tmp("hid"), tmp("open"));
    for i in 0..3 {
        std::fs::create_dir_all(hid.join(i.to_string())).unwrap();
        std::fs::write(hid.join(i.to_string()).join("rapor.txt"), b"x").unwrap();
    }
    std::fs::write(open.join("rapor-uzun-bir-isim-ile.txt"), b"x").unwrap();
    let (e, p) = start(vec![hid.clone(), open.clone()], "eh");
    let pipes = vec![(p, current_user_sid().unwrap())];
    let all = search_pipes(&pipes, "rapor", 1, &[]).unwrap();
    assert!(
        all.hits[0].path.starts_with(&*hid.to_string_lossy()),
        "a hidden hit is best"
    );
    let f = search_pipes(&pipes, "rapor", 1, &folder_keys(&[hid])).unwrap();
    assert_eq!(f.hits.len(), 1);
    assert_eq!(f.hits[0].name, "rapor-uzun-bir-isim-ile.txt");
    e.stop();
}

#[test]
fn a_mute_indexer_costs_at_most_its_timeout_and_marks_the_answer_partial() {
    // accepts the connection, never says Welcome
    let mute = pipe("mute");
    let me = current_user_sid().unwrap();
    let _server = flint_indexer::pipe::serve(
        &mute,
        &flint_indexer::pipe::user_sddl(&me),
        std::sync::Arc::new(|_c| std::thread::sleep(Duration::from_secs(30))),
    )
    .unwrap();
    let live = tmp("live");
    std::fs::write(live.join("rapor-canli.txt"), b"x").unwrap();
    let (e, lp) = start(vec![live], "elive");
    let t = std::time::Instant::now();
    let f = search_pipes(
        &[(mute.clone(), me.clone()), (lp, me.clone())],
        "rapor",
        20,
        &[],
    )
    .unwrap();
    assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
    assert_eq!(f.hits.len(), 1);
    assert_eq!(f.failed, 1, "the mute indexer is reported");
    e.stop();
    let e = search_pipes(&[(mute, me)], "rapor", 20, &[]).expect_err("only the mute one");
    assert!(e.to_string().contains("did not answer"), "{e}");
}

#[test]
fn a_hidden_folder_picked_through_a_junction_hides_the_real_one() {
    let real = tmp("real");
    std::fs::create_dir_all(real.join("Gizli")).unwrap();
    std::fs::write(real.join("Gizli").join("rapor-gizli.txt"), b"x").unwrap();
    std::fs::write(real.join("rapor-acik.txt"), b"x").unwrap();
    let link = tmp("link").join("kisayol");
    let ok = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(&link)
        .arg(&real)
        .stdout(std::process::Stdio::null())
        .status()
        .unwrap()
        .success();
    assert!(ok, "junction made");
    let settings = tmp("jset").join("settings.json");
    let body = serde_json::json!({ "aiHiddenFolders": [link.join("Gizli")] });
    std::fs::write(&settings, body.to_string()).unwrap();
    let hidden = hidden_from_assistants(Some(&settings)).unwrap();
    let (e, p) = start(vec![real.clone()], "ejunc");
    let f = search_pipes(&[(p, current_user_sid().unwrap())], "rapor", 20, &hidden).unwrap();
    let names: Vec<_> = f.hits.iter().map(|h| h.name.as_str()).collect();
    assert_eq!(names, ["rapor-acik.txt"]);
    e.stop();
    let _ = std::fs::remove_dir(&link);
}

struct Pipes(Vec<(String, String)>);

impl flint_indexer::mcp::Searcher for Pipes {
    fn search(&self, text: &str, limit: u32) -> Result<Found, String> {
        search_pipes(&self.0, text, limit, &[]).map_err(|e| e.to_string())
    }
}

#[test]
fn under_finds_a_file_beyond_the_best_matches_of_the_whole_index() {
    let root = tmp("under");
    let many = root.join("Cok");
    std::fs::create_dir_all(&many).unwrap();
    for i in 0..300 {
        std::fs::write(many.join(format!("rapor{i}.txt")), b"x").unwrap();
    }
    let target = root.join("Derin").join("Hedef Klasör");
    std::fs::create_dir_all(&target).unwrap();
    std::fs::write(target.join("rapor-uzun-bir-isimle-yazilmis.txt"), b"x").unwrap();
    let (e, p) = start(vec![root.clone()], "eunder");
    let mut s = flint_indexer::mcp::Server::new(Pipes(vec![(p, current_user_sid().unwrap())]));
    let ask =
        |s: &mut flint_indexer::mcp::Server<Pipes>, msg: serde_json::Value| -> serde_json::Value {
            let out = s
                .handle(msg.to_string().as_bytes(), std::time::Instant::now())
                .unwrap();
            serde_json::from_str(&out).unwrap()
        };
    ask(
        &mut s,
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}),
    );
    let r = ask(
        &mut s,
        serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"search","arguments":{"query":"rapor","limit":5,"under":target.to_string_lossy()}}}),
    );
    let res = r["result"]["structuredContent"]["results"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(res.len(), 1, "{r}");
    assert_eq!(res[0]["name"], "rapor-uzun-bir-isimle-yazilmis.txt");
    // folders only, also beyond the best 200 files
    let r = ask(
        &mut s,
        serde_json::json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"search","arguments":{"query":"hedef","kind":"folder"}}}),
    );
    let res = r["result"]["structuredContent"]["results"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(res.len(), 1, "{r}");
    assert_eq!(res[0]["name"], "Hedef Klasör");
    e.stop();
}

#[test]
fn the_count_tells_nothing_about_hidden_matches() {
    let hid = tmp("count");
    for i in 0..6 {
        std::fs::create_dir_all(hid.join(i.to_string())).unwrap();
        std::fs::write(hid.join(i.to_string()).join("parola.txt"), b"x").unwrap();
    }
    let (e, p) = start(vec![hid.clone()], "ecount");
    let pipes = vec![(p, current_user_sid().unwrap())];
    assert_eq!(search_pipes(&pipes, "parola", 1, &[]).unwrap().total, 6);
    let f = search_pipes(&pipes, "parola", 1, &folder_keys(&[hid])).unwrap();
    assert!(f.hits.is_empty());
    assert_eq!(
        f.total, 0,
        "hidden matches beyond those fetched are not counted"
    );
    e.stop();
}

#[test]
fn an_indexer_that_greets_but_never_answers_is_reported() {
    use flint_indexer::protocol::{ClientMsg, PROTOCOL_VERSION, ServerMsg};
    let slow = pipe("slow");
    let me = current_user_sid().unwrap();
    let _server = flint_indexer::pipe::serve(
        &slow,
        &flint_indexer::pipe::user_sddl(&me),
        std::sync::Arc::new(|c: std::sync::Arc<flint_indexer::pipe::PipeConn>| {
            let _ = c.recv::<ClientMsg>();
            let _ = c.send(&ServerMsg::Welcome {
                version: PROTOCOL_VERSION,
                mode: "user".into(),
            });
            std::thread::sleep(Duration::from_secs(30));
        }),
    )
    .unwrap();
    let live = tmp("live2");
    std::fs::write(live.join("rapor-canli.txt"), b"x").unwrap();
    let (e, lp) = start(vec![live], "elive2");
    let t = std::time::Instant::now();
    let f = search_pipes(&[(slow, me.clone()), (lp, me)], "rapor", 20, &[]).unwrap();
    assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
    assert_eq!(f.hits.len(), 1);
    assert_eq!(f.failed, 1);
    e.stop();
}

/// A pipe that greets and then answers every query with `code: message`.
fn refusing(tag: &str, code: &'static str) -> (String, flint_indexer::pipe::ServerHandle) {
    use flint_indexer::protocol::{ClientMsg, PROTOCOL_VERSION, ServerMsg};
    let name = pipe(tag);
    let me = current_user_sid().unwrap();
    let server = flint_indexer::pipe::serve(
        &name,
        &flint_indexer::pipe::user_sddl(&me),
        std::sync::Arc::new(move |c: std::sync::Arc<flint_indexer::pipe::PipeConn>| {
            let _ = c.recv::<ClientMsg>();
            let _ = c.send(&ServerMsg::Welcome {
                version: PROTOCOL_VERSION,
                mode: "service".into(),
            });
            while c.recv::<ClientMsg>().is_ok() {
                let _ = c.send(&ServerMsg::Error {
                    code: code.into(),
                    message: "not ready".into(),
                });
            }
        }),
    )
    .unwrap();
    (name, server)
}

#[test]
fn an_index_still_starting_makes_the_answer_partial() {
    let (starting, _s) = refusing("starting", "starting");
    let live = tmp("live3");
    std::fs::write(live.join("rapor-canli.txt"), b"x").unwrap();
    let (e, lp) = start(vec![live], "elive3");
    let me = current_user_sid().unwrap();
    let f = search_pipes(&[(starting, me.clone()), (lp, me)], "rapor", 20, &[]).unwrap();
    assert_eq!(f.hits.len(), 1);
    assert_eq!(f.failed, 1, "the starting index is reported");
    e.stop();
}

#[test]
fn a_query_every_index_refuses_gets_their_reason_not_a_timeout() {
    let live = tmp("refuse");
    let (e, lp) = start(vec![live], "erefuse");
    let words = (0..17)
        .map(|i| format!("w{i}"))
        .collect::<Vec<_>>()
        .join(" ");
    let err =
        search_pipes(&[(lp, current_user_sid().unwrap())], &words, 20, &[]).expect_err("17 words");
    assert!(err.to_string().contains("at most 16 words"), "{err}");
    e.stop();
}

#[test]
fn with_hidden_folders_the_count_says_whether_it_is_exact() {
    let root = tmp("exact");
    for i in 0..6 {
        std::fs::write(root.join(format!("parola{i}.txt")), b"x").unwrap();
    }
    let hid = root.join("Gizli");
    std::fs::create_dir_all(&hid).unwrap();
    std::fs::write(hid.join("parola-gizli.txt"), b"x").unwrap();
    let (e, p) = start(vec![root], "eexact");
    let pipes = vec![(p, current_user_sid().unwrap())];
    let keys = folder_keys(&[hid]);
    let f = search_pipes(&pipes, "parola", 20, &keys).unwrap();
    assert_eq!((f.total, f.exact, f.window_full), (6, true, false));
    // limit 1 asks for 4: the window is full, the count is a lower bound
    let f = search_pipes(&pipes, "parola", 1, &keys).unwrap();
    assert_eq!(f.hits.len(), 1);
    assert!(!f.exact && f.window_full);
    // nothing hidden: the index's own count
    let f = search_pipes(&pipes, "parola", 1, &[]).unwrap();
    assert_eq!((f.total, f.exact), (7, true));
    e.stop();
}
