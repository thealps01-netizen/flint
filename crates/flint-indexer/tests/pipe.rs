//! Named-pipe protocol tests: a server in this process on a unique pipe
//! name, real Win32 pipes, real clients.
#![cfg(windows)]

use flint_core::{Boosts, EntryInput, Index, OWNER_NONE, SearchOptions, parse, search};
use flint_indexer::pipe::{self, Client, connect_raw, current_user_sid, serve_index, user_sddl};
use flint_indexer::protocol::{ClientMsg, MAX_FRAME, ServerMsg, WireHit};
use flint_indexer::server::{Conn, Shared};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

const NOW: u32 = 1_800_000_000;

fn me() -> String {
    current_user_sid().unwrap()
}

fn pipe_name() -> String {
    static N: AtomicU32 = AtomicU32::new(0);
    format!(
        r"\\.\pipe\flint-test-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    )
}

fn small_index() -> Index {
    let mut ix = Index::new();
    let c = ix.insert(EntryInput {
        name: "C:",
        parent: None,
        is_dir: true,
        last_change: 0,
    });
    let d = ix.insert(EntryInput {
        name: "Belgeler",
        parent: Some(c),
        is_dir: true,
        last_change: 0,
    });
    for (n, t) in [
        ("rapor.docx", 10),
        ("Rapor 2025.pdf", 20),
        ("bütçe.xlsx", 30),
        ("notlar.txt", 40),
    ] {
        ix.insert(EntryInput {
            name: n,
            parent: Some(d),
            is_dir: false,
            last_change: t,
        });
    }
    ix
}

/// An index big enough that a broad query takes a while.
fn big_index(n: u32) -> Index {
    let mut ix = Index::new();
    let c = ix.insert(EntryInput {
        name: "C:",
        parent: None,
        is_dir: true,
        last_change: 0,
    });
    let dirs: Vec<_> = (0..500)
        .map(|i| {
            ix.insert(EntryInput {
                name: &format!("klasor{i}"),
                parent: Some(c),
                is_dir: true,
                last_change: 0,
            })
        })
        .collect();
    for i in 0..n {
        let name = format!("dosya{}_{}rapor{}.txt", i % 97, i % 13, i);
        ix.insert(EntryInput {
            name: &name,
            parent: Some(dirs[(i % 500) as usize]),
            is_dir: false,
            last_change: i,
        });
    }
    ix
}

fn start(ix: Index) -> (String, Arc<Shared>, pipe::ServerHandle) {
    let name = pipe_name();
    let mut shared = Shared::new(ix, "user");
    shared.clock = || NOW;
    let shared = Arc::new(shared);
    let sid = current_user_sid().unwrap();
    let h = serve_index(&name, &user_sddl(&sid), shared.clone()).unwrap();
    (name, shared, h)
}

#[test]
fn hello_query_roundtrip() {
    let (name, shared, _h) = start(small_index());
    let c = Client::connect(&name, &me()).unwrap();
    assert_eq!(c.mode, "user");
    let ServerMsg::Results { hits, total, .. } = c.query("rapor", 10, vec![]).unwrap() else {
        panic!()
    };
    // identical to an in-process search
    let ix = shared.index.read().unwrap();
    let b = Boosts::new();
    let cancel = std::sync::atomic::AtomicBool::new(false);
    let r = search(
        &ix,
        &parse("rapor"),
        &SearchOptions {
            limit: 10,
            now: NOW,
            boosts: &b,
            cancel: &cancel,
            viewer: OWNER_NONE,
        },
    );
    let want: Vec<WireHit> = r
        .hits
        .iter()
        .map(|h| WireHit {
            path: ix.path(h.id),
            name: ix.name(h.id).to_string(),
            is_dir: ix.is_dir(h.id),
            typo: h.typo,
            highlights: h.highlights.clone(),
            last_change: ix.last_change(h.id),
            score: h.score,
        })
        .collect();
    assert_eq!(hits, want);
    // hits carry their scores, best first (the app merges two sources by them)
    assert!(hits.windows(2).all(|w| w[0].score >= w[1].score));
    assert!(hits[0].score > 0);
    assert_eq!(total, r.total_matches);
    assert_eq!(hits[0].path, r"C:\Belgeler\rapor.docx");
    let ServerMsg::StatusReply { entries, .. } = c.status().unwrap() else {
        panic!()
    };
    assert_eq!(entries, 6);
}

#[test]
fn version_mismatch_is_reported() {
    let (name, _s, _h) = start(small_index());
    let c = connect_raw(&name).unwrap();
    c.send(&ClientMsg::Hello { version: 999 }).unwrap();
    let ServerMsg::Error { code, message } = c.recv::<ServerMsg>().unwrap() else {
        panic!()
    };
    assert_eq!(code, "version");
    assert!(message.contains("999"), "{message}");
    assert!(c.read_frame().is_err(), "the server closes the connection");
}

#[test]
fn query_before_hello_is_refused() {
    let (name, _s, _h) = start(small_index());
    let c = connect_raw(&name).unwrap();
    c.send(&ClientMsg::Status).unwrap();
    let ServerMsg::Error { code, .. } = c.recv::<ServerMsg>().unwrap() else {
        panic!()
    };
    assert_eq!(code, "protocol");
}

#[test]
fn new_query_cancels_old() {
    let (name, _s, _h) = start(big_index(400_000));
    let c = Client::connect(&name, &me()).unwrap();
    let s1 = c.send_query("ra", 50, vec![]).unwrap();
    let s2 = c.send_query("rapor1", 50, vec![]).unwrap();
    let s3 = c.send_query("dosya5", 50, vec![]).unwrap();
    // exactly one answer per query, in order; the last one has results
    let mut answers = Vec::new();
    while answers.len() < 3 {
        match c.recv().unwrap() {
            ServerMsg::Results { seq, .. } => answers.push((seq, true)),
            ServerMsg::Cancelled { seq } => answers.push((seq, false)),
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(
        answers.iter().map(|a| a.0).collect::<Vec<_>>(),
        vec![s1, s2, s3]
    );
    assert!(answers[2].1, "the newest query is answered with results");
}

#[test]
fn next_client_works_after_one_leaves_mid_query() {
    let (name, _s, _h) = start(big_index(400_000));
    {
        let c = Client::connect(&name, &me()).unwrap();
        c.send_query("ra", 50, vec![]).unwrap();
        // dropped here, while the query may still run
    }
    let c = Client::connect(&name, &me()).unwrap();
    assert!(matches!(
        c.query("dosya1", 5, vec![]).unwrap(),
        ServerMsg::Results { .. }
    ));
}

#[test]
fn oversized_frame_closes() {
    let (name, _s, _h) = start(small_index());
    let c = Client::connect(&name, &me()).unwrap();
    c.conn
        .write_raw(&((MAX_FRAME + 1) as u32).to_le_bytes())
        .unwrap();
    assert!(c.recv().is_err(), "the server drops the connection");
}

#[test]
fn identity_is_the_callers_sid() {
    let name = pipe_name();
    let (tx, rx) = std::sync::mpsc::channel();
    let tx = std::sync::Mutex::new(tx);
    let sid = current_user_sid().unwrap();
    let _h = pipe::serve(
        &name,
        &user_sddl(&sid),
        Arc::new(move |c: Arc<pipe::PipeConn>| {
            let _ = c.read_frame(); // impersonation needs a message first
            let _ = tx.lock().unwrap().send(c.client_sid());
        }),
    )
    .unwrap();
    let c = connect_raw(&name).unwrap();
    c.send(&ClientMsg::Hello { version: 1 }).unwrap();
    let got = rx.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
    let whoami = std::process::Command::new("whoami")
        .args(["/user", "/fo", "csv", "/nh"])
        .output()
        .unwrap();
    let line = String::from_utf8_lossy(&whoami.stdout);
    let expect = line.trim().rsplit(',').next().unwrap().trim_matches('"');
    assert_eq!(got, expect);
    assert_eq!(got, sid);
}

#[test]
fn second_server_on_the_same_name_fails() {
    let (name, _s, _h) = start(small_index());
    let sid = current_user_sid().unwrap();
    assert!(
        serve_index(
            &name,
            &user_sddl(&sid),
            Arc::new(Shared::new(Index::new(), "user"))
        )
        .is_err()
    );
}

#[test]
fn user_mode_refuses_another_sid() {
    let name = pipe_name();
    let mut shared = Shared::new(small_index(), "user");
    shared.only_sid = Some("S-1-5-21-1-2-3-1001".into()); // not us
    let sid = current_user_sid().unwrap();
    let _h = serve_index(&name, &user_sddl(&sid), Arc::new(shared)).unwrap();
    let err = Client::connect(&name, &me()).err().expect("refused");
    assert!(err.to_string().starts_with("denied"), "{err}");
}

#[test]
fn stopping_the_server_ends_open_connections() {
    let (name, _s, h) = start(small_index());
    let c = Client::connect(&name, &me()).unwrap();
    let t = Instant::now();
    drop(h);
    assert!(t.elapsed() < Duration::from_secs(2));
    assert!(
        c.query("rapor", 5, vec![]).is_err(),
        "the connection is closed"
    );
    assert!(
        Client::connect(&name, &me()).is_err(),
        "no longer accepting"
    );
}

/// A process that holds the pipe name first must not
/// receive the app's queries. The client checks who owns the pipe.
#[test]
fn client_refuses_a_pipe_owned_by_another_account() {
    let (name, _s, _h) = start(small_index());
    let err = Client::connect(&name, pipe::SYSTEM_SID)
        .err()
        .expect("refused");
    assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied, "{err}");
    assert!(
        err.to_string()
            .contains("not served by the expected account"),
        "{err}"
    );
    assert!(
        Client::connect(&name, &me()).is_ok(),
        "the real owner is accepted"
    );
}

/// GENERIC_WRITE includes FILE_CREATE_PIPE_INSTANCE,
/// which let any interactive user attach a fake server instance to the
/// live service pipe.
#[test]
fn client_access_cannot_create_pipe_instances() {
    assert_eq!(pipe::CLIENT_ACCESS & pipe::FILE_CREATE_PIPE_INSTANCE, 0);
    assert!(pipe::service_sddl().contains(&format!("{:#010x}", pipe::CLIENT_ACCESS)));
    assert!(
        pipe::service_sddl().starts_with("O:SY"),
        "the service pipe is owned by SYSTEM"
    );
    // a pipe granting this user only CLIENT_ACCESS: the client can talk
    // to it, and a second server instance cannot be added to it
    let name = pipe_name();
    let sddl = format!(
        "O:{0}D:P(A;;GA;;;SY)(A;;{1:#010x};;;{0})",
        me(),
        pipe::CLIENT_ACCESS
    );
    let _h = serve_index(&name, &sddl, Arc::new(Shared::new(small_index(), "user"))).unwrap();
    let c = Client::connect(&name, &me()).unwrap();
    assert!(matches!(
        c.query("rapor", 5, vec![]).unwrap(),
        ServerMsg::Results { .. }
    ));
    let err = pipe::add_instance_for_test(&name, &sddl, false).expect_err("instance refused");
    assert_eq!(err.raw_os_error(), Some(5), "{err}");
}

/// The server refuses to start on a name another process already holds.
#[test]
fn squatted_name_fails_the_server_loudly() {
    let (name, _s, _h) = start(small_index());
    let err = serve_index(
        &name,
        &user_sddl(&me()),
        Arc::new(Shared::new(Index::new(), "user")),
    )
    .err()
    .expect("refused");
    assert!(
        err.to_string().contains("already held by another process"),
        "{err}"
    );
}

/// Each word costs a table over every folder.
#[test]
fn too_many_words_are_refused() {
    let (name, _s, _h) = start(small_index());
    let c = Client::connect(&name, &me()).unwrap();
    let many: String = (0..17).map(|i| format!("k{i} ")).collect();
    let err = c.query(&many, 5, vec![]).expect_err("refused");
    assert!(err.to_string().starts_with("query:"), "{err}");
    let ok: String = (0..16).map(|i| format!("k{i} ")).collect();
    assert!(matches!(
        c.query(&ok, 5, vec![]).unwrap(),
        ServerMsg::Results { .. }
    ));
}

/// Between one connection and the next instance the name
/// had no instance, so another process could create it first and lock
/// every client out. The next instance now exists before the current one
/// is handed off or dropped.
#[test]
fn the_name_is_never_free_between_connections() {
    let (name, _s, _h) = start(small_index());
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let churn = {
        let (name, done) = (name.clone(), done.clone());
        std::thread::spawn(move || {
            for _ in 0..3_000 {
                drop(connect_raw(&name));
            }
            done.store(true, Ordering::SeqCst);
        })
    };
    let mut squats = 0;
    let mut tries = 0;
    while !done.load(Ordering::SeqCst) {
        tries += 1;
        if pipe::add_instance_for_test(&name, &user_sddl(&me()), true).is_ok() {
            squats += 1;
        }
    }
    churn.join().unwrap();
    assert!(tries > 100, "the race was exercised ({tries} tries)");
    assert_eq!(
        squats, 0,
        "the name was free {squats} times in {tries} tries"
    );
    assert!(Client::connect(&name, &me()).is_ok(), "still serving");
}

/// A sandboxed (low integrity) process of the same user
/// passes the owner check; the client also checks the pipe's label.
#[test]
fn a_low_integrity_pipe_is_refused() {
    let name = pipe_name();
    let sddl = format!("O:{0}D:P(A;;GA;;;SY)(A;;GA;;;{0})S:(ML;;NW;;;LW)", me());
    let _h = serve_index(&name, &sddl, Arc::new(Shared::new(small_index(), "user"))).unwrap();
    let Err(err) = Client::connect(&name, &me()) else {
        panic!("connected: refused")
    };
    assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied, "{err}");
    assert!(err.to_string().contains("integrity"), "{err}");
}

/// One user opened 2,000 connections (4,029 threads). Each
/// account gets MAX_CONNECTIONS_PER_USER; a closed one frees its slot.
#[test]
fn connections_per_user_are_capped() {
    use flint_indexer::server::MAX_CONNECTIONS_PER_USER;
    let (name, _s, _h) = start(small_index());
    let mut open: Vec<Client> = (0..MAX_CONNECTIONS_PER_USER)
        .map(|_| Client::connect(&name, &me()).unwrap())
        .collect();
    let Err(err) = Client::connect(&name, &me()) else {
        panic!("connected: over the cap")
    };
    assert!(err.to_string().starts_with("busy:"), "{err}");
    drop(open.pop());
    let end = Instant::now() + Duration::from_secs(20);
    loop {
        match Client::connect(&name, &me()) {
            Ok(c) => {
                open.push(c);
                break;
            }
            Err(e) => {
                assert!(Instant::now() < end, "slot never freed: {e}");
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

/// Connections that never say Hello hold no slot for
/// long, and past MAX_CONNECTIONS the server answers "busy" at once.
#[test]
fn silent_connections_are_dropped_and_total_is_capped() {
    use flint_indexer::server::MAX_CONNECTIONS;
    let (name, _s, _h) = start(small_index());
    let silent: Vec<_> = (0..MAX_CONNECTIONS)
        .map(|_| connect_raw(&name).unwrap())
        .collect();
    let extra = connect_raw(&name).unwrap();
    match extra.recv::<ServerMsg>().unwrap() {
        ServerMsg::Error { code, .. } => assert_eq!(code, "busy"),
        m => panic!("{m:?}"),
    }
    // the silent ones are dropped after the Hello deadline
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let r = silent[0].recv::<ServerMsg>();
        let _ = tx.send(r.is_err());
    });
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(30)),
        Ok(true),
        "a silent client was kept"
    );
    let end = Instant::now() + Duration::from_secs(20);
    while Client::connect(&name, &me()).is_err() {
        assert!(Instant::now() < end, "slots never freed");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// A service without privacy rules (not loaded yet, or
/// the profile list could not be read) would show everything to
/// everyone. It refuses queries and status instead.
#[test]
fn service_refuses_until_privacy_rules_are_set() {
    let name = pipe_name();
    let shared = Arc::new(Shared::new(small_index(), "service"));
    let _h = serve_index(&name, &user_sddl(&me()), shared.clone()).unwrap();
    let c = Client::connect(&name, &me()).unwrap();
    let err = c.query("rapor", 5, vec![]).expect_err("refused");
    assert!(err.to_string().starts_with("starting:"), "{err}");
    let err = c.status().expect_err("refused");
    assert!(err.to_string().starts_with("starting:"), "{err}");
    shared
        .index
        .write()
        .unwrap()
        .set_owner_rules(&[flint_core::OwnerRule {
            path: vec!["*:".into()],
            children: vec![],
            default: OWNER_NONE,
        }]);
    assert!(matches!(
        c.query("rapor", 5, vec![]).unwrap(),
        ServerMsg::Results { .. }
    ));
}
