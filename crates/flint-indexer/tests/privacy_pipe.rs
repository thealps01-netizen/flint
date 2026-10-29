//! Privacy end to end: the viewer comes from the pipe client's token, so a
//! client sees its own profile and public folders, never another user's.
#![cfg(windows)]

use flint_core::{EntryInput, Index};
use flint_indexer::pipe::{Client, connect_raw, current_user_sid, serve_index, user_sddl};
use flint_indexer::privacy::{self, Machine, Profile};
use flint_indexer::protocol::{ClientMsg, PROTOCOL_VERSION, ServerMsg};
use flint_indexer::server::Shared;
use std::sync::Arc;

fn paths(m: ServerMsg) -> Vec<String> {
    let ServerMsg::Results { hits, total, .. } = m else {
        panic!("{m:?}")
    };
    assert_eq!(total as usize, hits.len(), "counts include no hidden entry");
    let mut v: Vec<String> = hits.into_iter().map(|h| h.path).collect();
    v.sort();
    v
}

#[test]
fn viewer_from_pipe_identity_hides_other_profile() {
    let me = current_user_sid().unwrap();
    let mut ix = Index::new();
    let c = ix.insert(EntryInput {
        name: "C:",
        parent: None,
        is_dir: true,
        last_change: 0,
    });
    let users = ix.insert(EntryInput {
        name: "Users",
        parent: Some(c),
        is_dir: true,
        last_change: 0,
    });
    let mine = ix.insert(EntryInput {
        name: "ben",
        parent: Some(users),
        is_dir: true,
        last_change: 0,
    });
    let other = ix.insert(EntryInput {
        name: "baskasi",
        parent: Some(users),
        is_dir: true,
        last_change: 0,
    });
    let shared = ix.insert(EntryInput {
        name: "Ortak",
        parent: Some(c),
        is_dir: true,
        last_change: 0,
    });
    for p in [mine, other, shared] {
        ix.insert(EntryInput {
            name: "rapor.docx",
            parent: Some(p),
            is_dir: false,
            last_change: 0,
        });
    }
    ix.insert(EntryInput {
        name: "maas-bordrosu.xlsx",
        parent: Some(other),
        is_dir: false,
        last_change: 0,
    });
    let profiles = vec![
        Profile {
            sid: "S-1-5-21-9-9-9-1001".into(),
            path: r"C:\Users\baskasi".into(),
        },
        Profile {
            sid: me.clone(),
            path: r"C:\Users\ben".into(),
        },
    ];
    let mut machine = Machine {
        profiles,
        profiles_dir: r"C:\Users".into(),
        profiles_dir_aliases: vec![],
        public_dir: Some(r"C:\Users\Public".into()),
        public_dir_aliases: vec![],
        windows_dir: r"C:\Windows".into(),
    };
    let mut s = Shared::new(ix, "service");
    privacy::apply(
        s.index.get_mut().unwrap(),
        &machine,
        s.owners.get_mut().unwrap(),
    );
    let s = Arc::new(s);
    let name = format!(r"\\.\pipe\flint-test-privacy-{}", std::process::id());
    let _h = serve_index(&name, &user_sddl(&me), s.clone()).unwrap();

    let c = Client::connect(&name, &me).unwrap();
    assert_eq!(
        paths(c.query("rapor", 50, vec![]).unwrap()),
        vec![r"C:\Ortak\rapor.docx", r"C:\Users\ben\rapor.docx"]
    );
    assert!(paths(c.query("maas", 50, vec![]).unwrap()).is_empty());
    assert!(paths(c.query("maas bordro", 50, vec![]).unwrap()).is_empty());
    assert!(
        paths(c.query("baskasi", 50, vec![]).unwrap()).is_empty(),
        "the profile folder itself"
    );
    assert!(
        paths(c.query("baskasi rapor", 50, vec![]).unwrap()).is_empty(),
        "folder-name term"
    );
    // a boost on the hidden file does not surface it
    let boost = vec![(r"C:\Users\baskasi\maas-bordrosu.xlsx".to_string(), 1.0)];
    assert!(paths(c.query("maas", 50, boost).unwrap()).is_empty());
    // a second Hello cannot change who we are
    c.conn
        .send(&ClientMsg::Hello {
            version: PROTOCOL_VERSION,
        })
        .unwrap();
    assert!(matches!(c.recv().unwrap(), ServerMsg::Error { .. }));
    assert!(paths(c.query("maas", 50, vec![]).unwrap()).is_empty());

    // the status count is what this viewer may see;
    // so is every per-drive count (the service fills them
    // with raw totals)
    s.volumes
        .lock()
        .unwrap()
        .push(flint_indexer::protocol::VolumeStatus {
            drive: "C:".into(),
            state: "live".into(),
            entries: 999,
        });
    let ServerMsg::StatusReply { entries, volumes } = c.status().unwrap() else {
        panic!()
    };
    assert_eq!(
        entries, 6,
        "C:, Users, ben + its file, Ortak + its file; not the other profile"
    );
    assert_eq!(volumes[0].entries, 6, "{volumes:?}");

    // the rules are rebuilt after the other account is
    // deleted; this open connection must still see only its own profile
    machine.profiles.remove(0);
    machine.profiles.push(Profile {
        sid: "S-1-5-21-9-9-9-1002".into(),
        path: r"C:\Users\yeni".into(),
    });
    {
        let mut ix = s.index.write().unwrap();
        let mut reg = s.owners.write().unwrap();
        privacy::apply(&mut ix, &machine, &mut reg);
    }
    assert_eq!(
        paths(c.query("rapor", 50, vec![]).unwrap()),
        vec![r"C:\Ortak\rapor.docx", r"C:\Users\ben\rapor.docx"]
    );
    assert!(
        paths(c.query("maas", 50, vec![]).unwrap()).is_empty(),
        "the deleted account's folder stays hidden"
    );

    // a raw client that never says Hello gets nothing
    let raw = connect_raw(&name).unwrap();
    raw.send(&ClientMsg::Query {
        seq: 1,
        text: "maas".into(),
        limit: 5,
        boosts: vec![],
    })
    .unwrap();
    assert!(matches!(
        raw.recv::<ServerMsg>().unwrap(),
        ServerMsg::Error { .. }
    ));
}
