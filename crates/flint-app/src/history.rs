//! What the user opened through Flint, as frecency weights for ranking.
//! Stored only in %APPDATA%\Flint\history.json;
//! "Clear history" empties it.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

/// Opens kept per file: older ones add almost nothing.
const KEEP_OPENS: usize = 10;
/// An open counts half after this many days.
const HALF_LIFE_DAYS: f32 = 14.0;
/// Boosts sent with each query, at most.
pub const MAX_BOOSTS: usize = 300;
/// A file counts as "opened often" from this share of the most-used
/// file's weight.
pub const OFTEN_WEIGHT: f32 = 0.25;

/// For each hit: is it one of the files the user opens often?
pub fn often(hits: &[flint_indexer::protocol::WireHit], boosts: &[(String, f32)]) -> Vec<bool> {
    let set: std::collections::HashSet<String> = boosts
        .iter()
        .filter(|(_, w)| *w >= OFTEN_WEIGHT)
        .map(|(p, _)| key(p))
        .collect();
    hits.iter().map(|h| set.contains(&key(&h.path))).collect()
}
const FORMAT_VERSION: u32 = 1;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
struct Entry {
    /// As last opened (original case).
    path: String,
    /// Unix seconds, newest last.
    opens: Vec<u32>,
}

#[derive(Serialize, Deserialize)]
struct File {
    version: u32,
    entries: Vec<Entry>,
    /// Windows' recent items up to this time (Unix seconds) were already
    /// taken in, or cleared away: seeding skips them.
    #[serde(default)]
    seeded_until: u32,
}

pub struct History {
    file: PathBuf,
    /// Keyed by the lowercased path: Windows paths are case-insensitive.
    entries: HashMap<String, Entry>,
    seeded_until: u32,
}

fn key(path: &str) -> String {
    path.to_lowercase()
}

impl History {
    /// `%APPDATA%\Flint\history.json`.
    pub fn default_file() -> Option<PathBuf> {
        std::env::var_os("APPDATA").map(|d| PathBuf::from(d).join("Flint").join("history.json"))
    }

    /// Loads `file`; a missing file is an empty history, a damaged one too
    /// (logged, and replaced on the next save).
    pub fn load(file: &Path) -> History {
        let mut h = History {
            file: file.to_path_buf(),
            entries: HashMap::new(),
            seeded_until: 0,
        };
        let bytes = match std::fs::read(file) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return h,
            Err(e) => {
                crate::log::line(format!("history not read ({}): {e}", file.display()));
                return h;
            }
        };
        match serde_json::from_slice::<File>(&bytes) {
            Ok(f) if f.version == FORMAT_VERSION => {
                h.seeded_until = f.seeded_until;
                for e in f.entries {
                    h.entries.insert(key(&e.path), e);
                }
            }
            Ok(f) => crate::log::line(format!(
                "history version {} not understood; starting empty",
                f.version
            )),
            Err(e) => crate::log::line(format!("history damaged ({e}); starting empty")),
        }
        h
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn record_open(&mut self, path: &str, now: u32) {
        let e = self.entries.entry(key(path)).or_insert_with(|| Entry {
            path: path.to_string(),
            opens: Vec::new(),
        });
        e.path = path.to_string();
        e.opens.push(now);
        e.opens.sort_unstable();
        if e.opens.len() > KEEP_OPENS {
            e.opens.drain(..e.opens.len() - KEEP_OPENS);
        }
    }

    /// Adds Windows' recent items as one open each at the shortcut's time.
    /// Items from before the last seeding or the last Clear are skipped, so
    /// a restart neither counts them twice nor brings back a cleared history.
    pub fn seed(&mut self, items: &[(String, u32)]) {
        let since = self.seeded_until;
        for (path, t) in items {
            if *t <= since {
                continue;
            }
            let known = self
                .entries
                .get(&key(path))
                .is_some_and(|e| e.opens.contains(t));
            if !known {
                self.record_open(path, *t);
            }
            self.seeded_until = self.seeded_until.max(*t);
        }
    }

