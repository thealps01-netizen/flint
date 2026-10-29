//! Query server: one handler per connection over any framed transport
//! (`Conn`). A new query cancels the connection's running one; the viewer
//! (privacy filter) comes only from the caller's token, never from a
//! message.

use crate::privacy::OwnerRegistry;
use crate::protocol::{ClientMsg, MAX_LIMIT, PROTOCOL_VERSION, ServerMsg, VolumeStatus, WireHit};
use flint_core::{Boosts, Index, OWNER_NOBODY, Owner, SearchOptions, parse, search};
use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock};

/// A framed, duplex connection: reads and writes may run on different
/// threads at the same time; writes are serialized by the transport.
pub trait Conn: Send + Sync + 'static {
    fn read_frame(&self) -> io::Result<Vec<u8>>;
    fn write_frame(&self, payload: &[u8]) -> io::Result<()>;
    /// String SID of the connected client (from its token).
    fn client_sid(&self) -> io::Result<String>;
    /// Wakes a blocked read or write with an error (the connection is
    /// being dropped).
    fn close(&self);
    /// Reads give up with `TimedOut` after this many milliseconds
    /// (`None`: wait for ever).
    fn set_read_timeout(&self, ms: Option<u32>);
}

/// State every connection shares.
pub struct Shared {
    pub index: RwLock<Index>,
    /// "service" or "user".
    pub mode: String,
    pub volumes: Mutex<Vec<VolumeStatus>>,
    /// Client SID -> owner number (privacy filter), append-only. An
    /// unknown SID sees only public entries.
    pub owners: RwLock<OwnerRegistry>,
    /// When set, only this SID may connect (user mode: the owner only).
    pub only_sid: Option<String>,
    /// Refuse queries and status until the index has owner rules (the
    /// service: without rules every entry would be public to everyone).
    pub require_rules: bool,
    /// Open connections per client SID (capped).
    per_user: Mutex<HashMap<String, usize>>,
    /// Searches running now (capped; each holds the index read lock).
    searching: (Mutex<usize>, Condvar),
    /// Unix seconds "now" for recency.
    pub clock: fn() -> u32,
}

impl Shared {
    pub fn new(index: Index, mode: &str) -> Shared {
        Shared {
            index: RwLock::new(index),
            mode: mode.to_string(),
            volumes: Mutex::new(Vec::new()),
            owners: RwLock::new(OwnerRegistry::new()),
            only_sid: None,
            require_rules: mode == "service",
            per_user: Mutex::default(),
            searching: (Mutex::new(0), Condvar::new()),
            clock: unix_now,
        }
    }

    /// Why the index may not be shown yet, if so.
    fn not_ready(&self, ix: &Index) -> Option<ServerMsg> {
        (self.require_rules && !ix.has_owners()).then(|| {
            error(
                "starting",
                "the privacy rules are not loaded yet; try again shortly",
            )
        })
    }

    /// The owner a client views as, read fresh for every query; no SID
    /// or a poisoned lock fails closed (public entries only).
    pub fn viewer(&self, sid: Option<&str>) -> Owner {
        match sid {
            Some(s) => self.owners.read().map_or(OWNER_NOBODY, |r| r.viewer(s)),
            None => OWNER_NOBODY,
        }
    }
}

pub fn unix_now() -> u32 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs().min(u32::MAX as u64) as u32)
}

/// Connections currently inside `handle` (tests check it drains).
pub static ACTIVE_CONNECTIONS: AtomicUsize = AtomicUsize::new(0);

/// A client that sends this many queries without reading the answers is
/// disconnected (bounds the server's memory per connection).
pub const MAX_UNANSWERED: usize = 1024;

/// Most words a query may have; more is refused (each word costs a
/// table over every folder).
pub const MAX_QUERY_TERMS: usize = 16;

/// Open connections one account may have; more are answered `busy`.
pub const MAX_CONNECTIONS_PER_USER: usize = 8;

/// Open connections in all; more are answered `busy` before `Hello`.
pub const MAX_CONNECTIONS: usize = 64;

/// A client must send `Hello` within this many milliseconds.
pub const HELLO_DEADLINE_MS: u32 = 2_000;

/// Searches that may run at once (each holds the index read lock, which
/// the journal writer waits for).
pub const MAX_SEARCHES: usize = 4;

/// One connection's slot in [`Shared::per_user`].
struct UserSlot<'a>(&'a Shared, String);
impl Drop for UserSlot<'_> {
    fn drop(&mut self) {
        if let Ok(mut m) = self.0.per_user.lock()
            && let Some(n) = m.get_mut(&self.1)
        {
            *n -= 1;
            if *n == 0 {
                m.remove(&self.1);
            }
        }
    }
}

/// One running search in [`Shared::searching`].
struct SearchSlot<'a>(&'a Shared);
impl<'a> SearchSlot<'a> {
    fn take(s: &'a Shared) -> Option<SearchSlot<'a>> {
        let (m, cv) = &s.searching;
        let mut n = m.lock().ok()?;
        while *n >= MAX_SEARCHES {
            n = cv.wait(n).ok()?;
        }
        *n += 1;
        Some(SearchSlot(s))
    }
}
impl Drop for SearchSlot<'_> {
    fn drop(&mut self) {
        let (m, cv) = &self.0.searching;
        if let Ok(mut n) = m.lock() {
            *n -= 1;
        }
        cv.notify_one();
    }
}

