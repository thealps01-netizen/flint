//! The indexing engine behind the Windows service, `serve` and
//! `serve --user-mode`: fills the index, keeps it live,
//! saves it for warm starts and serves it on the pipe.
//!
//! User mode (no administrator rights): walks the given folders, follows
//! them with directory watches, and serves only the current user.
//! Service mode (LocalSystem): reads every fixed NTFS volume's MFT, follows
//! each change journal, applies the privacy rules and folder restrictions
//! before anything is served.

use crate::access::{self, Verdict};
use crate::pipe::{ServerHandle, current_user_sid, serve_index, service_sddl, user_sddl};
use crate::privacy::{self, OwnerRegistry};
use crate::protocol::{PIPE_NAME, ServerMsg, VolumeStatus};
use crate::server::Shared;
use crate::snapshot::{self, SnapshotError, VolumeSnapshot};
use crate::volindex::VolumeIndex;
use crate::volume::{self, JournalInfo, Replay, Volume};
use crate::walk::{self, FsEvent, PathMap};
use crate::{log, watch};
use flint_core::Index;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// The service's saved index (see [`snapshot::encode_image`]).
pub const IMAGE_FILE: &str = "index.img";

/// How often a changed index is saved (plan: every 10 min).
pub const SAVE_EVERY: Duration = Duration::from_secs(600);

#[derive(Clone, Debug)]
pub enum Mode {
    /// Walk and watch these folders; serve the current user only.
    User { roots: Vec<PathBuf>, sid: String },
    /// Every fixed NTFS volume, every account.
    Service,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub mode: Mode,
    /// Snapshots go here (the service's must be secured first: see
    /// [`service_data_dir`]).
    pub data_dir: PathBuf,
    pub pipe_name: String,
    pub sddl: String,
    pub save_every: Duration,
}

impl Config {
    /// A user-mode engine for the current user.
    pub fn user(roots: Vec<PathBuf>, data_dir: PathBuf, pipe_name: String) -> io::Result<Config> {
        let sid = current_user_sid()?;
        Ok(Config {
            sddl: user_sddl(&sid),
            mode: Mode::User { roots, sid },
            data_dir,
            pipe_name,
            save_every: SAVE_EVERY,
        })
    }

    /// The service engine on the well-known pipe.
    pub fn service(data_dir: PathBuf) -> Config {
        Config {
            mode: Mode::Service,
            data_dir,
            pipe_name: PIPE_NAME.to_string(),
            sddl: service_sddl(),
            save_every: SAVE_EVERY,
        }
    }

    fn snapshot_path(&self) -> PathBuf {
        self.data_dir.join(match self.mode {
            Mode::User { .. } => "index-user.bin",
            Mode::Service => IMAGE_FILE,
        })
    }
}

