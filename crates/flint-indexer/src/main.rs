//! `flint-indexer`: the Flint index process.
//!
//! `flint-indexer scan C: [--snapshot FILE] [--follow SECS]` indexes one
//! NTFS volume from its MFT (needs administrator rights), prints entries,
//! time and memory, optionally saves a snapshot and follows the change
//! journal for SECS seconds, printing each change with its path.
//! `flint-indexer query TEXT` asks the running indexer.

use clap::{Parser, Subcommand};
use std::process::ExitCode;

#[derive(Parser)]
#[command(about = "Flint file-name indexer")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Index one NTFS volume from its MFT (administrator rights needed).
    Scan {
        /// Drive, e.g. "C:".
        drive: String,
        /// Save a snapshot of the volume to this file.
        #[arg(long)]
        snapshot: Option<std::path::PathBuf>,
        /// Follow the change journal for this many seconds after the scan.
        #[arg(long)]
        follow: Option<u64>,
    },
    /// Ask the running indexer (service first, then this user's) and
    /// print the results.
    Query {
        text: String,
        #[arg(long, default_value_t = 10)]
        limit: u32,
    },
    /// Print the running indexer's state (mode, volumes, entries).
    Status,
    /// Run the indexer in this console until Ctrl+C. Without
    /// `--user-mode` it indexes every NTFS drive (administrator rights,
    /// as the service does).
    Serve {
        /// Index only folders this user can open, without administrator
        /// rights (default folder: the user profile).
        #[arg(long)]
        user_mode: bool,
        /// Folders to index in user mode (repeatable).
        #[arg(long = "root")]
        roots: Vec<std::path::PathBuf>,
    },
    /// Install and start the FlintIndexer service (administrator).
    InstallService,
    /// Stop and remove the service and its data (administrator).
    UninstallService,
    /// Stop the service if it is installed (administrator; the installer
    /// uses it before replacing the program).
    StopService,
    /// Entry point for the Service Control Manager.
    #[command(name = "service-run", hide = true)]
    ServiceRun,
}

fn main() -> ExitCode {
    match Cli::parse().cmd {
        Cmd::Scan {
            drive,
            snapshot,
            follow,
        } => scan::run(&drive, snapshot.as_deref(), follow),
        Cmd::Query { text, limit } => query(&text, limit),
        Cmd::Status => status(),
        Cmd::Serve { user_mode, roots } => serve::run(user_mode, roots),
        Cmd::InstallService => serve::admin(serve::Admin::Install),
        Cmd::UninstallService => serve::admin(serve::Admin::Uninstall),
        Cmd::StopService => serve::admin(serve::Admin::Stop),
        Cmd::ServiceRun => serve::service_run(),
    }
}

#[cfg(not(windows))]
fn status() -> ExitCode {
    eprintln!("flint-indexer runs on Windows only");
    ExitCode::from(2)
}

#[cfg(windows)]
fn status() -> ExitCode {
    let c = match connect() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(4);
        }
    };
    match c.status() {
        Ok(flint_indexer::protocol::ServerMsg::StatusReply { entries, volumes }) => {
            println!("{} indexer, {entries} entries", c.mode);
            for v in volumes {
                println!("  {} {} ({} entries)", v.drive, v.state, v.entries);
            }
            ExitCode::SUCCESS
        }
        Ok(other) => {
            eprintln!("unexpected answer: {other:?}");
            ExitCode::from(2)
        }
        Err(e) => {
            eprintln!("status failed: {e}");
            ExitCode::from(2)
        }
    }
}

#[cfg(not(windows))]
mod serve {
    use std::process::ExitCode;
    pub fn run(_: bool, _: Vec<std::path::PathBuf>) -> ExitCode {
        eprintln!("flint-indexer runs on Windows only");
        ExitCode::from(2)
    }
    pub enum Admin {
        Install,
        Uninstall,
        Stop,
    }
    pub fn admin(_: Admin) -> ExitCode {
        run(false, vec![])
    }
    pub fn service_run() -> ExitCode {
        run(false, vec![])
    }
}

#[cfg(windows)]
mod serve {
    use flint_indexer::engine::{Config, Engine, service_data_dir};
    use flint_indexer::pipe::{current_user_sid, user_pipe_name};
    use flint_indexer::volume::{NEEDS_ADMIN, is_elevated};
    use flint_indexer::{log, service};
    use std::path::PathBuf;
    use std::process::ExitCode;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    static STOP: AtomicBool = AtomicBool::new(false);

    unsafe extern "system" fn on_ctrl(_: u32) -> windows_sys::core::BOOL {
        STOP.store(true, Ordering::SeqCst);
        1
    }