struct Active;
impl Active {
    fn enter() -> Active {
        ACTIVE_CONNECTIONS.fetch_add(1, Ordering::SeqCst);
        Active
    }
}
impl Drop for Active {
    fn drop(&mut self) {
        ACTIVE_CONNECTIONS.fetch_sub(1, Ordering::SeqCst);
    }
}

fn send(conn: &dyn Conn, m: &ServerMsg) -> io::Result<()> {
    conn.write_frame(&serde_json::to_vec(m).map_err(io::Error::other)?)
}

fn error(code: &str, message: impl Into<String>) -> ServerMsg {
    ServerMsg::Error {
        code: code.into(),
        message: message.into(),
    }
}

struct Job {
    seq: u64,
    text: String,
    limit: u32,
    boosts: Vec<(String, f32)>,
    cancel: Arc<AtomicBool>,
}

/// Work waiting for the connection's worker: only the newest query is
/// kept; the ones it replaced are remembered by number for their
/// `Cancelled` answer.
#[derive(Default)]
struct Pending {
    job: Option<Job>,
    superseded: Vec<u64>,
    closed: bool,
}

#[derive(Default)]
struct Queue {
    m: Mutex<Pending>,
    cv: Condvar,
}

/// Serves one connection until the client leaves or breaks the protocol.
pub fn handle(conn: Arc<dyn Conn>, shared: Arc<Shared>) {
    let _active = Active::enter();
    conn.set_read_timeout(Some(HELLO_DEADLINE_MS));
    let Ok(first) = conn.read_frame() else { return };
    conn.set_read_timeout(None);
    match serde_json::from_slice::<ClientMsg>(&first) {
        Ok(ClientMsg::Hello { version }) if version == PROTOCOL_VERSION => {}
        Ok(ClientMsg::Hello { version }) => {
            let _ = send(
                &*conn,
                &error(
                    "version",
                    format!("the indexer speaks protocol {PROTOCOL_VERSION}, the app {version}"),
                ),
            );
            return;
        }
        _ => {
            let _ = send(
                &*conn,
                &error("protocol", "the first message must be Hello"),
            );
            return;
        }
    }
    let sid = match conn.client_sid() {
        Ok(s) => Some(s),
        Err(e) => {
            crate::log::line(format!(
                "client identity unavailable, serving public entries only: {e}"
            ));
            None
        }
    };
    if let Some(only) = &shared.only_sid
        && !sid.as_deref().is_some_and(|s| s.eq_ignore_ascii_case(only))
    {
        let _ = send(
            &*conn,
            &error("denied", "this indexer serves only the user who started it"),
        );
        return;
    }
    let key = sid.as_deref().unwrap_or("").to_ascii_uppercase();
    let _slot = {
        let Ok(mut m) = shared.per_user.lock() else {
            return;
        };
        let n = m.entry(key.clone()).or_default();
        if *n >= MAX_CONNECTIONS_PER_USER {
            drop(m);
            let _ = send(
                &*conn,
                &error(
                    "busy",
                    format!("at most {MAX_CONNECTIONS_PER_USER} connections per user"),
                ),
            );
            return;
        }
        *n += 1;
        UserSlot(&shared, key)
    };
    if send(
        &*conn,
        &ServerMsg::Welcome {
            version: PROTOCOL_VERSION,
            mode: shared.mode.clone(),
        },
    )
    .is_err()
    {
        return;
    }
    let queue = Arc::new(Queue::default());
    let worker = {
        let (conn, shared, queue, sid) = (conn.clone(), shared.clone(), queue.clone(), sid.clone());
        std::thread::spawn(move || run_jobs(&*conn, &shared, sid.as_deref(), &queue))
    };
    let mut current: Option<Arc<AtomicBool>> = None;
    while let Ok(frame) = conn.read_frame() {
        match serde_json::from_slice::<ClientMsg>(&frame) {
            Ok(ClientMsg::Query {
                seq,
                text,
                limit,
                boosts,
            }) => {
                if parse(&text).terms.len() > MAX_QUERY_TERMS {
                    let e = error(
                        "query",
                        format!("a query may have at most {MAX_QUERY_TERMS} words"),
                    );
                    if send(&*conn, &e).is_err() {
                        break;
                    }
                    continue;
                }
                if let Some(c) = current.take() {
                    c.store(true, Ordering::Relaxed);
                }
                let cancel = Arc::new(AtomicBool::new(false));
                current = Some(cancel.clone());
                let Ok(mut p) = queue.m.lock() else { break };
                if let Some(old) = p.job.take() {
                    p.superseded.push(old.seq);
                }
                if p.superseded.len() > MAX_UNANSWERED {
                    crate::log::line(
                        "a client sent too many queries without reading answers; disconnected",
                    );
                    break;
                }
                p.job = Some(Job {
                    seq,
                    text,
                    limit,
                    boosts,
                    cancel,
                });
                queue.cv.notify_one();
            }
            Ok(ClientMsg::Status) => {
                let viewer = shared.viewer(sid.as_deref());
                let mut volumes = shared.volumes.lock().map(|v| v.clone()).unwrap_or_default();
                let reply = match shared.index.read() {
                    Err(_) => error("internal", "the index is unavailable"),
                    Ok(ix) => match shared.not_ready(&ix) {
                        Some(e) => e,
                        None => {
                            // every count is what this viewer may see (a raw
                            // per-drive count minus the total would reveal
                            // other users' files)
                            let by = ix.visible_len_by_drive(viewer);
                            for v in &mut volumes {
                                let d = v
                                    .drive
                                    .bytes()
                                    .next()
                                    .map_or(b'?', |b| b.to_ascii_lowercase());
                                v.entries = by
                                    .iter()
                                    .find(|&&(x, _)| x == d)
                                    .map_or(0, |&(_, n)| n as u64);
                            }
                            let entries = by.iter().map(|&(_, n)| n as u64).sum();
                            ServerMsg::StatusReply { entries, volumes }
                        }
                    },
                };
                if send(&*conn, &reply).is_err() {
                    break;
                }
            }
            Ok(ClientMsg::Hello { .. }) => {
                // identity and version are fixed at connect; ignore a second Hello
                if send(&*conn, &error("protocol", "Hello was already sent")).is_err() {
                    break;
                }
            }
            Err(e) => {
                let _ = send(
                    &*conn,
                    &error("protocol", format!("unreadable message: {e}")),
                );
                break;
            }
        }
    }
    if let Some(c) = current {
        c.store(true, Ordering::Relaxed);
    }
    if let Ok(mut p) = queue.m.lock() {
        p.closed = true;
    }
    queue.cv.notify_one();
    conn.close(); // wakes the worker if it is blocked writing to a client that stopped reading
    let _ = worker.join();
}

