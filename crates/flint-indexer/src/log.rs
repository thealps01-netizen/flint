//! Minimal append-only log (spec: every swallowed error writes a line the
//! user can find). `init` points it at a file; before that, lines go to
//! stderr.

use std::io::Write;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

static FILE: OnceLock<Mutex<std::fs::File>> = OnceLock::new();

/// Opens (appending) the log file, creating its folder. Later calls keep
/// the first file.
pub fn init(path: &Path) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    let _ = FILE.set(Mutex::new(f));
    Ok(())
}

/// Writes one timestamped line.
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