/// `%ProgramData%\Flint`, created (or taken back) with an ACL for SYSTEM
/// and Administrators only: the snapshots hold every account's file
/// names.
pub fn service_data_dir() -> io::Result<PathBuf> {
    let base = crate::folders::program_data()?;
    let dir = base.join("Flint");
    privacy::os::secure_dir(
        &dir,
        "O:SYD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)",
        &["S-1-5-18", "S-1-5-32-544"],
    )?;
    Ok(dir)
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum StartKind {
    /// Nothing usable was saved: indexed from scratch.
    Cold,
    /// Served from the saved snapshot while it is brought up to date.
    Warm,
}

/// Set once every volume or folder is indexed and followed.
#[derive(Default)]
struct Live {
    done: Mutex<bool>,
    cv: Condvar,
}

impl Live {
    fn set(&self) {
        if let Ok(mut d) = self.done.lock() {
            *d = true;
            self.cv.notify_all();
        }
    }
}

/// A running engine; [`Engine::stop`] (or drop) ends it and saves.
pub struct Engine {
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
    live: Arc<Live>,
    kind: StartKind,
    loaded: usize,
    /// The user a user-mode engine serves (in-process searches view as
    /// this account); `None` for the service.
    sid: Option<String>,
    server: Option<ServerHandle>,
    workers: Vec<JoinHandle<()>>,
    /// Threads that may block in a journal read (woken on stop).
    blocking: Arc<Mutex<Vec<JoinHandle<()>>>>,
    /// Runs once every thread has ended (the service's last save).
    final_save: Option<Box<dyn FnOnce() + Send + Sync>>,
}

impl Engine {
    pub fn start(cfg: Config) -> io::Result<Engine> {
        match cfg.mode.clone() {
            Mode::User { roots, sid } => start_user(cfg, roots, sid),
            Mode::Service => start_service(cfg),
        }
    }

    pub fn shared(&self) -> &Arc<Shared> {
        &self.shared
    }

    pub fn start_kind(&self) -> StartKind {
        self.kind
    }

    /// Entries restored from the snapshot (0 on a cold start).
    pub fn loaded_from_snapshot(&self) -> usize {
        self.loaded
    }

    /// Searches in this process as the engine's own user, with the same
    /// answer the pipe would give (see [`crate::server::answer`]). The
    /// service engine answers with public entries only.
    pub fn search(&self, text: &str, limit: u32, boosts: &[(String, f32)]) -> ServerMsg {
        let mut b = flint_core::Boosts::new();
        for (p, w) in boosts {
            b.add(p, *w);
        }
        let viewer = self.shared.viewer(self.sid.as_deref());
        let cancel = AtomicBool::new(false);
        crate::server::answer(&self.shared, viewer, 0, text, limit, &b, &cancel).unwrap_or_else(
            || ServerMsg::Error {
                code: "index".into(),
                message: "the index is not readable (a writer failed); restart Flint".into(),
            },
        )
    }

    /// Waits until everything is indexed and followed; `false` on timeout.
    pub fn wait_live(&self, timeout: Duration) -> bool {
        let Ok(g) = self.live.done.lock() else {
            return false;
        };
        self.live
            .cv
            .wait_timeout_while(g, timeout, |d| !*d)
            .map(|(d, _)| *d)
            .unwrap_or(false)
    }

    /// Stops every thread (within seconds, also mid-scan), saves a
    /// complete index, and stops serving.
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // journal reads block in the kernel: cancel until they return
        let blocking: Vec<JoinHandle<()>> = self
            .blocking
            .lock()
            .map(|mut b| b.drain(..).collect())
            .unwrap_or_default();
        for t in blocking {
            let end = Instant::now() + Duration::from_secs(5);
            while !t.is_finished() && Instant::now() < end {
                volume::cancel_blocking_io(&t);
                std::thread::sleep(Duration::from_millis(20));
            }
            if t.is_finished() {
                let _ = t.join();
            } else {
                log::line("a journal thread did not stop within 5 s");
            }
        }
        for t in self.workers.drain(..) {
            let _ = t.join();
        }
        if let Some(f) = self.final_save.take() {
            f();
        }
        self.server.take();
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Reads the snapshot file: `None` when there is none or it is unusable
/// (logged; the caller indexes from scratch).
fn load_snapshot(cfg: &Config) -> Option<Vec<VolumeSnapshot>> {
    let p = cfg.snapshot_path();
    match snapshot::load(&p) {
        Ok(s) => Some(s),
        Err(SnapshotError::Io(e)) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => {
            log::line(format!("snapshot {} discarded: {e:?}", p.display()));
            None
        }
    }
}

fn save(path: &std::path::Path, snaps: &[VolumeSnapshot]) {
    match snapshot::save_atomic(path, &snapshot::encode(snaps)) {
        Ok(()) => log::line(format!("snapshot saved: {}", path.display())),
        Err(e) => log::line(format!("snapshot not saved ({}): {e}", path.display())),
    }
}

// ---------------------------------------------------------------- user mode

fn start_user(cfg: Config, roots: Vec<PathBuf>, sid: String) -> io::Result<Engine> {
    std::fs::create_dir_all(&cfg.data_dir)?;
    let mut ix = Index::new();
    let mut kind = StartKind::Cold;
    if let Some(snaps) = load_snapshot(&cfg) {
        for s in &snaps {
            snapshot::restore(&mut ix, s);
        }
        kind = StartKind::Warm;
        log::line(format!(
            "warm start: {} entries from the snapshot",
            ix.live_len()
        ));
    }
    let loaded = if kind == StartKind::Warm {
        ix.live_len()
    } else {
        0
    };
    let mut shared = Shared::new(ix, "user");
    shared.only_sid = Some(sid.clone());
    let shared = Arc::new(shared);
    set_volumes(&shared, &roots, "scanning");
    let server = serve_index(&cfg.pipe_name, &cfg.sddl, shared.clone())?;
    let stop = Arc::new(AtomicBool::new(false));
    let live = Arc::new(Live::default());
    let worker = {
        let (shared, stop, live) = (shared.clone(), stop.clone(), live.clone());
        std::thread::Builder::new()
            .name("flint-user".into())
            .spawn(move || run_user(&cfg, &roots, &shared, &stop, &live))?
    };
    Ok(Engine {
        shared,
        stop,
        live,
        kind,
        loaded,
        sid: Some(sid),
        server: Some(server),
        workers: vec![worker],
        blocking: Arc::default(),
        final_save: None,
    })
}

fn set_volumes(shared: &Shared, roots: &[PathBuf], state: &str) {
    let mut drives: Vec<String> = roots
        .iter()
        .filter_map(|r| r.to_str().and_then(|s| s.get(..2)).map(str::to_uppercase))
        .collect();
    drives.dedup();
    if let Ok(mut v) = shared.volumes.lock() {
        *v = drives
            .into_iter()
            .map(|drive| VolumeStatus {
                drive,
                state: state.into(),
                entries: 0,
            })
            .collect();
    }
}

/// A complete walk of `roots` into a new index; `None` when stopped.
fn fresh_walk(roots: &[PathBuf], stop: &AtomicBool) -> Option<(Index, PathMap)> {
    let mut ix = Index::new();
    let mut map = PathMap::new();
    let t = Instant::now();
    for r in roots {
        let s = walk::walk_until(&mut ix, &mut map, r, stop, |_| {});
        if s.stopped {
            return None;
        }
        if s.unreadable_dirs > 0 {
            log::line(format!(
                "{}: {} folders could not be read",
                r.display(),
                s.unreadable_dirs
            ));
        }
    }
    log::line(format!(
        "walked {} entries in {:.2} s",
        ix.live_len(),
        t.elapsed().as_secs_f64()
    ));
    Some((ix, map))
}

fn run_user(cfg: &Config, roots: &[PathBuf], shared: &Shared, stop: &AtomicBool, live: &Live) {
    // watch first: no change made during the walk is missed
    let (tx, rx) = channel();
    let mut watches = Vec::new();
    for r in roots {
        match watch::watch(r, tx.clone()) {
            Ok(w) => watches.push(w),
            Err(e) => log::line(format!("cannot watch {}: {e}", r.display())),
        }
    }
    drop(tx);
    let Some(mut map) = publish_walk(roots, shared, stop) else {
        return;
    };
    set_volumes(shared, roots, "basic");
    live.set();
    drain(&rx, shared, &mut map, roots, stop, Duration::ZERO);
    // saved at once: a process that is killed later (a crash, a forced
    // quit) still starts warm next time instead of walking from scratch
    save_user(cfg, shared);
    let mut dirty = false;
    let mut last_save = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        dirty |= drain(
            &rx,
            shared,
            &mut map,
            roots,
            stop,
            Duration::from_millis(100),
        );
        if dirty && last_save.elapsed() >= cfg.save_every {
            compact_user(shared, &mut map);
            save_user(cfg, shared);
            dirty = false;
            last_save = Instant::now();
        }
    }
    drop(watches);
    save_user(cfg, shared);
}

/// Walks into a new index and serves it; the new path map, or `None`
/// when stopped (the old index stays served).
fn publish_walk(roots: &[PathBuf], shared: &Shared, stop: &AtomicBool) -> Option<PathMap> {
    let (ix, map) = fresh_walk(roots, stop)?;
    match shared.index.write() {
        Ok(mut g) => *g = ix,
        Err(_) => {
            log::line("index lock poisoned: the walk is not published");
            return None;
        }
    }
    Some(map)
}

/// Applies the watch events that arrive within `wait` (and all queued
/// ones); `true` when the index changed.
fn drain(
    rx: &Receiver<FsEvent>,
    shared: &Shared,
    map: &mut PathMap,
    roots: &[PathBuf],
    stop: &AtomicBool,
    wait: Duration,
) -> bool {
    let mut batch = Vec::new();
    match rx.recv_timeout(wait) {
        Ok(e) => batch.push(e),
        Err(RecvTimeoutError::Timeout) => {}
        // every watch ended: nothing more will come (logged where it ended)
        Err(RecvTimeoutError::Disconnected) => std::thread::sleep(wait),
    }
    batch.extend(rx.try_iter());
    if batch.is_empty() {
        return false;
    }
    let mut rewalk = false;
    if let Ok(mut ix) = shared.index.write() {
        for e in &batch {
            rewalk |= walk::apply_fs_event(&mut ix, map, e);
        }
    }
    if rewalk {
        log::line("watch buffer overflowed: walking again");
        if let Some(m) = publish_walk(roots, shared, stop) {
            *map = m;
        }
    }
    true
}

/// Rebuilds the index without the names renames and removals left
/// behind, once they are a large share (a long-running indexer would grow
/// without bound otherwise).
fn compact_user(shared: &Shared, map: &mut PathMap) {
    let snaps = match shared.index.read() {
        Ok(ix) if ix.garbage_ratio() > COMPACT_GARBAGE => snapshot::capture_walk(&ix),
        _ => return,
    };
    let mut fresh = Index::new();
    for s in snaps {
        snapshot::restore_owned(&mut fresh, s);
    }
    let mut m = PathMap::new();
    for id in (0..fresh.raw_len() as u32).map(flint_core::EntryId) {
        if fresh.is_live(id) {
            m.bind(&fresh, id);
        }
    }
    if let Ok(mut w) = shared.index.write() {
        log::line(format!(
            "compacted the index: {} -> {} entries",
            w.raw_len(),
            fresh.raw_len()
        ));
        *w = fresh;
        *map = m;
    }
}

fn save_user(cfg: &Config, shared: &Shared) {
    let snaps = match shared.index.read() {
        Ok(ix) => snapshot::capture_walk(&ix),
        Err(_) => return,
    };
    save(&cfg.snapshot_path(), &snaps);
}

// ------------------------------------------------------------- service mode

/// One NTFS volume followed by its journal thread.
struct VolState {
    drive: String,
    serial: u32,
    journal: JournalInfo,
    /// Journal position applied so far.
    next_usn: i64,
    vi: Option<VolumeIndex>,
    /// Caught up with the journal and following it.
    live: bool,
}

fn start_service(cfg: Config) -> io::Result<Engine> {
    let started = Instant::now();
    if let Err(e) = volume::enable_privilege("SeBackupPrivilege") {
        log::line(format!(
            "SeBackupPrivilege not enabled ({e}); some folders may not be checked"
        ));
    }
    // the saved image, if any; its owner rules are set below
    let image = cfg.data_dir.join(IMAGE_FILE);
    let (mut ix, mut saved) = match std::fs::read(&image) {
        Ok(b) => match snapshot::decode_image(&b) {
            Ok(x) => x,
            Err(e) => {
                log::line(format!("image {} discarded: {e}", image.display()));
                (Index::new(), Vec::new())
            }
        },
        Err(e) if e.kind() == io::ErrorKind::NotFound => (Index::new(), Vec::new()),
        Err(e) => {
            log::line(format!("image {} not read: {e}", image.display()));
            (Index::new(), Vec::new())
        }
    };
    let t_load = started.elapsed();
    let mut reg = OwnerRegistry::new();
    // rules before anything is served: without them the
    // server keeps answering "starting"
    let machine = match privacy::os::machine_resolved() {
        Ok(m) => {
            privacy::apply(&mut ix, &m, &mut reg);
            Some(m)
        }
        Err(e) => {
            log::line(format!(
                "cannot read the profile list ({e}): serving nothing"
            ));
            None
        }
    };
    let t_rules = started.elapsed();
    let mut vols = Vec::new();
    let mut kind = StartKind::Cold;
    for drive in volume::fixed_ntfs_drives() {
        let v = match Volume::open(&drive) {
            Ok(v) => v,
            Err(e) => {
                log::line(format!("{drive}: cannot open ({e}); not indexed"));
                continue;
            }
        };
        let journal = match v.query_journal() {
            Ok((j, created)) => {
                if created {
                    log::line(format!("{drive}: created a change journal"));
                }
                j
            }
            Err(e) => {
                log::line(format!("{drive}: no change journal ({e}); not indexed"));
                continue;
            }
        };
        let mut st = VolState {
            drive: drive.clone(),
            serial: v.serial,
            journal,
            next_usn: 0,
            vi: None,
            live: false,
        };
        if let Some(at) = saved
            .iter()
            .position(|s| s.drive == drive && s.volume_serial == v.serial)
        {
            let s = saved.swap_remove(at);
            let root = s.parts.root;
            match volume::replay_from(s.journal_id, s.next_usn, &journal) {
                Replay::From(usn) => match VolumeIndex::from_parts(&ix, s.parts) {
                    Some(vi) => {
                        st.vi = Some(vi);
                        st.next_usn = usn;
                        kind = StartKind::Warm;
                    }
                    None => {
                        log::line(format!(
                            "{drive}: the saved volume does not fit its index; scanning again"
                        ));
                        drop_tree(&mut ix, root);
                    }
                },
                Replay::Rescan(why) => {
                    log::line(format!("{drive}: {why}; scanning again"));
                    drop_tree(&mut ix, root);
                }
            }
        }
        vols.push((v, st));
    }
    // saved volumes no longer present (removed drive, new serial)
    for s in saved {
        log::line(format!(
            "{}: in the image but not mounted now; dropped",
            s.drive
        ));
        drop_tree(&mut ix, s.parts.root);
    }
    let loaded = ix.live_len();
    if kind == StartKind::Warm {
        log::line(format!(
            "warm start: {loaded} entries from the image in {:.2} s \
             (image read {:.2} s, rules {:.2} s, volumes {:.2} s)",
            started.elapsed().as_secs_f64(),
            t_load.as_secs_f64(),
            (t_rules - t_load).as_secs_f64(),
            (started.elapsed() - t_rules).as_secs_f64()
        ));
    }
    let shared = Arc::new(Shared::new(ix, "service"));
    if let Ok(mut o) = shared.owners.write() {
        *o = reg;
    }
    publish_status(&shared, &vols.iter().map(|(_, s)| s).collect::<Vec<_>>());
    let server = serve_index(&cfg.pipe_name, &cfg.sddl, shared.clone())?;
    let stop = Arc::new(AtomicBool::new(false));
    let live = Arc::new(Live::default());
    let blocking: Arc<Mutex<Vec<JoinHandle<()>>>> = Arc::default();
    let (volumes, states): (Vec<Volume>, Vec<Arc<Mutex<VolState>>>) = vols
        .into_iter()
        .map(|(v, s)| (v, Arc::new(Mutex::new(s))))
        .unzip();
    let pending = Arc::new(AtomicUsize::new(volumes.len()));
    if volumes.is_empty() {
        log::line("no fixed NTFS volume could be opened: nothing to index");
        live.set();
    }
    let dirty = Arc::new(AtomicBool::new(false));
    for (v, slot) in volumes.into_iter().zip(&states) {
        let ctx = VolCtx {
            shared: shared.clone(),
            stop: stop.clone(),
            live: live.clone(),
            pending: pending.clone(),
            dirty: dirty.clone(),
            machine: machine.clone(),
            state: slot.clone(),
            all: states.clone(),
        };
        let t = std::thread::Builder::new()
            .name(format!("flint-journal-{}", v.drive))
            .spawn(move || run_volume(v, ctx))?;
        if let Ok(mut b) = blocking.lock() {
            b.push(t);
        }
    }
    let path = cfg.snapshot_path();
    let saver = {
        let (stop, dirty, shared, states, path, machine) = (
            stop.clone(),
            dirty.clone(),
            shared.clone(),
            states.clone(),
            path.clone(),
            machine.clone(),
        );
        let every = cfg.save_every;
        std::thread::Builder::new()
            .name("flint-save".into())
            .spawn(move || {
                let mut last = Instant::now();
                while !stop.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(100));
                    if last.elapsed() >= every && dirty.swap(false, Ordering::SeqCst) {
                        save_service(&path, &shared, &states, machine.as_ref());
                        last = Instant::now();
                    }
                }
            })?
    };
    let final_save: Box<dyn FnOnce() + Send + Sync> = {
        let shared = shared.clone();
        Box::new(move || {
            if let Some(w) = writer_wait_summary() {
                log::line(w);
            }
            if dirty.load(Ordering::SeqCst) {
                save_service(&path, &shared, &states, machine.as_ref());
            }
        })
    };
    Ok(Engine {
        shared,
        stop,
        live,
        kind,
        loaded,
        sid: None,
        server: Some(server),
        workers: vec![saver],
        blocking,
        final_save: Some(final_save),
    })
}

