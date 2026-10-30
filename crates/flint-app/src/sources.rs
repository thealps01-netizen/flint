//! Where answers come from: the service (every NTFS volume) and indexers in
//! this process (fixed drives without NTFS, and the user's own folders
//! while the service is missing), merged by score.

use flint_indexer::protocol::{ServerMsg, VolumeStatus, WireHit};
use serde::Serialize;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub type Hit = WireHit;

/// Why a backend gave no answer.
#[derive(Clone, Debug, PartialEq)]
pub enum BackendError {
    /// Not reachable (not installed, stopped, pipe broken).
    Lost(String),
    /// Speaks another protocol version: the app and indexer must match.
    Version(String),
    /// Running, but not ready to answer yet (privacy rules loading).
    Starting(String),
}

pub struct Reply {
    pub hits: Vec<Hit>,
    pub total: u64,
    pub elapsed_us: u64,
}

pub trait Backend: Send + Sync {
    fn query(
        &self,
        text: &str,
        limit: u32,
        boosts: &[(String, f32)],
    ) -> Result<Reply, BackendError>;
    fn status(&self) -> Result<(u64, Vec<VolumeStatus>), BackendError>;
    /// Stops whatever runs in this process (and saves it); later queries
    /// answer `Lost`.
    fn shutdown(&self) {}
}

