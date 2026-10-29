//! Log file in %LOCALAPPDATA%\Flint\logs: every swallowed error
//! writes a line here.

use std::io::Write;
use std::sync::{Mutex, OnceLock};

static FILE: OnceLock<Mutex<std::fs::File>> = OnceLock::new();

pub fn dir() -> Option<std::path::PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|d| std::path::PathBuf::from(d).join("Flint").join("logs"))
}

pub fn init() {
    let Some(d) = dir() else { return };
    if std::fs::create_dir_all(&d).is_err() {
        return;
    }
    if let Ok(f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(d.join("flint.log"))
    {
        let _ = FILE.set(Mutex::new(f));
    }
    // the in-process indexers log on their own (walk and watch errors)
    if let Err(e) = flint_indexer::log::init(&d.join("indexer.log")) {
        line(format!("indexer log not opened: {e}"));
    }
}

pub fn line(msg: impl AsRef<str>) {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let text = format!("{secs} {}\n", msg.as_ref());
    match FILE.get() {
        Some(f) => {
            if let Ok(mut f) = f.lock() {
                let _ = f.write_all(text.as_bytes());
            }
        }
        None => eprint!("{text}"),
    }
}