struct VolCtx {
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
    live: Arc<Live>,
    pending: Arc<AtomicUsize>,
    dirty: Arc<AtomicBool>,
    machine: Option<privacy::Machine>,
    state: Arc<Mutex<VolState>>,
    all: Vec<Arc<Mutex<VolState>>>,
}

fn publish_status(shared: &Shared, vols: &[&VolState]) {
    if let Ok(mut v) = shared.volumes.lock() {
        *v = vols
            .iter()
            .map(|s| VolumeStatus {
                drive: s.drive.clone(),
                state: if s.live { "live" } else { "scanning" }.into(),
                entries: s.vi.as_ref().map_or(0, |v| v.len() as u64),
            })
            .collect();
    }
}

fn refresh_status(ctx: &VolCtx) {
    let guards: Vec<_> = ctx.all.iter().filter_map(|s| s.lock().ok()).collect();
    let refs: Vec<&VolState> = guards.iter().map(|g| &**g).collect();
    publish_status(&ctx.shared, &refs);
}

/// Checks one folder by FRN for the access rules.
fn check_frn(
    v: &Volume,
    checker: &mut privacy::os::AccessChecker,
    frn: u64,
) -> io::Result<Verdict> {
    let h = v.open_by_id(frn)?;
    checker.check_handle(&h)
}

