//! The `FlintIndexer` Windows service: install, uninstall,
//! and the service entry point the Service Control Manager runs.

use crate::engine::{Config, Engine, service_data_dir};
use crate::log;
use std::ffi::{OsStr, OsString};
use std::io;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};
use windows_service::service::{
    ServiceAccess, ServiceAction, ServiceActionType, ServiceControl, ServiceControlAccept,
    ServiceErrorControl, ServiceExitCode, ServiceFailureActions, ServiceFailureResetPeriod,
    ServiceInfo, ServiceStartType, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
use windows_service::{define_windows_service, service_dispatcher};

pub const SERVICE_NAME: &str = "FlintIndexer";
pub const DISPLAY_NAME: &str = "Flint Indexer";
const DESCRIPTION: &str = "Keeps the Flint file-name index of the local NTFS drives up to date \
     and answers searches from Flint for each signed-in user, showing every user only the files \
     they may see.";
/// The argument the service is launched with.
pub const RUN_ARG: &str = "service-run";
/// `ERROR_SERVICE_EXISTS`.
const SERVICE_EXISTS: i32 = 1073;
/// `ERROR_SERVICE_DOES_NOT_EXIST`.
const SERVICE_MISSING: i32 = 1060;

/// Where the service's program is installed: `%ProgramFiles%\Flint`.
/// The service runs as SYSTEM, so its program must live where only
/// administrators can write (a copy in the user's build folder could be
/// replaced by that user).
pub fn install_dir() -> io::Result<PathBuf> {
    Ok(crate::folders::program_files()?.join("Flint"))
}

fn err(e: windows_service::Error) -> io::Error {
    match e {
        windows_service::Error::Winapi(e) => e,
        other => io::Error::other(other.to_string()),
    }
}

/// Copies this program to [`install_dir`], registers the service
/// (LocalSystem, delayed automatic start, restart after 5 s twice) and
/// starts it. Needs administrator rights.
pub fn install() -> io::Result<()> {
    let exe = std::env::current_exe()?;
    let dir = install_dir()?;
    std::fs::create_dir_all(&dir)?;
    let target = dir.join("flint-indexer.exe");
    let manager = ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
    )
    .map_err(err)?;
    if !same_file(&exe, &target) {
        // installing again (an update, or a new install folder): the
        // running service holds its program open, so it stops first and
        // starts again on the new copy below
        if let Ok(running) = manager.open_service(
            SERVICE_NAME,
            ServiceAccess::QUERY_STATUS | ServiceAccess::STOP,
        ) {
            stop_and_wait(&running)?;
        }
        std::fs::copy(&exe, &target)?;
    }
    let info = ServiceInfo {
        name: OsString::from(SERVICE_NAME),
        display_name: OsString::from(DISPLAY_NAME),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: target.clone(),
        launch_arguments: vec![OsString::from(RUN_ARG)],
        dependencies: vec![],
        account_name: None, // LocalSystem
        account_password: None,
    };
    let access = ServiceAccess::CHANGE_CONFIG | ServiceAccess::START | ServiceAccess::QUERY_STATUS;
    let service = match manager.create_service(&info, access) {
        Ok(s) => s,
        Err(windows_service::Error::Winapi(e)) if e.raw_os_error() == Some(SERVICE_EXISTS) => {
            let s = manager.open_service(SERVICE_NAME, access).map_err(err)?;
            s.change_config(&info).map_err(err)?;
            s
        }
        Err(e) => return Err(err(e)),
    };
    service.set_description(DESCRIPTION).map_err(err)?;
    service.set_delayed_auto_start(true).map_err(err)?;
    service
        .update_failure_actions(ServiceFailureActions {
            reset_period: ServiceFailureResetPeriod::After(Duration::from_secs(24 * 3600)),
            reboot_msg: None,
            command: None,
            actions: Some(vec![
                ServiceAction {
                    action_type: ServiceActionType::Restart,
                    delay: Duration::from_secs(5),
                },
                ServiceAction {
                    action_type: ServiceActionType::Restart,
                    delay: Duration::from_secs(5),
                },
                ServiceAction {
                    action_type: ServiceActionType::None,
                    delay: Duration::ZERO,
                },
            ]),
        })
        .map_err(err)?;
    service
        .set_failure_actions_on_non_crash_failures(true)
        .map_err(err)?;
    if service.query_status().map_err(err)?.current_state != ServiceState::Running {
        service.start(&[] as &[&OsStr]).map_err(err)?;
    }
    println!(
        "installed {SERVICE_NAME} ({}) and started it",
        target.display()
    );
    Ok(())
}

