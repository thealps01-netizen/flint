//! A client that sends queries and never reads the
//! answers made the server queue every query (388 MiB measured). Only the
//! newest query is kept now, and a client past MAX_UNANSWERED is dropped.
//! Its own test binary: the connection counter is process-wide.
#![cfg(windows)]

use flint_core::{EntryInput, Index};
use flint_indexer::pipe::{Client, current_user_sid, serve_index, user_sddl};
use flint_indexer::server::{ACTIVE_CONNECTIONS, MAX_UNANSWERED, Shared};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

#[test]
fn a_client_that_never_reads_is_bounded_and_dropped() {
    let mut ix = Index::new();
    let c = ix.insert(EntryInput {
        name: "C:",
        parent: None,
        is_dir: true,
        last_change: 0,
    });
    let deep = (0..20).fold(c, |p, i| {
        ix.insert(EntryInput {
            name: &format!("uzun-bir-klasor-adi-{i}"),
            parent: Some(p),
            is_dir: true,
            last_change: 0,
        })
    });
    for i in 0..5_000u32 {
        ix.insert(EntryInput {
            name: &format!("rapor-{i}.txt"),
            parent: Some(deep),
            is_dir: false,
            last_change: i,
        });
    }
    let name = format!(r"\\.\pipe\flint-test-flood-{}", std::process::id());
    let sid = current_user_sid().unwrap();
    let _h = serve_index(&name, &user_sddl(&sid), Arc::new(Shared::new(ix, "user"))).unwrap();
    let client = Client::connect(&name, &sid).unwrap();
    // big answers (200 long paths each) fill the pipe, so the server's
    // worker blocks writing while we keep sending and never read
    let boosts: Vec<(String, f32)> = (0..2_000)
        .map(|i| (format!(r"C:\bir\uzun\yol\{i}.txt"), 0.5))
        .collect();
    let t = Instant::now();
    let mut sent = 0;
    let dropped = loop {
        if client.send_query("rapor", 200, boosts.clone()).is_err() {
            break true;
        }
        sent += 1;
        if sent > MAX_UNANSWERED * 4 || t.elapsed() > Duration::from_secs(60) {
            break false;
        }
    };
    assert!(
        dropped,
        "the server kept a client that read nothing after {sent} queries"
    );
    assert!(sent > MAX_UNANSWERED, "dropped too early ({sent})");
    drop(client);
    let end = Instant::now() + Duration::from_secs(10);
    while ACTIVE_CONNECTIONS.load(Ordering::SeqCst) > 0 {
        assert!(Instant::now() < end, "connection threads did not end");
        std::thread::sleep(Duration::from_millis(10));
    }
}