/// Verdicts for `frns`, in order: one open per distinct security id
/// (folders without one are opened each), spread over a few threads.
/// Returns the verdicts and the number of folders opened.
fn check_all(
    v: &Volume,
    stop: &AtomicBool,
    frns: &[u64],
    sids: &std::collections::HashMap<u64, u32>,
) -> (Vec<io::Result<Verdict>>, usize) {
    use std::collections::HashMap;
    use std::collections::hash_map::Entry;
    // one representative per security id; the others copy its verdict
    let mut first_of: HashMap<u32, usize> = HashMap::new();
    let mut to_open: Vec<usize> = Vec::new();
    for (i, f) in frns.iter().enumerate() {
        match sids.get(f) {
            Some(&sid) => {
                if let Entry::Vacant(e) = first_of.entry(sid) {
                    e.insert(i);
                    to_open.push(i);
                }
            }
            None => to_open.push(i),
        }
    }
    let threads = std::thread::available_parallelism()
        .map_or(4, |n| n.get())
        .clamp(1, 8);
    let chunk = to_open.len().div_ceil(threads).max(1);
    let opened: Vec<(usize, io::Result<Verdict>)> = std::thread::scope(|s| {
        let workers: Vec<_> = to_open
            .chunks(chunk)
            .map(|part| {
                s.spawn(move || {
                    let mut checker = privacy::os::AccessChecker::new();
                    part.iter()
                        .map(|&i| {
                            let r = if stop.load(Ordering::Relaxed) {
                                Err(io::Error::new(io::ErrorKind::Interrupted, "stopped"))
                            } else {
                                match &mut checker {
                                    Ok(c) => check_frn(v, c, frns[i]),
                                    Err(e) => Err(io::Error::new(e.kind(), e.to_string())),
                                }
                            };
                            (i, r)
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        workers
            .into_iter()
            .flat_map(|w| w.join().unwrap_or_default())
            .collect()
    });
    let n = opened.len();
    let mut by_index: Vec<Option<io::Result<Verdict>>> = (0..frns.len()).map(|_| None).collect();
    for (i, r) in opened {
        by_index[i] = Some(r);
    }
    let copy = |r: &io::Result<Verdict>| match r {
        Ok(v) => Ok(v.clone()),
        Err(e) => Err(io::Error::new(e.kind(), e.to_string())),
    };
    let mut out = Vec::with_capacity(frns.len());
    for i in 0..frns.len() {
        let rep = sids.get(&frns[i]).and_then(|s| first_of.get(s)).copied();
        let r = match (&by_index[i], rep) {
            (Some(r), _) => copy(r),
            (None, Some(j)) => match &by_index[j] {
                Some(r) => copy(r),
                None => Err(io::Error::other("no verdict")),
            },
            // fail closed (a worker thread panicked)
            (None, None) => Err(io::Error::other("no verdict")),
        };
        out.push(r);
    }
    (out, n)
}

/// A volume read from scratch: built in a private index, given the owner
/// rules and the folder restrictions, then copied into the served index,
/// so nothing in it is shown before its restrictions are set.
fn scan_volume(v: &Volume, ctx: &VolCtx, journal: JournalInfo) -> bool {
    let high_usn = journal.next_usn;
    let drive = v.drive.clone();
    let t = Instant::now();
    let root = match volume::root_frn(&drive) {
        Ok(r) => r,
        Err(e) => {
            log::line(format!("{drive}: cannot read the root ({e})"));
            return false;
        }
    };
    let mut records = Vec::new();
    if let Err(e) = v.enum_mft_until(high_usn, &ctx.stop, |r| records.push(r)) {
        if !ctx.stop.load(Ordering::Relaxed) {
            log::line(format!("{drive}: MFT enumeration failed ({e})"));
        }
        return false;
    }
    let read = t.elapsed();
    // folders with the same security id share one descriptor: one check
    // per id (budget +20 % of the first index)
    let sids: std::collections::HashMap<u64, u32> = records
        .iter()
        .filter(|r| r.is_dir() && r.security_id != 0)
        .map(|r| (r.frn, r.security_id))
        .collect();
    let mut ix = Index::new();
    if let Some(m) = &ctx.machine {
        privacy::apply(&mut ix, m, &mut OwnerRegistry::new());
    }
    let vi = VolumeIndex::build(&mut ix, &drive, root, records);
    let built = t.elapsed();
    let mut opened = 0usize;
    let stats = access::first_pass_batch(&mut ix, &vi, |frns| {
        let (verdicts, n) = check_all(v, &ctx.stop, frns, &sids);
        opened = n;
        verdicts
    });
    if ctx.stop.load(Ordering::Relaxed) {
        return false;
    }
    let checked = t.elapsed();
    log::line(format!(
        "{drive}: {} entries; MFT read {:.2} s, build {:.2} s, access checks {:.2} s \
         ({} folders checked, {} opened, {} with a security id, {} restricted, {} failed)",
        ix.live_len(),
        read.as_secs_f64(),
        (built - read).as_secs_f64(),
        (checked - built).as_secs_f64(),
        stats.checked,
        opened,
        sids.len(),
        stats.restricted,
        stats.failed
    ));

    let snap = snapshot::capture(&vi, &ix, &drive, v.serial, 0, high_usn);
    drop((ix, vi));
    // the volume state is locked first (lock order: state, then index), so
    // a compaction can never swap the index between the copy and the moment
    // the volume's FRN map is published: the map always matches the index
    let (Ok(mut s), Ok(mut g)) = (ctx.state.lock(), ctx.shared.index.write()) else {
        return false;
    };
    s.vi = Some(snapshot::restore_owned(&mut g, snap));
    s.journal = journal;
    s.next_usn = journal.next_usn;
    true
}

/// Removes `root` and everything below it (a saved volume that cannot be
/// brought up to date, without its FRN map).
fn drop_tree(ix: &mut Index, root: flint_core::EntryId) {
    if root.0 as usize >= ix.raw_len() {
        return;
    }
    let mut ids: Vec<(usize, flint_core::EntryId)> = (0..ix.raw_len() as u32)
        .map(flint_core::EntryId)
        .filter(|&id| ix.is_live(id) && (id == root || ix.ancestors(id).any(|a| a == root)))
        .map(|id| (ix.ancestors(id).count(), id))
        .collect();
    ids.sort_unstable_by_key(|x| std::cmp::Reverse(x.0));
    for (_, id) in ids {
        ix.remove(id);
    }
}

/// Removes every entry of a volume from the served index (journal lost).
fn drop_volume(ix: &mut Index, vi: &VolumeIndex) {
    let mut ids: Vec<(usize, flint_core::EntryId)> = vi
        .entries(ix)
        .map(|(_, _, id)| (ix.ancestors(id).count(), id))
        .collect();
    ids.push((0, vi.root()));
    ids.sort_unstable_by_key(|x| std::cmp::Reverse(x.0));
    for (_, id) in ids {
        if ix.is_live(id) {
            ix.remove(id);
        }
    }
}

/// `ERROR_JOURNAL_ENTRY_DELETED`, `ERROR_JOURNAL_DELETE_IN_PROGRESS`,
/// `ERROR_JOURNAL_NOT_ACTIVE`: the journal cannot continue from here.
const JOURNAL_LOST: [i32; 3] = [1181, 1178, 1179];
/// How long a journal thread gathers records after the first one of a
/// burst before it applies them.
const DEBOUNCE: Duration = Duration::from_millis(30);
/// Records applied as one batch at most.
const MAX_BATCH: usize = 100_000;
/// `ERROR_OPERATION_ABORTED`: woken by [`volume::cancel_blocking_io`].
const ABORTED: i32 = 995;

fn run_volume(v: Volume, ctx: VolCtx) {
    let mut counted = false;
    follow_volume(&v, &ctx, &mut counted);
    if !counted {
        // ended before it was live (stopped, or the volume failed: logged)
        count_live(&ctx);
    }
}

fn count_live(ctx: &VolCtx) {
    if ctx.pending.fetch_sub(1, Ordering::SeqCst) == 1 {
        log::line("live: every volume is indexed and followed");
        ctx.live.set();
    }
}

fn follow_volume(v: &Volume, ctx: &VolCtx, counted: &mut bool) {
    let mut checker = privacy::os::AccessChecker::new().ok();
    'volume: loop {
        // scan when there is no usable snapshot for this volume
        let needs_scan = ctx.state.lock().map(|s| s.vi.is_none()).unwrap_or(true);
        if needs_scan {
            let (journal, _) = match v.query_journal() {
                Ok(j) => j,
                Err(e) => {
                    log::line(format!("{}: journal unavailable ({e})", v.drive));
                    break;
                }
            };
            if !scan_volume(v, ctx, journal) {
                break;
            }
            ctx.dirty.store(true, Ordering::SeqCst);
        }
        refresh_status(ctx);
        let mut live_sent = false;
        loop {
            if ctx.stop.load(Ordering::Relaxed) {
                break 'volume;
            }
            let (from, jid) = match ctx.state.lock() {
                Ok(s) => (s.next_usn, s.journal.id),
                Err(_) => break 'volume,
            };
            // catch up without waiting; then the volume is live and reads block
            let wait = live_sent;
            let (mut next, mut recs) = match v.read_journal(from, jid, wait) {
                Ok(x) => x,
                Err(e) if ctx.stop.load(Ordering::Relaxed) || e.raw_os_error() == Some(ABORTED) => {
                    continue;
                }
                Err(e) if e.raw_os_error().is_some_and(|c| JOURNAL_LOST.contains(&c)) => {
                    log::line(format!(
                        "{}: change journal lost ({e}); scanning again",
                        v.drive
                    ));
                    if let (Ok(mut s), Ok(mut ix)) = (ctx.state.lock(), ctx.shared.index.write())
                        && let Some(vi) = s.vi.take()
                    {
                        s.live = false;
                        drop_volume(&mut ix, &vi);
                    }
                    continue 'volume;
                }
                Err(e) => {
                    log::line(format!("{}: journal read failed ({e}); retrying", v.drive));
                    std::thread::sleep(Duration::from_secs(1));
                    continue;
                }
            };
            if wait && !recs.is_empty() {
                // a change rarely comes alone: gather what follows within
                // the debounce, so a burst is one batch (idle CPU)
                std::thread::sleep(DEBOUNCE);
                while recs.len() < MAX_BATCH && !ctx.stop.load(Ordering::Relaxed) {
                    match v.read_journal(next, jid, false) {
                        Ok((n, more)) if !more.is_empty() => {
                            next = n;
                            recs.extend(more);
                        }
                        // caught up, or an error the next blocking read reports
                        _ => break,
                    }
                }
            }
            if !recs.is_empty() && !apply_batch(v, ctx, checker.as_mut(), &recs) {
                // a poisoned lock: stop following rather than skip records
                // (the served index is then refused or stale, never wrong)
                log::line(format!(
                    "{}: journal batch not applied; volume no longer followed",
                    v.drive
                ));
                return;
            }
            if let Ok(mut s) = ctx.state.lock() {
                s.next_usn = next;
            }
            if !recs.is_empty() {
                ctx.dirty.store(true, Ordering::SeqCst);
            }
            if !live_sent && (recs.is_empty() || next == from) {
                live_sent = true;
                if let Ok(mut s) = ctx.state.lock() {
                    s.live = true;
                }
                if !*counted {
                    *counted = true;
                    count_live(ctx);
                }
                refresh_status(ctx);
            }
        }
    }
}

/// Applies a batch of journal records with the restrictions of the
/// folders it touches, checked before the batch is shown.
fn apply_batch(
    v: &Volume,
    ctx: &VolCtx,
    checker: Option<&mut privacy::os::AccessChecker>,
    recs: &[crate::usn::UsnRecord],
) -> bool {
    let Ok(mut s) = ctx.state.lock() else {
        return false;
    };
    let Some(vi) = s.vi.as_mut() else {
        return false;
    };
    let todo = match ctx.shared.index.read() {
        Ok(ix) => access::checks_for(&ix, vi, recs),
        Err(_) => return false,
    };
    let verdicts: Vec<(u64, io::Result<Verdict>)> = match checker {
        Some(c) => todo.into_iter().map(|f| (f, check_frn(v, c, f))).collect(),
        None => todo
            .into_iter()
            .map(|f| (f, Err(io::Error::other("access checks unavailable"))))
            .collect(),
    };
    let waited = Instant::now();
    let Ok(mut ix) = ctx.shared.index.write() else {
        return false;
    };
    record_wait(waited.elapsed());
    for r in recs {
        vi.apply(&mut ix, r);
    }
    access::apply_verdicts(&mut ix, vi, &verdicts);
    true
}

/// How long journal batches waited for the index write lock, in µs
/// (readers move to a swapped snapshot if p95 > 100 ms).
static WRITER_WAITS: Mutex<Vec<u32>> = Mutex::new(Vec::new());

fn record_wait(d: Duration) {
    if let Ok(mut w) = WRITER_WAITS.lock()
        && w.len() < 1_000_000
    {
        w.push(d.as_micros().min(u32::MAX as u128) as u32);
    }
}

/// "n batches, p95 …, max …" of the journal writer's lock waits so far.
pub fn writer_wait_summary() -> Option<String> {
    let mut w = WRITER_WAITS.lock().ok()?.clone();
    if w.is_empty() {
        return None;
    }
    w.sort_unstable();
    let p95 = w[(w.len() * 95 / 100).min(w.len() - 1)];
    Some(format!(
        "journal writer lock wait: {} batches, p95 {:.2} ms, max {:.2} ms",
        w.len(),
        p95 as f64 / 1000.0,
        w[w.len() - 1] as f64 / 1000.0
    ))
}

/// Compacts the index when renames and removals left much behind (names
/// are appended, never overwritten: weeks of journal changes grow it), then
/// saves the image. Journal threads pause meanwhile (their state locks);
/// searches go on until the compacted index is swapped in.
fn save_service(
    path: &std::path::Path,
    shared: &Shared,
    states: &[Arc<Mutex<VolState>>],
    machine: Option<&privacy::Machine>,
) {
    let mut guards: Vec<_> = states.iter().filter_map(|s| s.lock().ok()).collect();
    if guards.len() != states.len() {
        log::line("a volume state lock is poisoned: not saved");
        return;
    }
    let needs_compaction = shared.index.read().is_ok_and(|ix| {
        ix.garbage_ratio() > COMPACT_GARBAGE || ix.raw_len() > ix.live_len() * 13 / 10 + 1000
    });
    if needs_compaction {
        let t = Instant::now();
        let snaps: Option<Vec<VolumeSnapshot>> = shared.index.read().ok().map(|ix| {
            guards
                .iter()
                .filter_map(|s| {
                    let vi = s.vi.as_ref()?;
                    Some(snapshot::capture(
                        vi,
                        &ix,
                        &s.drive,
                        s.serial,
                        s.journal.id,
                        s.next_usn,
                    ))
                })
                .collect()
        });
        if let Some(snaps) = snaps {
            let mut fresh = Index::new();
            if let (Some(m), Ok(mut reg)) = (machine, shared.owners.write()) {
                privacy::apply(&mut fresh, m, &mut reg);
            }
            let mut vis = snaps
                .into_iter()
                .map(|s| snapshot::restore_owned(&mut fresh, s))
                .collect::<Vec<_>>()
                .into_iter();
            let before = shared.index.read().map(|ix| ix.raw_len()).unwrap_or(0);
            let after = fresh.raw_len();
            if let Ok(mut w) = shared.index.write() {
                *w = fresh;
                for g in guards.iter_mut().filter(|g| g.vi.is_some()) {
                    g.vi = vis.next();
                }
                log::line(format!(
                    "compacted the index: {before} -> {after} entries in {:.2} s",
                    t.elapsed().as_secs_f64()
                ));
            }
        }
    }
    let bytes = match shared.index.read() {
        Ok(ix) => {
            let vols: Vec<snapshot::VolumeImage> = guards
                .iter()
                .filter_map(|s| {
                    Some(snapshot::VolumeImage {
                        drive: s.drive.clone(),
                        volume_serial: s.serial,
                        journal_id: s.journal.id,
                        next_usn: s.next_usn,
                        parts: s.vi.as_ref()?.parts(),
                    })
                })
                .collect();
            if vols.is_empty() {
                return;
            }
            snapshot::encode_image(&ix, &vols)
        }
        Err(_) => return,
    };
    drop(guards);
    match snapshot::save_atomic(path, &bytes) {
        Ok(()) => log::line(format!(
            "image saved: {} ({} MiB)",
            path.display(),
            bytes.len() >> 20
        )),
        Err(e) => log::line(format!("image not saved ({}): {e}", path.display())),
    }
}

/// Share of left-over name text above which a save compacts first.
const COMPACT_GARBAGE: f64 = 0.3;