/// What uninstalling the service does with [`install_dir`].
#[derive(Debug, PartialEq, Eq)]
pub enum DirRemoval {
    /// The folder also holds the app (`flint.exe`): the app's own
    /// uninstaller removes it, not the service's.
    Keep,
    Remove,
}

pub fn program_dir_removal(dir: &std::path::Path) -> DirRemoval {
    if dir.join("flint.exe").is_file() {
        DirRemoval::Keep
    } else {
        DirRemoval::Remove
    }
}

/// Asks the service to stop and waits (15 s at most) until it has.
fn stop_and_wait(service: &windows_service::service::Service) -> io::Result<()> {
    if service.query_status().map_err(err)?.current_state != ServiceState::Stopped {
        let _ = service.stop();
    }
    let end = Instant::now() + Duration::from_secs(15);
    while service.query_status().map_err(err)?.current_state != ServiceState::Stopped {
        if Instant::now() > end {
            return Err(io::Error::other("the service did not stop within 15 s"));
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    Ok(())
}

/// Stops the service if it is installed (the installer calls this before
/// it replaces the service's program). Needs administrator rights.
pub fn stop() -> io::Result<()> {
    let manager =
        ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT).map_err(err)?;
    match manager.open_service(
        SERVICE_NAME,
        ServiceAccess::QUERY_STATUS | ServiceAccess::STOP,
    ) {
        Ok(service) => {
            stop_and_wait(&service)?;
            println!("{SERVICE_NAME} is stopped");
            Ok(())
        }
        Err(windows_service::Error::Winapi(e)) if e.raw_os_error() == Some(SERVICE_MISSING) => {
            println!("{SERVICE_NAME} is not installed");
            Ok(())
        }
        Err(e) => Err(err(e)),
    }
}

fn same_file(a: &std::path::Path, b: &std::path::Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// Stops and deletes the service, then removes `%ProgramData%\Flint`
/// (the snapshots and the log) and `%ProgramFiles%\Flint` unless the app
/// is installed there too (see [`program_dir_removal`]).
pub fn uninstall() -> io::Result<()> {
    let manager =
        ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT).map_err(err)?;
    let access = ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE;
    match manager.open_service(SERVICE_NAME, access) {
        Ok(service) => {
            // marked for removal first: Windows removes it once it has
            // stopped, even if that takes longer than the wait below
            service.delete().map_err(err)?;
            stop_and_wait(&service)?;
            drop(service);
            println!("stopped and removed {SERVICE_NAME}");
        }
        Err(windows_service::Error::Winapi(e)) if e.raw_os_error() == Some(SERVICE_MISSING) => {
            println!("{SERVICE_NAME} is not installed");
        }
        Err(e) => return Err(err(e)),
    }
    let data = crate::folders::program_data()?.join("Flint");
    remove_dir(&data)?;
    let prog = install_dir()?;
    let exe = std::env::current_exe()?;
    if program_dir_removal(&prog) == DirRemoval::Keep {
        println!(
            "{} also holds the app: its uninstaller removes it",
            prog.display()
        );
    } else if exe
        .canonicalize()
        .ok()
        .is_some_and(|e| prog.canonicalize().is_ok_and(|p| e.starts_with(p)))
    {
        println!(
            "{} is in use by this program: delete it after it exits",
            prog.display()
        );
    } else {
        // the SCM may hold the program open for a moment after the stop
        let end = Instant::now() + Duration::from_secs(10);
        loop {
            match remove_dir(&prog) {
                Ok(()) => break,
                Err(e) if Instant::now() < end => {
                    let _ = e;
                    std::thread::sleep(Duration::from_millis(300));
                }
                Err(e) => return Err(e),
            }
        }
    }
    Ok(())
}

fn remove_dir(p: &std::path::Path) -> io::Result<()> {
    match std::fs::remove_dir_all(p) {
        Ok(()) => {
            println!("removed {}", p.display());
            Ok(())
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(io::Error::new(e.kind(), format!("{}: {e}", p.display()))),
    }
}

define_windows_service!(ffi_service_main, service_main);

/// Hands this process to the Service Control Manager (blocks until the
/// service stops). Only works when the SCM started the process.
pub fn run() -> io::Result<()> {
    service_dispatcher::start(SERVICE_NAME, ffi_service_main).map_err(err)
}

fn service_main(_args: Vec<OsString>) {
    if let Err(e) = run_service() {
        log::line(format!("service ended with an error: {e}"));
    }
}

fn run_service() -> io::Result<()> {
    let (tx, rx) = mpsc::channel();
    let handle = service_control_handler::register(SERVICE_NAME, move |c| match c {
        ServiceControl::Stop | ServiceControl::Shutdown => {
            let _ = tx.send(());
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        _ => ServiceControlHandlerResult::NotImplemented,
    })
    .map_err(err)?;
    let status = |state, accept, wait: u64, code: u32| ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: state,
        controls_accepted: accept,
        exit_code: ServiceExitCode::Win32(code),
        checkpoint: 0,
        wait_hint: Duration::from_secs(wait),
        process_id: None,
    };
    let none = ServiceControlAccept::empty();
    handle
        .set_service_status(status(ServiceState::StartPending, none, 30, 0))
        .map_err(err)?;
    let started = service_data_dir().and_then(|dir| {
        let _ = log::init(&dir.join("indexer.log"));
        log::line("service starting");
        Engine::start(Config::service(dir))
    });
    let engine = match started {
        Ok(e) => e,
        Err(e) => {
            log::line(format!("service could not start: {e}"));
            handle
                .set_service_status(status(ServiceState::Stopped, none, 0, 1))
                .map_err(err)?;
            return Err(e);
        }
    };
    let accept = ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN;
    handle
        .set_service_status(status(ServiceState::Running, accept, 0, 0))
        .map_err(err)?;
    let _ = rx.recv();
    log::line("service stopping");
    handle
        .set_service_status(status(ServiceState::StopPending, none, 10, 0))
        .map_err(err)?;
    let t = Instant::now();
    engine.stop();
    log::line(format!(
        "service stopped in {:.2} s",
        t.elapsed().as_secs_f64()
    ));
    handle
        .set_service_status(status(ServiceState::Stopped, none, 0, 0))
        .map_err(err)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("flint-svc-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn install_dir_ignores_the_environment() {
        // a user can set their own %ProgramFiles%; the elevated installer
        // must still use the real, administrators-only folder
        let real = install_dir().unwrap();
        let before = std::env::var_os("ProgramFiles");
        // SAFETY: no other test in this crate reads ProgramFiles
        unsafe { std::env::set_var("ProgramFiles", std::env::temp_dir()) };
        let tricked = install_dir();
        match before {
            Some(v) => unsafe { std::env::set_var("ProgramFiles", v) },
            None => unsafe { std::env::remove_var("ProgramFiles") },
        }
        assert_eq!(tricked.unwrap(), real);
        assert!(real.ends_with("Flint"));
    }

    #[test]
    fn uninstall_keeps_a_folder_that_holds_the_app() {
        // Program Files\Flint is also where the installer puts the app:
        // removing the service (from Settings, or a development build)
        // must not delete the installed app
        let d = tmp("keep");
        std::fs::write(d.join("flint.exe"), b"").unwrap();
        std::fs::write(d.join("flint-indexer.exe"), b"").unwrap();
        assert_eq!(program_dir_removal(&d), DirRemoval::Keep);
        std::fs::remove_file(d.join("flint.exe")).unwrap();
        assert_eq!(program_dir_removal(&d), DirRemoval::Remove);
        std::fs::remove_file(d.join("flint-indexer.exe")).unwrap();
        assert_eq!(program_dir_removal(&d), DirRemoval::Remove);
        assert_eq!(program_dir_removal(&d.join("absent")), DirRemoval::Remove);
        let _ = std::fs::remove_dir_all(&d);
    }
}