/// Which source answered, for the launcher's status line and banners.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum SourceState {
    /// The service answered.
    Service,
    /// No service since the app started: only the user's folders and
    /// other drives are indexed ("Basic mode").
    UserMode,
    /// The service answered earlier and is gone now; answers come from the
    /// user's folders until it returns.
    ServiceLost { reason: String },
    /// The service is starting (its privacy rules are not loaded yet).
    ServiceStarting,
    /// The service speaks another protocol version.
    VersionMismatch { detail: String },
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Answer {
    pub seq: u64,
    pub hits: Vec<Hit>,
    pub total: u64,
    pub elapsed_us: u64,
    pub source: SourceState,
    /// Per hit: opened through Flint before (the "Opened often" mark);
    /// filled by the caller that owns the history.
    pub opened: Vec<bool>,
    /// Per hit: what its icon depends on (`icons::icon_key`), so the page
    /// asks for each icon once; filled by the caller.
    pub icon_keys: Vec<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusInfo {
    pub entries: u64,
    pub volumes: Vec<VolumeStatus>,
    pub state: SourceState,
}

/// With exclusions set, each backend is asked for this many times the
/// limit (at most `MAX_ASK`), so excluded hits do not shorten the list.
const EXCLUDED_OVERFETCH: u32 = 4;
const MAX_ASK: u32 = 1000;
/// A service answer slower than this counts as the service being lost
/// (answers take ~10 ms; the first after a start, well under a second).
const SERVICE_READ_TIMEOUT_MS: u32 = 2000;
/// The user-folder indexer is stopped after this many service answers in
/// a row (it holds the profile's index in memory, ~300 MB).
const RELEASE_FALLBACK_AFTER: u32 = 20;

pub type Factory = Box<dyn Fn() -> Result<Box<dyn Backend>, String> + Send + Sync>;

/// The user-folder indexer, started when the service cannot answer and
/// stopped again once the service answers steadily.
struct Fallback {
    make: Factory,
    running: Option<Arc<dyn Backend>>,
    failed: Option<String>,
    /// Held while a stopped indexer saves, so a new one does not read its
    /// files half-written.
    stopping: Arc<Mutex<()>>,
}

/// What the app indexes beside the service: the folders the service
/// leaves out (app data) that Settings asks for. Started while the
/// service answers; stopped when it does not (the user-folder indexer
/// then covers them) or is turned off.
#[derive(Default)]
struct Beside {
    make: Option<Factory>,
    running: Option<Arc<dyn Backend>>,
    failed: bool,
    /// Held while a stopped indexer saves, so a new one does not read its
    /// files half-written.
    stopping: Arc<Mutex<()>>,
}

pub struct Sources {
    service: Box<dyn Backend>,
    /// Always asked (fixed drives the service does not index).
    always: Vec<Box<dyn Backend>>,
    fallback: Mutex<Fallback>,
    beside: Mutex<Beside>,
    /// The service was turned off from Settings: Basic mode, whatever an
    /// answer still on its way says.
    removed: AtomicBool,
    seen_service: AtomicBool,
    /// Service answers in a row (see `RELEASE_FALLBACK_AFTER`).
    service_streak: std::sync::atomic::AtomicU32,
    latest: AtomicU64,
    /// Excluded folders (Settings), as `settings::exclusion_keys` gives them.
    excluded: std::sync::RwLock<Vec<String>>,
}

/// Merges answers: best score first (ties: shorter path), one hit per path
/// (paths compare case-insensitively, as on Windows), at most `limit`.
pub fn merge(mut hits: Vec<Hit>, limit: usize) -> Vec<Hit> {
    hits.sort_by(|a, b| b.score.cmp(&a.score).then(a.path.len().cmp(&b.path.len())));
    let mut seen = std::collections::HashSet::new();
    hits.retain(|h| seen.insert(h.path.to_lowercase()));
    hits.truncate(limit);
    hits
}

impl Sources {
    pub fn new(
        service: Box<dyn Backend>,
        always: Vec<Box<dyn Backend>>,
        fallback: Factory,
    ) -> Sources {
        Sources {
            service,
            always,
            fallback: Mutex::new(Fallback {
                make: fallback,
                running: None,
                failed: None,
                stopping: Arc::default(),
            }),
            beside: Mutex::default(),
            removed: AtomicBool::new(false),
            seen_service: AtomicBool::new(false),
            service_streak: Default::default(),
            latest: AtomicU64::new(0),
            excluded: Default::default(),
        }
    }

    /// Also asks the backend `make` starts while the service answers.
    pub fn beside_service(self, make: Factory) -> Sources {
        if let Ok(mut b) = self.beside.lock() {
            b.make = Some(make);
        }
        self
    }

    fn beside(&self) -> Option<Arc<dyn Backend>> {
        // turned off: an answer still on its way starts nothing
        if self.removed.load(Ordering::SeqCst) {
            return None;
        }
        let mut b = self.beside.lock().ok()?;
        if b.running.is_none()
            && !b.failed
            && let Some(make) = b.make.as_ref()
        {
            let stopping = b.stopping.clone();
            let _saved = stopping.lock();
            match make() {
                Ok(x) => b.running = Some(Arc::from(x)),
                Err(e) => {
                    crate::log::line(format!("the app data indexer did not start: {e}"));
                    b.failed = true;
                }
            }
        }
        b.running.clone()
    }

    /// Stops what is indexed beside the service (it saves on its own
    /// thread); it starts again when the service answers.
    fn stop_beside(&self) {
        let Ok(mut b) = self.beside.lock() else {
            return;
        };
        let Some(running) = b.running.take() else {
            return;
        };
        let stopping = b.stopping.clone();
        std::thread::spawn(move || {
            let _saving = stopping.lock();
            running.shutdown();
        });
    }

    /// The service was removed on purpose ("Turn off full indexing"):
    /// Flint is in Basic mode, not missing its service, and what it
    /// indexed beside the service is stopped (the user-folder indexer
    /// covers it now).
    pub fn forget_service(&self) {
        self.removed.store(true, Ordering::SeqCst);
        self.seen_service.store(false, Ordering::Relaxed);
        self.service_streak.store(0, Ordering::Relaxed);
        self.stop_beside();
    }

    /// The service was turned on again from Settings.
    pub fn expect_service(&self) {
        self.removed.store(false, Ordering::SeqCst);
        if let Ok(mut b) = self.beside.lock() {
            b.failed = false;
        }
    }

    pub fn set_excluded(&self, keys: Vec<String>) {
        if let Ok(mut ex) = self.excluded.write() {
            *ex = keys;
        }
    }

    fn fallback(&self) -> Option<Arc<dyn Backend>> {
        let mut f = self.fallback.lock().ok()?;
        if f.running.is_none() && f.failed.is_none() {
            let stopping = f.stopping.clone();
            let _saved = stopping.lock();
            match (f.make)() {
                Ok(b) => f.running = Some(Arc::from(b)),
                Err(e) => {
                    crate::log::line(format!("the user-folder indexer did not start: {e}"));
                    f.failed = Some(e);
                }
            }
        }
        f.running.clone()
    }

    fn state_for(&self, e: &BackendError) -> SourceState {
        match e {
            BackendError::Version(d) => SourceState::VersionMismatch { detail: d.clone() },
            BackendError::Starting(_) => SourceState::ServiceStarting,
            BackendError::Lost(r)
                if self.seen_service.load(Ordering::Relaxed)
                    && !self.removed.load(Ordering::SeqCst) =>
            {
                SourceState::ServiceLost { reason: r.clone() }
            }
            BackendError::Lost(_) => SourceState::UserMode,
        }
    }

    /// Answers query `seq`; `None` when a newer query started meanwhile
    /// (its answer must not be replaced by this older one).
    pub fn query(
        &self,
        seq: u64,
        text: &str,
        limit: u32,
        boosts: &[(String, f32)],
    ) -> Option<Answer> {
        // a newer query already reached the app: this one is not asked at all
        if self.latest.fetch_max(seq, Ordering::SeqCst) > seq {
            return None;
        }
        let excluded = self.excluded.read().map(|e| e.clone()).unwrap_or_default();
        // excluded hits are dropped after the backends cut to their limit;
        // ask for more so the list still fills behind them
        let ask = if excluded.is_empty() {
            limit
        } else {
            limit.saturating_mul(EXCLUDED_OVERFETCH).min(MAX_ASK)
        };
        let mut hits = Vec::new();
        let (mut total, mut elapsed_us) = (0u64, 0u64);
        let mut take = |mut r: Reply| {
            // excluded hits are dropped and not counted; ones beyond the
            // limit are never seen, so the count can stay a little high
            let before = r.hits.len();
            r.hits
                .retain(|h| !crate::settings::is_excluded(&h.path, &excluded));
            total += r.total.saturating_sub((before - r.hits.len()) as u64);
            elapsed_us = elapsed_us.max(r.elapsed_us);
            hits.extend(r.hits);
        };
        let source = match self.service.query(text, ask, boosts) {
            Ok(r) => {
                self.seen_service.store(true, Ordering::Relaxed);
                if self.service_streak.fetch_add(1, Ordering::Relaxed) + 1 == RELEASE_FALLBACK_AFTER
                {
                    self.release_fallback();
                }
                take(r);
                if let Some(b) = self.beside() {
                    match b.query(text, ask, boosts) {
                        Ok(r) => take(r),
                        Err(e) => crate::log::line(format!("app data search failed: {e:?}")),
                    }
                }
                SourceState::Service
            }
            Err(e) => {
                self.service_streak.store(0, Ordering::Relaxed);
                // the user-folder indexer covers app data folders itself
                self.stop_beside();
                if let Some(f) = self.fallback() {
                    match f.query(text, ask, boosts) {
                        Ok(r) => take(r),
                        Err(fe) => crate::log::line(format!("user-folder search failed: {fe:?}")),
                    }
                }
                self.state_for(&e)
            }
        };
        for b in &self.always {
            if self.latest.load(Ordering::SeqCst) > seq {
                return None;
            }
            match b.query(text, ask, boosts) {
                Ok(r) => take(r),
                Err(e) => crate::log::line(format!("drive search failed: {e:?}")),
            }
        }
        if self.latest.load(Ordering::SeqCst) > seq {
            return None;
        }
        Some(Answer {
            seq,
            hits: merge(hits, limit as usize),
            total,
            elapsed_us,
            source,
            opened: Vec::new(),
            icon_keys: Vec::new(),
        })
    }

    /// The service answers steadily: the user-folder indexer is stopped
    /// (it saves on its own thread) and its memory freed; it starts again
    /// if the service goes away.
    fn release_fallback(&self) {
        let Ok(mut f) = self.fallback.lock() else {
            return;
        };
        let Some(running) = f.running.take() else {
            return;
        };
        let stopping = f.stopping.clone();
        crate::log::line("the service answers: stopping the user-folder indexer");
        std::thread::spawn(move || {
            let _saving = stopping.lock();
            running.shutdown();
        });
    }

    /// Stops the in-process indexers so they save (the app is quitting).
    pub fn shutdown(&self) {
        for b in &self.always {
            b.shutdown();
        }
        if let Some(b) = self.beside.lock().ok().and_then(|b| b.running.clone()) {
            b.shutdown();
        }
        let (running, stopping) = match self.fallback.lock() {
            Ok(f) => (f.running.clone(), Some(f.stopping.clone())),
            Err(_) => (None, None),
        };
        if let Some(f) = running {
            f.shutdown();
        }
        // one stopped earlier may still be saving
        if let Some(s) = stopping {
            drop(s.lock());
        }
    }

    pub fn status(&self) -> StatusInfo {
        let mut entries = 0;
        let mut volumes = Vec::new();
        let mut add = |r: Result<(u64, Vec<VolumeStatus>), BackendError>| match r {
            Ok((n, v)) => {
                entries += n;
                volumes.extend(v);
                true
            }
            Err(_) => false,
        };
        let state = match self.service.status() {
            Ok(s) => {
                add(Ok(s));
                self.seen_service.store(true, Ordering::Relaxed);
                // its entries count; its drive is the service's already
                if let Some(b) = self.beside() {
                    add(b.status().map(|(n, _)| (n, Vec::new())));
                }
                SourceState::Service
            }
            Err(e) => {
                if let Some(f) = self.fallback() {
                    add(f.status());
                }
                self.state_for(&e)
            }
        };
        for b in &self.always {
            add(b.status());
        }
        StatusInfo {
            entries,
            volumes,
            state,
        }
    }
}

/// The service over its pipe; reconnects at most every `retry`.
pub struct ServicePipe {
    client: Mutex<Option<flint_indexer::pipe::Client>>,
    last_try: Mutex<Option<Instant>>,
    last_error: Mutex<BackendError>,
    retry: Duration,
}

impl Default for ServicePipe {
    fn default() -> Self {
        ServicePipe {
            client: Mutex::new(None),
            last_try: Mutex::new(None),
            last_error: Mutex::new(BackendError::Lost("not connected yet".into())),
            retry: Duration::from_secs(2),
        }
    }
}

/// Maps an indexer error ("code: message") to a backend error.
pub fn classify(e: &std::io::Error) -> BackendError {
    let s = e.to_string();
    if s.starts_with("version:") {
        BackendError::Version(s)
    } else if s.starts_with("starting:") {
        BackendError::Starting(s)
    } else {
        BackendError::Lost(s)
    }
}

impl ServicePipe {
    fn with<T>(
        &self,
        f: impl FnOnce(&flint_indexer::pipe::Client) -> std::io::Result<T>,
    ) -> Result<T, BackendError> {
        let mut c = self
            .client
            .lock()
            .map_err(|_| BackendError::Lost("poisoned".into()))?;
        if c.is_none() {
            let mut last = self
                .last_try
                .lock()
                .map_err(|_| BackendError::Lost("poisoned".into()))?;
            if last.is_some_and(|t| t.elapsed() < self.retry) {
                return Err(self
                    .last_error
                    .lock()
                    .map(|e| e.clone())
                    .unwrap_or(BackendError::Lost("".into())));
            }
            *last = Some(Instant::now());
            match flint_indexer::pipe::Client::connect_service() {
                Ok(client) => {
                    // a service that stops answering must not hold every
                    // search: past this, the pipe counts as lost and the
                    // user-folder indexer answers
                    use flint_indexer::server::Conn;
                    client.conn.set_read_timeout(Some(SERVICE_READ_TIMEOUT_MS));
                    *c = Some(client)
                }
                Err(e) => {
                    let err = classify(&e);
                    if let Ok(mut l) = self.last_error.lock() {
                        *l = err.clone();
                    }
                    return Err(err);
                }
            }
        }
        let client = c.as_ref().expect("connected above");
        f(client).map_err(|e| {
            let err = classify(&e);
            if matches!(err, BackendError::Lost(_)) {
                *c = None; // broken pipe: reconnect next time
            }
            err
        })
    }
}

fn reply(m: ServerMsg) -> Result<Reply, BackendError> {
    match m {
        ServerMsg::Results {
            hits,
            total,
            elapsed_us,
            ..
        } => Ok(Reply {
            hits,
            total: total as u64,
            elapsed_us: elapsed_us as u64,
        }),
        ServerMsg::Cancelled { .. } => Ok(Reply {
            hits: vec![],
            total: 0,
            elapsed_us: 0,
        }),
        ServerMsg::Error { code, message } if code == "starting" => {
            Err(BackendError::Starting(message))
        }
        ServerMsg::Error { code, message } => Err(BackendError::Lost(format!("{code}: {message}"))),
        other => Err(BackendError::Lost(format!("unexpected reply {other:?}"))),
    }
}

impl Backend for ServicePipe {
    fn query(
        &self,
        text: &str,
        limit: u32,
        boosts: &[(String, f32)],
    ) -> Result<Reply, BackendError> {
        self.with(|c| c.query(text, limit, boosts.to_vec()))
            .and_then(reply)
    }

    fn status(&self) -> Result<(u64, Vec<VolumeStatus>), BackendError> {
        match self.with(|c| c.status())? {
            ServerMsg::StatusReply { entries, volumes } => Ok((entries, volumes)),
            other => Err(BackendError::Lost(format!("unexpected reply {other:?}"))),
        }
    }
}

/// An indexer running inside the app (user mode: walk + watch). Searches
/// share the read lock; `shutdown` takes the engine out and stops it.
pub struct Local(std::sync::RwLock<Option<flint_indexer::engine::Engine>>);

impl Local {
    pub fn new(e: flint_indexer::engine::Engine) -> Local {
        Local(std::sync::RwLock::new(Some(e)))
    }

    fn with<T>(
        &self,
        f: impl FnOnce(&flint_indexer::engine::Engine) -> T,
    ) -> Result<T, BackendError> {
        match self.0.read() {
            Ok(g) => g
                .as_ref()
                .map(f)
                .ok_or_else(|| BackendError::Lost("stopped".into())),
            Err(_) => Err(BackendError::Lost("poisoned".into())),
        }
    }
}

impl Backend for Local {
    fn query(
        &self,
        text: &str,
        limit: u32,
        boosts: &[(String, f32)],
    ) -> Result<Reply, BackendError> {
        self.with(|e| e.search(text, limit, boosts)).and_then(reply)
    }

    fn status(&self) -> Result<(u64, Vec<VolumeStatus>), BackendError> {
        self.with(|e| {
            let sh = e.shared();
            let entries = sh.index.read().map(|i| i.live_len() as u64).unwrap_or(0);
            let mut volumes = sh.volumes.lock().map(|v| v.clone()).unwrap_or_default();
            // a user-mode indexer reports drives, not counts: a single
            // drive gets the whole count
            if let [v] = volumes.as_mut_slice() {
                v.entries = entries;
            }
            (entries, volumes)
        })
    }

    fn shutdown(&self) {
        let taken = self.0.write().ok().and_then(|mut g| g.take());
        if let Some(e) = taken {
            e.stop();
        }
    }
}

/// Starts an in-process indexer over `roots`, saving to
/// `%LOCALAPPDATA%\Flint\<tag>` and serving on a pipe of its own.
pub fn start_local(
    tag: &str,
    roots: Vec<std::path::PathBuf>,
    skip: Vec<std::path::PathBuf>,
) -> Result<Box<dyn Backend>, String> {
    use flint_indexer::engine::{Config, Engine};
    use flint_indexer::pipe::{current_user_sid, user_pipe_name};
    let base = std::env::var_os("LOCALAPPDATA").ok_or("LOCALAPPDATA is not set")?;
    let data = std::path::PathBuf::from(base).join("Flint").join(tag);
    let sid = current_user_sid().map_err(|e| e.to_string())?;
    let pipe = format!("{}-app-{tag}", user_pipe_name(&sid));
    let cfg = Config::user(roots, data, pipe)
        .map_err(|e| e.to_string())?
        .skipping(skip);
    Engine::start(cfg)
        .map(|e| Box::new(Local::new(e)) as Box<dyn Backend>)
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(path: &str, score: i64) -> Hit {
        Hit {
            path: path.into(),
            name: path.rsplit('\\').next().unwrap().into(),
            is_dir: false,
            typo: false,
            highlights: vec![],
            last_change: 0,
            score,
        }
    }

    /// A backend whose answer the test sets.
    #[derive(Clone)]
    struct Fake(Arc<Mutex<Result<Vec<Hit>, BackendError>>>);

    impl Fake {
        fn new(r: Result<Vec<Hit>, BackendError>) -> Fake {
            Fake(Arc::new(Mutex::new(r)))
        }
        fn set(&self, r: Result<Vec<Hit>, BackendError>) {
            *self.0.lock().unwrap() = r;
        }
    }

    impl Backend for Fake {
        fn query(&self, _t: &str, limit: u32, _b: &[(String, f32)]) -> Result<Reply, BackendError> {
            self.0.lock().unwrap().clone().map(|mut hits| {
                let total = hits.len() as u64;
                hits.truncate(limit as usize);
                Reply {
                    total,
                    hits,
                    elapsed_us: 5,
                }
            })
        }
        fn status(&self) -> Result<(u64, Vec<VolumeStatus>), BackendError> {
            self.0
                .lock()
                .unwrap()
                .clone()
                .map(|h| (h.len() as u64, vec![]))
        }
    }

    fn sources(service: &Fake, always: Vec<Fake>, profile: &Fake) -> Sources {
        let p = profile.clone();
        Sources::new(
            Box::new(service.clone()),
            always
                .into_iter()
                .map(|f| Box::new(f) as Box<dyn Backend>)
                .collect(),
            Box::new(move || Ok(Box::new(p.clone()) as Box<dyn Backend>)),
        )
    }

    fn paths(a: &Answer) -> Vec<&str> {
        a.hits.iter().map(|h| h.path.as_str()).collect()
    }

    #[test]
    fn excluded_folders_never_reach_the_list() {
        let service = Fake::new(Ok(vec![
            hit(r"C:\Data\rapor.txt", 90),
            hit(r"C:\Database\rapor.txt", 80),
            hit(r"C:\data\x\RAPOR.md", 70),
        ]));
        let s = sources(&service, vec![], &Fake::new(Ok(vec![])));
        s.set_excluded(crate::settings::exclusion_keys(&[r"C:\Data".into()]));
        let a = s.query(1, "rapor", 10, &[]).unwrap();
        assert_eq!(paths(&a), [r"C:\Database\rapor.txt"]);
        // the excluded hits are not counted either
        assert_eq!(a.total, 1);
        s.set_excluded(vec![]);
        assert_eq!(s.query(2, "rapor", 10, &[]).unwrap().hits.len(), 3);
    }

    #[test]
    fn excluded_folders_do_not_shorten_the_list() {
        // the best hits all sit in an excluded folder; the list still
        // fills with the matches behind them
        let mut all: Vec<Hit> = (0..6)
            .map(|i| hit(&format!(r"C:\nm\rapor{i}.txt"), 90))
            .collect();
        all.push(hit(r"C:\docs\rapor.txt", 50));
        all.push(hit(r"C:\docs\rapor2.txt", 40));
        let service = Fake::new(Ok(all));
        let s = sources(&service, vec![], &Fake::new(Ok(vec![])));
        s.set_excluded(crate::settings::exclusion_keys(&[r"C:\nm".into()]));
        let a = s.query(1, "rapor", 2, &[]).unwrap();
        assert_eq!(paths(&a), [r"C:\docs\rapor.txt", r"C:\docs\rapor2.txt"]);
    }

    struct Counting(Arc<std::sync::atomic::AtomicUsize>);
    impl Backend for Counting {
        fn query(&self, _t: &str, _l: u32, _b: &[(String, f32)]) -> Result<Reply, BackendError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(Reply {
                hits: vec![],
                total: 0,
                elapsed_us: 1,
            })
        }
        fn status(&self) -> Result<(u64, Vec<VolumeStatus>), BackendError> {
            Ok((0, vec![]))
        }
    }

    struct Stoppable(Arc<std::sync::atomic::AtomicUsize>);
    impl Backend for Stoppable {
        fn query(&self, _t: &str, _l: u32, _b: &[(String, f32)]) -> Result<Reply, BackendError> {
            Ok(Reply {
                hits: vec![],
                total: 0,
                elapsed_us: 1,
            })
        }
        fn status(&self) -> Result<(u64, Vec<VolumeStatus>), BackendError> {
            Ok((0, vec![]))
        }
        fn shutdown(&self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn the_fallback_stops_once_the_service_answers_steadily() {
        let service = Fake::new(Err(BackendError::Lost("not running".into())));
        let made = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let stopped = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (m, st) = (made.clone(), stopped.clone());
        let s = Sources::new(
            Box::new(service.clone()),
            vec![],
            Box::new(move || {
                m.fetch_add(1, Ordering::SeqCst);
                Ok(Box::new(Stoppable(st.clone())) as Box<dyn Backend>)
            }),
        );
        let mut seq = 0;
        let mut ask = |s: &Sources| {
            seq += 1;
            s.query(seq, "a", 10, &[])
        };
        ask(&s);
        assert_eq!(
            made.load(Ordering::SeqCst),
            1,
            "started while the service is away"
        );
        service.set(Ok(vec![]));
        for _ in 0..RELEASE_FALLBACK_AFTER - 1 {
            ask(&s);
        }
        assert_eq!(
            stopped.load(Ordering::SeqCst),
            0,
            "not on the first answers"
        );
        ask(&s);
        let t = std::time::Instant::now();
        while stopped.load(Ordering::SeqCst) == 0 && t.elapsed().as_secs() < 5 {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(stopped.load(Ordering::SeqCst), 1, "stopped (and saved)");
        // the service goes away again: the fallback starts again
        service.set(Err(BackendError::Lost("stopped".into())));
        ask(&s);
        assert_eq!(made.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn a_query_already_stale_is_not_sent() {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let s = Sources::new(
            Box::new(Counting(calls.clone())),
            vec![],
            Box::new(|| Err("no fallback".into())),
        );
        assert!(s.query(5, "a", 10, &[]).is_some());
        // query 3 was typed before 5 but reached the app after it
        assert!(s.query(3, "ab", 10, &[]).is_none());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn merge_orders_by_score_and_dedups() {
        let service = Fake::new(Ok(vec![
            hit(r"C:\a\rapor.txt", 90),
            hit(r"C:\b\rapor-eski.txt", 40),
        ]));
        let drives = Fake::new(Ok(vec![
            hit(r"D:\rapor.txt", 60),
            hit(r"D:\x\rapor.txt", 60),
        ]));
        let s = sources(&service, vec![drives.clone()], &Fake::new(Ok(vec![])));
        let a = s.query(1, "rapor", 10, &[]).unwrap();
        assert_eq!(
            paths(&a),
            [
                r"C:\a\rapor.txt",
                r"D:\rapor.txt",
                r"D:\x\rapor.txt",
                r"C:\b\rapor-eski.txt"
            ]
        );
        assert_eq!(a.source, SourceState::Service);
        // the same file from two sources (case differs) is listed once
        drives.set(Ok(vec![hit(r"c:\A\RAPOR.TXT", 10)]));
        assert_eq!(paths(&s.query(2, "rapor", 10, &[]).unwrap()).len(), 2);
    }

    #[test]
    fn limit_applies_after_merge() {
        let service = Fake::new(Ok(vec![hit(r"C:\1", 10), hit(r"C:\2", 8)]));
        let drives = Fake::new(Ok(vec![hit(r"D:\1", 9), hit(r"D:\2", 7)]));
        let s = sources(&service, vec![drives], &Fake::new(Ok(vec![])));
        assert_eq!(
            paths(&s.query(1, "x", 3, &[]).unwrap()),
            [r"C:\1", r"D:\1", r"C:\2"]
        );
    }

    #[test]
    fn service_loss_falls_back_and_recovers() {
        let service = Fake::new(Ok(vec![hit(r"C:\Windows\a.txt", 50)]));
        let profile = Fake::new(Ok(vec![hit(r"C:\Users\x\a.txt", 50)]));
        let s = sources(&service, vec![], &profile);
        assert_eq!(
            s.query(1, "a", 10, &[]).unwrap().source,
            SourceState::Service
        );
        service.set(Err(BackendError::Lost("pipe broken".into())));
        let a = s.query(2, "a", 10, &[]).unwrap();
        assert_eq!(
            a.source,
            SourceState::ServiceLost {
                reason: "pipe broken".into()
            }
        );
        assert_eq!(
            paths(&a),
            [r"C:\Users\x\a.txt"],
            "answered from the user's folders"
        );
        service.set(Ok(vec![hit(r"C:\Windows\a.txt", 50)]));
        let a = s.query(3, "a", 10, &[]).unwrap();
        assert_eq!(a.source, SourceState::Service);
        assert_eq!(
            paths(&a),
            [r"C:\Windows\a.txt"],
            "the fallback is not asked while the service answers"
        );
    }

    /// A backend that records its shutdown (and answers like a Fake).
    struct Recording(Fake, Arc<AtomicBool>);
    impl Backend for Recording {
        fn query(&self, t: &str, l: u32, b: &[(String, f32)]) -> Result<Reply, BackendError> {
            self.0.query(t, l, b)
        }
        fn status(&self) -> Result<(u64, Vec<VolumeStatus>), BackendError> {
            // a basic-mode drive, as a folder walk reports it
            self.0.status().map(|(n, _)| {
                let drive = VolumeStatus {
                    drive: "C:".into(),
                    state: "basic".into(),
                    entries: n,
                };
                (n, vec![drive])
            })
        }
        fn shutdown(&self) {
            self.1.store(true, Ordering::SeqCst);
        }
    }

    #[test]
    fn app_data_beside_the_service_is_asked_while_it_answers() {
        let service = Fake::new(Err(BackendError::Lost("not installed".into())));
        let profile = Fake::new(Ok(vec![hit(r"C:\Users\x\a.txt", 50)]));
        let beside = Fake::new(Ok(vec![hit(r"C:\Users\x\AppData\Roaming\a.txt", 40)]));
        let (started, stopped) = (
            Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            Arc::new(AtomicBool::new(false)),
        );
        let (b, n, st) = (beside.clone(), started.clone(), stopped.clone());
        let s = sources(&service, vec![], &profile).beside_service(Box::new(move || {
            n.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(Recording(b.clone(), st.clone())) as Box<dyn Backend>)
        }));
        // Basic mode: the user-folder indexer covers app data itself
        assert_eq!(
            paths(&s.query(1, "a", 10, &[]).unwrap()),
            [r"C:\Users\x\a.txt"]
        );
        assert_eq!(started.load(Ordering::SeqCst), 0);
        service.set(Ok(vec![hit(r"C:\Windows\a.txt", 50)]));
        for seq in 2..5 {
            let a = s.query(seq, "a", 10, &[]).unwrap();
            assert_eq!(
                paths(&a),
                [r"C:\Windows\a.txt", r"C:\Users\x\AppData\Roaming\a.txt"]
            );
        }
        assert_eq!(started.load(Ordering::SeqCst), 1, "started once");
        let st = s.status();
        assert_eq!(st.entries, 2);
        assert!(
            st.volumes.is_empty(),
            "the service lists the drive; the beside one does not"
        );
        // the service lost: the user-folder indexer covers app data, the
        // beside one stops; it starts again when the service is back
        service.set(Err(BackendError::Lost("crashed".into())));
        let a = s.query(10, "a", 10, &[]).unwrap();
        assert_eq!(paths(&a), [r"C:\Users\x\a.txt"]);
        let end = Instant::now() + Duration::from_secs(5);
        while !stopped.load(Ordering::SeqCst) && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(stopped.swap(false, Ordering::SeqCst));
        service.set(Ok(vec![hit(r"C:\Windows\a.txt", 50)]));
        assert_eq!(s.query(11, "a", 10, &[]).unwrap().hits.len(), 2);
        assert_eq!(started.load(Ordering::SeqCst), 2, "started again");
        // turned off on purpose: Basic mode, and the beside indexer stops
        service.set(Err(BackendError::Lost("removed".into())));
        s.forget_service();
        let a = s.query(20, "a", 10, &[]).unwrap();
        assert_eq!(a.source, SourceState::UserMode);
        assert_eq!(paths(&a), [r"C:\Users\x\a.txt"]);
        let end = Instant::now() + Duration::from_secs(5);
        while !stopped.load(Ordering::SeqCst) && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(stopped.load(Ordering::SeqCst));
    }

    #[test]
    fn turned_off_is_basic_mode_even_after_a_late_answer() {
        let service = Fake::new(Ok(vec![hit(r"C:\Windows\a.txt", 50)]));
        let profile = Fake::new(Ok(vec![hit(r"C:\Users\x\a.txt", 50)]));
        let s = sources(&service, vec![], &profile);
        assert_eq!(
            s.query(1, "a", 10, &[]).unwrap().source,
            SourceState::Service
        );
        s.forget_service();
        // an answer that was already on its way
        assert_eq!(
            s.query(2, "a", 10, &[]).unwrap().source,
            SourceState::Service
        );
        service.set(Err(BackendError::Lost("removed".into())));
        assert_eq!(
            s.query(3, "a", 10, &[]).unwrap().source,
            SourceState::UserMode
        );
        // turned on again: a lost service is reported again
        s.expect_service();
        service.set(Ok(vec![hit(r"C:\Windows\a.txt", 50)]));
        s.query(4, "a", 10, &[]).unwrap();
        service.set(Err(BackendError::Lost("crashed".into())));
        assert!(matches!(
            s.query(5, "a", 10, &[]).unwrap().source,
            SourceState::ServiceLost { .. }
        ));
    }

    #[test]
    fn no_service_at_all_is_basic_mode() {
        let service = Fake::new(Err(BackendError::Lost("not installed".into())));
        let profile = Fake::new(Ok(vec![hit(r"C:\Users\x\a.txt", 50)]));
        let s = sources(&service, vec![], &profile);
        let a = s.query(1, "a", 10, &[]).unwrap();
        assert_eq!(a.source, SourceState::UserMode);
        assert_eq!(a.hits.len(), 1);
        assert_eq!(s.status().state, SourceState::UserMode);
    }

    #[test]
    fn version_mismatch_is_reported() {
        let service = Fake::new(Err(BackendError::Version(
            "version: the indexer speaks protocol 1".into(),
        )));
        let s = sources(&service, vec![], &Fake::new(Ok(vec![])));
        assert!(matches!(
            s.query(1, "a", 10, &[]).unwrap().source,
            SourceState::VersionMismatch { .. }
        ));
        assert!(matches!(
            classify(&std::io::Error::other(
                "version: the indexer speaks protocol 1, the app 2"
            )),
            BackendError::Version(_)
        ));
        assert!(matches!(
            classify(&std::io::Error::other("starting: rules")),
            BackendError::Starting(_)
        ));
        assert!(matches!(
            classify(&std::io::Error::other("pipe closed")),
            BackendError::Lost(_)
        ));
    }

    #[test]
    fn older_seq_never_overwrites() {
        /// Blocks its first query until released, so a second query can
        /// start and finish while the first is still running.
        struct Slow(Mutex<Option<std::sync::mpsc::Receiver<()>>>);
        impl Backend for Slow {
            fn query(
                &self,
                _t: &str,
                _l: u32,
                _b: &[(String, f32)],
            ) -> Result<Reply, BackendError> {
                let rx = self.0.lock().unwrap().take();
                if let Some(rx) = rx {
                    rx.recv().unwrap();
                }
                Ok(Reply {
                    hits: vec![hit(r"C:\x", 1)],
                    total: 1,
                    elapsed_us: 1,
                })
            }
            fn status(&self) -> Result<(u64, Vec<VolumeStatus>), BackendError> {
                Ok((0, vec![]))
            }
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let s = Arc::new(Sources::new(
            Box::new(Slow(Mutex::new(Some(rx)))),
            vec![],
            Box::new(|| Err("unused".into())),
        ));
        let first = {
            let s = s.clone();
            std::thread::spawn(move || s.query(1, "r", 10, &[]))
        };
        std::thread::sleep(Duration::from_millis(50));
        assert!(
            s.query(2, "ra", 10, &[]).is_some(),
            "the newer query answers"
        );
        tx.send(()).unwrap();
        assert!(
            first.join().unwrap().is_none(),
            "the older answer is dropped"
        );
    }
}