    pub fn run(user_mode: bool, roots: Vec<PathBuf>) -> ExitCode {
        let cfg = if user_mode {
            let sid = match current_user_sid() {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("cannot read the current user: {e}");
                    return ExitCode::from(2);
                }
            };
            let roots = if roots.is_empty() {
                match std::env::var_os("USERPROFILE") {
                    Some(p) => vec![PathBuf::from(p)],
                    None => {
                        eprintln!("USERPROFILE is not set: pass --root");
                        return ExitCode::from(2);
                    }
                }
            } else {
                roots
            };
            let Some(data) =
                std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("Flint"))
            else {
                eprintln!("LOCALAPPDATA is not set");
                return ExitCode::from(2);
            };
            match Config::user(roots, data, user_pipe_name(&sid)) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("{e}");
                    return ExitCode::from(2);
                }
            }
        } else {
            if !is_elevated() {
                eprintln!("{NEEDS_ADMIN}");
                return ExitCode::from(3);
            }
            match service_data_dir() {
                Ok(d) => Config::service(d),
                Err(e) => {
                    eprintln!("cannot prepare the data folder: {e}");
                    return ExitCode::from(2);
                }
            }
        };
        let _ = log::init(&cfg.data_dir.join(if user_mode {
            "indexer-user.log"
        } else {
            "indexer.log"
        }));
        // SAFETY: on_ctrl only stores to an atomic.
        unsafe { windows_sys::Win32::System::Console::SetConsoleCtrlHandler(Some(on_ctrl), 1) };
        let t = Instant::now();
        let engine = match Engine::start(cfg) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("cannot start: {e}");
                return ExitCode::from(2);
            }
        };
        println!(
            "serving ({:?} start, {} entries from the snapshot, {:.2} s); Ctrl+C stops",
            engine.start_kind(),
            engine.loaded_from_snapshot(),
            t.elapsed().as_secs_f64()
        );
        let mut said_live = false;
        while !STOP.load(Ordering::SeqCst) {
            if !said_live && engine.wait_live(Duration::from_millis(200)) {
                said_live = true;
                println!("live after {:.2} s", t.elapsed().as_secs_f64());
            } else if said_live {
                std::thread::sleep(Duration::from_millis(200));
            }
        }
        let t = Instant::now();
        engine.stop();
        println!("stopped in {:.2} s", t.elapsed().as_secs_f64());
        ExitCode::SUCCESS
    }

    pub enum Admin {
        Install,
        Uninstall,
        Stop,
    }

    pub fn admin(what: Admin) -> ExitCode {
        if !is_elevated() {
            eprintln!("changing the service needs administrator rights");
            return ExitCode::from(3);
        }
        let r = match what {
            Admin::Install => service::install(),
            Admin::Uninstall => service::uninstall(),
            Admin::Stop => service::stop(),
        };
        match r {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("{e}");
                ExitCode::from(2)
            }
        }
    }

    pub fn service_run() -> ExitCode {
        match service::run() {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("{e} (this command is for the Service Control Manager)");
                ExitCode::from(2)
            }
        }
    }
}

#[cfg(not(windows))]
fn query(_: &str, _: u32) -> ExitCode {
    eprintln!("flint-indexer runs on Windows only");
    ExitCode::from(2)
}

/// Connects to the service, else to this user's user-mode indexer.
#[cfg(windows)]
fn connect() -> std::io::Result<flint_indexer::pipe::Client> {
    flint_indexer::pipe::Client::connect_any()
}

#[cfg(windows)]
fn query(text: &str, limit: u32) -> ExitCode {
    use flint_indexer::protocol::ServerMsg;
    let c = match connect() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(4);
        }
    };
    match c.query(text, limit, vec![]) {
        Ok(ServerMsg::Results {
            total,
            elapsed_us,
            hits,
            ..
        }) => {
            println!(
                "{total} matches, {:.1} ms ({} indexer)",
                elapsed_us as f64 / 1000.0,
                c.mode
            );
            for (i, h) in hits.iter().enumerate() {
                println!(
                    "{:>2}. {}{}",
                    i + 1,
                    h.path,
                    if h.typo { "   [typo]" } else { "" }
                );
            }
            ExitCode::SUCCESS
        }
        Ok(other) => {
            eprintln!("unexpected answer: {other:?}");
            ExitCode::from(2)
        }
        Err(e) => {
            eprintln!("query failed: {e}");
            ExitCode::from(2)
        }
    }
}

#[cfg(not(windows))]
mod scan {
    pub fn run(_: &str, _: Option<&std::path::Path>, _: Option<u64>) -> std::process::ExitCode {
        eprintln!("flint-indexer runs on Windows only");
        std::process::ExitCode::from(2)
    }
}

