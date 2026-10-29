//! A client that leaves while its query runs must not leak the server's
//! connection threads. Its own test binary, so no other test's
//! connections share the process-wide counter.
#![cfg(windows)]

use flint_core::{EntryInput, Index};
use flint_indexer::pipe::{Client, current_user_sid, serve_index, user_sddl};
use flint_indexer::server::{ACTIVE_CONNECTIONS, Shared};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

#[test]
fn client_disconnect_mid_query() {
    let mut ix = Index::new();
    let c = ix.insert(EntryInput {
        name: "C:",
        parent: None,
        is_dir: true,
        last_change: 0,
    });
    for i in 0..400_000u32 {
        let name = format!("dosya{}_{}rapor{}.txt", i % 97, i % 13, i);
        ix.insert(EntryInput {
            name: &name,
            parent: Some(c),
            is_dir: false,
            last_change: i,
        });
    }
    let name = format!(r"\\.\pipe\flint-test-drain-{}", std::process::id());
    let sid = current_user_sid().unwrap();
    let _h = serve_index(&name, &user_sddl(&sid), Arc::new(Shared::new(ix, "user"))).unwrap();
    for _ in 0..5 {
        let c = Client::connect(&name, &sid).unwrap();
        c.send_query("ra", 50, vec![]).unwrap();
        drop(c); // gone while the query may still run
    }
    let end = Instant::now() + Duration::from_secs(10);
    while ACTIVE_CONNECTIONS.load(Ordering::SeqCst) > 0 {
        assert!(
            Instant::now() < end,
            "connection threads did not end: {}",
            ACTIVE_CONNECTIONS.load(Ordering::SeqCst)
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}
