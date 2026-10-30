//! Flint indexer: fills a `flint_core::Index` from local drives, keeps it
//! live, saves it for warm starts and serves queries over a named pipe.
//!
//! Pure modules (no OS calls): `usn`, `volindex`, `snapshot` (file I/O only).

pub mod access;
pub mod cut;
#[cfg(windows)]
pub mod engine;
#[cfg(windows)]
pub mod folders;
pub mod log;
pub mod ntfs;
#[cfg(windows)]
pub mod pipe;
pub mod privacy;
pub mod protocol;
pub mod server;
#[cfg(windows)]
pub mod service;
pub mod snapshot;
pub mod usn;
pub mod volindex;
pub mod volume;
pub mod walk;
pub mod watch;