/// Runs queries one at a time: the newest waiting query runs, the ones it
/// replaced are answered `Cancelled`. The viewer is read for every query,
/// so a rebuilt owner list applies at once.
/// One search as `viewer`: `Results`, `Cancelled`, or the "starting" error
/// while privacy rules are missing; `None` when the index lock is
/// poisoned. Shared by the pipe and in-process callers so both answer
/// identically. The index read lock is held only inside.
pub fn answer(
    shared: &Shared,
    viewer: Owner,
    seq: u64,
    text: &str,
    limit: u32,
    boosts: &Boosts,
    cancel: &AtomicBool,
) -> Option<ServerMsg> {
    let ix = shared.index.read().ok()?;
    if let Some(e) = shared.not_ready(&ix) {
        return Some(e);
    }
    let opt = SearchOptions {
        limit: limit.min(MAX_LIMIT) as usize,
        now: (shared.clock)(),
        boosts,
        cancel,
        viewer,
    };
    let r = search(&ix, &parse(text), &opt);
    Some(if r.cancelled {
        ServerMsg::Cancelled { seq }
    } else {
        ServerMsg::Results {
            seq,
            total: r.total_matches,
            elapsed_us: r.elapsed_us,
            hits: r
                .hits
                .into_iter()
                .map(|h| WireHit {
                    path: ix.path(h.id),
                    name: ix.name(h.id).to_string(),
                    is_dir: ix.is_dir(h.id),
                    typo: h.typo,
                    highlights: h.highlights,
                    last_change: ix.last_change(h.id),
                    score: h.score,
                })
                .collect(),
        }
    })
}

fn run_jobs(conn: &dyn Conn, shared: &Shared, sid: Option<&str>, queue: &Queue) {
    let mut boosts_src: Vec<(String, f32)> = Vec::new();
    let mut boosts = Boosts::new();
    loop {
        let (superseded, job) = {
            let Ok(mut p) = queue.m.lock() else { return };
            while p.job.is_none() && p.superseded.is_empty() && !p.closed {
                p = match queue.cv.wait(p) {
                    Ok(p) => p,
                    Err(_) => return,
                };
            }
            if p.closed {
                return;
            }
            (std::mem::take(&mut p.superseded), p.job.take())
        };
        for seq in superseded {
            if send(conn, &ServerMsg::Cancelled { seq }).is_err() {
                return;
            }
        }
        let Some(mut job) = job else { continue };
        if job.boosts != boosts_src {
            boosts = Boosts::new();
            for (p, w) in &job.boosts {
                boosts.add(p, *w);
            }
            boosts_src = std::mem::take(&mut job.boosts);
        }
        let viewer = shared.viewer(sid);
        let Some(running) = SearchSlot::take(shared) else {
            return;
        };
        let Some(msg) = answer(
            shared,
            viewer,
            job.seq,
            &job.text,
            job.limit,
            &boosts,
            &job.cancel,
        ) else {
            return;
        };
        // the index lock is released inside `answer`, before writing
        drop(running); // and the search slot: a client that stops reading holds neither
        if send(conn, &msg).is_err() {
            return;
        }
    }
}