    pub fn forget(&mut self, path: &str) {
        self.entries.remove(&key(path));
    }

    /// Empties the history; Windows' recent items up to `now` stay out of it.
    pub fn clear(&mut self, now: u32) {
        self.entries.clear();
        self.seeded_until = self.seeded_until.max(now);
    }

    /// Weights in (0, 1], best first, at most `max`: each open counts
    /// `0.5^(age_days / 14)`, the sum normalised by the largest.
    pub fn boosts(&self, now: u32, max: usize) -> Vec<(String, f32)> {
        let mut w: Vec<(String, f32)> = self
            .entries
            .values()
            .map(|e| {
                let s: f32 = e
                    .opens
                    .iter()
                    .map(|&t| {
                        let days = now.saturating_sub(t) as f32 / 86_400.0;
                        0.5f32.powf(days / HALF_LIFE_DAYS)
                    })
                    .sum();
                (e.path.clone(), s)
            })
            .filter(|(_, s)| *s > 0.0)
            .collect();
        w.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        w.truncate(max);
        let top = w.first().map_or(1.0, |x| x.1);
        for x in &mut w {
            x.1 /= top;
        }
        w
    }

    /// Writes a temporary file and renames it over the old one: a failed
    /// write leaves the previous history intact.
    pub fn save(&self) -> io::Result<()> {
        if let Some(d) = self.file.parent() {
            std::fs::create_dir_all(d)?;
        }
        let mut entries: Vec<Entry> = self.entries.values().cloned().collect();
        entries.sort_by(|a, b| a.path.cmp(&b.path));
        let bytes = serde_json::to_vec(&File {
            version: FORMAT_VERSION,
            entries,
            seeded_until: self.seeded_until,
        })
        .map_err(io::Error::other)?;
        let tmp = self.file.with_extension("json.tmp");
        let r = std::fs::write(&tmp, &bytes).and_then(|()| std::fs::rename(&tmp, &self.file));
        if r.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        r
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: u32 = 86_400;
    const NOW: u32 = 1_800_000_000;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("flint-history-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn weight(h: &History, path: &str) -> f32 {
        h.boosts(NOW, MAX_BOOSTS)
            .into_iter()
            .find(|(p, _)| p.eq_ignore_ascii_case(path))
            .map_or(0.0, |x| x.1)
    }

    #[test]
    fn recent_open_outranks_old_frequent() {
        let mut h = History::load(&tmp("rank").join("h.json"));
        h.record_open(r"C:\new.txt", NOW - 3600);
        for i in 0..5 {
            h.record_open(r"C:\old.txt", NOW - 90 * DAY - i);
        }
        assert!(weight(&h, r"C:\new.txt") > weight(&h, r"C:\old.txt"));
        // and frequency still counts among equally recent files
        h.record_open(r"C:\often.txt", NOW - 3600);
        h.record_open(r"C:\often.txt", NOW - 7200);
        assert!(weight(&h, r"C:\often.txt") > weight(&h, r"C:\new.txt"));
    }

    #[test]
    fn weights_normalised_and_capped() {
        let mut h = History::load(&tmp("cap").join("h.json"));
        for i in 0..400u32 {
            h.record_open(&format!(r"C:\f{i}.txt"), NOW - i * 3600);
        }
        for _ in 0..30 {
            h.record_open(r"C:\f0.txt", NOW);
        }
        let b = h.boosts(NOW, MAX_BOOSTS);
        assert_eq!(b.len(), MAX_BOOSTS);
        assert_eq!(b[0], (r"C:\f0.txt".to_string(), 1.0));
        assert!(b.iter().all(|(_, w)| *w > 0.0 && *w <= 1.0));
        assert!(b.windows(2).all(|x| x[0].1 >= x[1].1), "best first");
        // only the last 10 opens of a file count
        assert_eq!(h.entries[&key(r"C:\f0.txt")].opens.len(), KEEP_OPENS);
    }

    #[test]
    fn damaged_file_is_empty_not_fatal() {
        let f = tmp("damaged").join("h.json");
        std::fs::write(&f, b"{ not json").unwrap();
        assert_eq!(History::load(&f).len(), 0);
        std::fs::write(
            &f,
            br#"{"version":99,"entries":[{"path":"C:\\a.txt","opens":[1]}]}"#,
        )
        .unwrap();
        assert_eq!(History::load(&f).len(), 0);
        assert_eq!(History::load(&f.with_file_name("missing.json")).len(), 0);
    }

    #[test]
    fn case_insensitive_keys() {
        let mut h = History::load(&tmp("case").join("h.json"));
        h.record_open(r"C:\Users\x\Rapor.docx", NOW - 10);
        h.record_open(r"c:\users\X\RAPOR.DOCX", NOW);
        assert_eq!(h.len(), 1);
        assert_eq!(
            h.boosts(NOW, 10)[0].0,
            r"c:\users\X\RAPOR.DOCX",
            "kept as last opened"
        );
        h.forget(r"C:\USERS\x\rapor.docx");
        assert_eq!(h.len(), 0);
    }

    #[test]
    fn save_is_atomic() {
        let d = tmp("save");
        let f = d.join("h.json");
        let mut h = History::load(&f);
        h.record_open(r"C:\a.txt", NOW);
        h.save().unwrap();
        assert!(
            !f.with_extension("json.tmp").exists(),
            "no temporary file left"
        );
        let back = History::load(&f);
        assert_eq!(back.boosts(NOW, 10), h.boosts(NOW, 10));
        // a write that fails (the temporary name is taken by a folder)
        // leaves the saved history as it was
        std::fs::create_dir(f.with_extension("json.tmp")).unwrap();
        h.record_open(r"C:\b.txt", NOW);
        assert!(h.save().is_err());
        assert_eq!(History::load(&f).len(), 1);
    }

    #[test]
    fn clear_empties_and_saves_empty() {
        let f = tmp("clear").join("h.json");
        let mut h = History::load(&f);
        h.record_open(r"C:\a.txt", NOW);
        h.save().unwrap();
        h.clear(NOW);
        h.save().unwrap();
        assert_eq!(History::load(&f).len(), 0);
    }

    #[test]
    fn clear_is_not_undone_by_the_next_start() {
        let f = tmp("clear-seed").join("h.json");
        let mut h = History::load(&f);
        h.seed(&[(r"C:\a.txt".to_string(), NOW - DAY)]);
        // opened in Explorer during the session, after that seeding
        let before_clear = (r"C:\c.txt".to_string(), NOW - 60);
        h.clear(NOW);
        h.save().unwrap();
        // the next start reads Windows' recent items again: the ones from
        // before Clear stay out, ones opened since come in
        let mut h = History::load(&f);
        h.seed(&[before_clear, (r"C:\b.txt".to_string(), NOW + 60)]);
        assert_eq!(h.len(), 1);
        assert!(h.entries.contains_key(&key(r"C:\b.txt")));
    }

    #[test]
    fn often_marks_heavily_used_files_only() {
        let hit = |p: &str| flint_indexer::protocol::WireHit {
            path: p.into(),
            name: String::new(),
            is_dir: false,
            typo: false,
            highlights: vec![],
            last_change: 0,
            score: 0,
        };
        let boosts = vec![
            (r"C:\A.txt".to_string(), 1.0),
            (r"C:\b.txt".to_string(), 0.1),
        ];
        let hits = [hit(r"c:\a.TXT"), hit(r"C:\b.txt"), hit(r"C:\c.txt")];
        assert_eq!(often(&hits, &boosts), [true, false, false]);
    }

    #[test]
    fn seeding_counts_each_recent_item_once() {
        let mut h = History::load(&tmp("seed").join("h.json"));
        let items = vec![
            (r"C:\a.txt".to_string(), NOW - DAY),
            (r"C:\b.txt".to_string(), NOW - 2 * DAY),
        ];
        h.seed(&items);
        h.seed(&items); // the next start reads the same shortcuts
        assert_eq!(h.entries[&key(r"C:\a.txt")].opens.len(), 1);
        assert_eq!(h.len(), 2);
    }
}