#[cfg(windows)]
mod scan {
    use flint_core::Index;
    use flint_indexer::snapshot;
    use flint_indexer::usn::{
        USN_REASON_FILE_CREATE, USN_REASON_FILE_DELETE, USN_REASON_RENAME_NEW_NAME,
    };
    use flint_indexer::volindex::VolumeIndex;
    use flint_indexer::volume::{self, ERROR_ACCESS_DENIED, NEEDS_ADMIN, Volume};
    use std::path::Path;
    use std::process::ExitCode;
    use std::time::{Duration, Instant};

    pub fn run(drive: &str, snap: Option<&Path>, follow: Option<u64>) -> ExitCode {
        let drive = drive.trim_end_matches(['\\', '/']).to_uppercase();
        match volume::volume_info(&drive) {
            Ok((fs, _)) if fs != "NTFS" => {
                eprintln!(
                    "{drive} is {fs}, not NTFS: it is indexed by the folder walk, not by scan"
                );
                return ExitCode::from(2);
            }
            Ok(_) => {}
            Err(e) => {
                eprintln!("cannot read {drive}: {e}");
                return ExitCode::from(2);
            }
        }
        let vol = match Volume::open(&drive) {
            Ok(v) => v,
            Err(e) if e.raw_os_error() == Some(ERROR_ACCESS_DENIED) => {
                eprintln!("{NEEDS_ADMIN}");
                return ExitCode::from(3);
            }
            Err(e) => {
                eprintln!("cannot open {drive}: {e}");
                return ExitCode::from(2);
            }
        };
        let (journal, created) = match vol.query_journal() {
            Ok(j) => j,
            Err(e) => {
                eprintln!("cannot read the change journal of {drive}: {e}");
                return ExitCode::from(2);
            }
        };
        if created {
            println!("created a change journal on {drive}");
        }
        let root = match volume::root_frn(&drive) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("cannot read the root of {drive}: {e}");
                return ExitCode::from(2);
            }
        };
        let t = Instant::now();
        let mut records = Vec::new();
        if let Err(e) = vol.enum_mft(journal.next_usn, |r| records.push(r)) {
            eprintln!("MFT enumeration of {drive} failed: {e}");
            return ExitCode::from(2);
        }
        let read = t.elapsed();
        let with_time = records.iter().filter(|r| r.time != 0).count();
        let n = records.len();
        let mut ix = Index::new();
        let t2 = Instant::now();
        let mut v = VolumeIndex::build(&mut ix, &drive, root, records);
        let build = t2.elapsed();
        let total = t.elapsed().as_secs_f64();
        println!(
            "volume {drive}: serial {:08X}, journal {:#x}, next USN {}",
            vol.serial, journal.id, journal.next_usn
        );
        println!("records: {n} ({with_time} with a timestamp)");
        println!("entries: {}", ix.live_len());
        println!(
            "time: {total:.2} s (MFT read {:.2} s, index build {:.2} s) = {:.2} s per 1 M entries",
            read.as_secs_f64(),
            build.as_secs_f64(),
            total / (ix.live_len().max(1) as f64 / 1e6)
        );
        println!("index heap: {:.0} MiB", ix.heap_bytes() as f64 / 1048576.0);
        if let Some(p) = snap {
            let s = snapshot::capture(&v, &ix, &drive, vol.serial, journal.id, journal.next_usn);
            match snapshot::save_atomic(p, &snapshot::encode(&[s])) {
                Ok(()) => println!("snapshot saved: {}", p.display()),
                Err(e) => eprintln!("snapshot not saved ({}): {e}", p.display()),
            }
        }
        if let Some(secs) = follow {
            println!("following the change journal for {secs} s");
            let end = Instant::now() + Duration::from_secs(secs);
            let mut from = journal.next_usn;
            while Instant::now() < end {
                let (next, recs) = match vol.read_journal(from, journal.id, false) {
                    Ok(x) => x,
                    Err(e) => {
                        eprintln!("journal read failed: {e}");
                        return ExitCode::from(2);
                    }
                };
                for r in &recs {
                    v.apply(&mut ix, r);
                    let what = if r.reason & USN_REASON_FILE_DELETE != 0 {
                        "deleted"
                    } else if r.reason & USN_REASON_RENAME_NEW_NAME != 0 {
                        "renamed"
                    } else if r.reason & USN_REASON_FILE_CREATE != 0 {
                        "created"
                    } else {
                        continue;
                    };
                    let path = match v.id(r.frn) {
                        Some(id) => ix.path(id),
                        None => r.name.clone(),
                    };
                    println!("  {what:8} {path}");
                }
                if next == from {
                    std::thread::sleep(Duration::from_millis(200));
                }
                from = next;
            }
        }
        ExitCode::SUCCESS
    }
}
