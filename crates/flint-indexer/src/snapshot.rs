//! Versioned on-disk snapshot of indexed volumes, for a warm start without
//! a rescan (warm start ≤ 2 s).
//!
//! Format, little-endian: magic `FLNTIDX\0`, `u32` version, `u64` payload
//! length, `u64` FNV-1a-64 of the payload, then the payload: `u32` volume
//! count, and per volume `drive` (u16 length + UTF-8), `u32` serial, `u64`
//! journal id, `i64` next USN, `u64` root FRN, `u64` record count, records
//! (`u64` frn, `u64` parent, `u32` time, `u32` attrs: folder bit plus
//! [`ATTR_NO_LIST`]/[`ATTR_UNTRUSTED`], u16 length + UTF-8 name). A file that fails any check is rejected whole, never trusted.

use crate::usn::{FILE_ATTRIBUTE_DIRECTORY, UsnRecord};
use crate::volindex::VolumeIndex;
use flint_core::{EntryId, Index, Restriction};
use std::io::{self, Write};
use std::path::Path;

pub const MAGIC: &[u8; 8] = b"FLNTIDX\0";
/// 2: folder restrictions in `attrs` ([`ATTR_NO_LIST`], [`ATTR_UNTRUSTED`]).
pub const SNAPSHOT_VERSION: u32 = 2;

/// Record `attrs` bit (unused by Windows): the folder is
/// [`flint_core::Restriction::NoList`].
pub const ATTR_NO_LIST: u32 = 0x8000_0000;
/// Record `attrs` bit: the folder is [`flint_core::Restriction::Untrusted`].
pub const ATTR_UNTRUSTED: u32 = 0x4000_0000;
const HEADER: usize = 8 + 4 + 8 + 8;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VolumeSnapshot {
    pub drive: String,
    pub volume_serial: u32,
    pub journal_id: u64,
    /// The journal position the snapshot is complete up to.
    pub next_usn: i64,
    pub root_frn: u64,
    /// Only `frn`, `parent`, `time`, `attrs` (directory bit) and `name`
    /// are kept; `usn` and `reason` are zero.
    pub records: Vec<UsnRecord>,
    /// What the volume leaves out (kept in the index image, not in the
    /// user-mode snapshot file, whose volumes leave nothing out).
    pub cut: crate::cut::Cut,
}

#[derive(Debug)]
pub enum SnapshotError {
    Io(io::Error),
    BadMagic,
    Version(u32),
    Checksum,
    Truncated,
    BadName,
}

impl std::fmt::Display for SnapshotError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "cannot read the snapshot: {e}"),
            Self::BadMagic => f.write_str("not a Flint snapshot"),
            Self::Version(v) => write!(f, "snapshot version {v}, expected {SNAPSHOT_VERSION}"),
            Self::Checksum => f.write_str("snapshot checksum mismatch (damaged file)"),
            Self::Truncated => f.write_str("snapshot is truncated"),
            Self::BadName => f.write_str("snapshot holds a name that is not UTF-8"),
        }
    }
}

impl std::error::Error for SnapshotError {}

/// Magic of an index image file (the service's warm start).
pub const IMAGE_MAGIC: &[u8; 8] = b"FLNTIMG\0";
/// 2: each volume also keeps what it leaves out (`crate::cut`).
/// 3: one name text, a name equal to its folded form stored once (2 is
/// still read: see [`flint_core::Index::from_image_v2`]).
pub const IMAGE_VERSION: u32 = 3;

/// A volume's journal position and FRN map in an image.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VolumeImage {
    pub drive: String,
    pub volume_serial: u32,
    pub journal_id: u64,
    pub next_usn: i64,
    pub parts: crate::volindex::VolumeParts,
}

/// Fast checksum over 8-byte words (corruption check, not security: the
/// file sits in a folder only SYSTEM and Administrators can write).
fn sum64(b: &[u8]) -> u64 {
    let (words, rest) = b.as_chunks::<8>();
    let mut h: u64 = 0x9e37_79b9_7f4a_7c15 ^ b.len() as u64;
    for w in words {
        h = (h ^ u64::from_le_bytes(*w))
            .wrapping_mul(0x0100_0000_01b3)
            .rotate_left(29);
    }
    for &x in rest {
        h = (h ^ x as u64).wrapping_mul(0x0100_0000_01b3);
    }
    h
}

/// The whole index plus each volume's journal position: restoring it is
/// a copy, not a rebuild (warm start ≤ 2 s on millions of entries).
pub fn encode_image(ix: &Index, vols: &[VolumeImage]) -> Vec<u8> {
    encode_image_as(ix, vols, IMAGE_VERSION)
}

/// [`encode_image`] in an image `version`: 2 writes the index as Flint
/// 0.1.3 did (to test the upgrade).
#[doc(hidden)]
pub fn encode_image_as(ix: &Index, vols: &[VolumeImage], version: u32) -> Vec<u8> {
    assert!(
        version == 2 || version == IMAGE_VERSION,
        "image version {version}"
    );
    let mut p = Vec::with_capacity(ix.heap_bytes() + 64);
    if version == 2 {
        ix.to_image_v2(&mut p);
    } else {
        ix.to_image(&mut p);
    }
    p.extend_from_slice(&(vols.len() as u32).to_le_bytes());
    for v in vols {
        put_str(&mut p, &v.drive);
        p.extend_from_slice(&v.volume_serial.to_le_bytes());
        p.extend_from_slice(&v.journal_id.to_le_bytes());
        p.extend_from_slice(&v.next_usn.to_le_bytes());
        p.extend_from_slice(&v.parts.root_frn.to_le_bytes());
        p.extend_from_slice(&v.parts.root.0.to_le_bytes());
        p.extend_from_slice(&v.parts.orphans.map_or(u32::MAX, |o| o.0).to_le_bytes());
        p.extend_from_slice(&(v.parts.frns.len() as u64).to_le_bytes());
        for &(id, frn) in &v.parts.frns {
            p.extend_from_slice(&id.to_le_bytes());
            p.extend_from_slice(&frn.to_le_bytes());
        }
        p.extend_from_slice(&(v.parts.rules.len() as u32).to_le_bytes());
        for rule in &v.parts.rules {
            p.push(rule.depth);
            p.extend_from_slice(&(rule.path.len() as u32).to_le_bytes());
            for name in &rule.path {
                put_str(&mut p, name);
            }
        }
        p.extend_from_slice(&(v.parts.cut.len() as u64).to_le_bytes());
        for &(frn, depth) in &v.parts.cut {
            p.extend_from_slice(&frn.to_le_bytes());
            p.push(depth);
        }
        p.extend_from_slice(&(v.parts.hidden.len() as u64).to_le_bytes());
        for &frn in &v.parts.hidden {
            p.extend_from_slice(&frn.to_le_bytes());
        }
    }
    let mut out = Vec::with_capacity(p.len() + 28);
    out.extend_from_slice(IMAGE_MAGIC);
    out.extend_from_slice(&version.to_le_bytes());
    out.extend_from_slice(&(p.len() as u64).to_le_bytes());
    out.extend_from_slice(&sum64(&p).to_le_bytes());
    out.extend_from_slice(&p);
    out
}

/// Reads an image written by [`encode_image`]. The index comes back
/// without owner rules (the caller sets them).
pub fn decode_image(bytes: &[u8]) -> Result<(Index, Vec<VolumeImage>), SnapshotError> {
    let mut h = Reader { b: bytes, at: 0 };
    let magic = h.take(8).map_err(|_| SnapshotError::Truncated)?;
    if magic != IMAGE_MAGIC {
        return Err(SnapshotError::BadMagic);
    }
    let version = h.u32()?;
    let read = match version {
        IMAGE_VERSION => Index::from_image,
        2 => Index::from_image_v2,
        _ => return Err(SnapshotError::Version(version)),
    };
    let len = h.u64()?;
    let sum = h.u64()?;
    let payload = h.take(usize::try_from(len).map_err(|_| SnapshotError::Truncated)?)?;
    if h.at != bytes.len() || sum64(payload) != sum {
        return Err(SnapshotError::Checksum);
    }
    let (ix, used) = read(payload).map_err(|e| match e {
        flint_core::ImageError::Truncated => SnapshotError::Truncated,
        flint_core::ImageError::Invalid(_) => SnapshotError::Checksum,
    })?;
    let mut r = Reader {
        b: payload,
        at: used,
    };
    let n = r.u32()?;
    let mut vols = Vec::new();
    for _ in 0..n {
        let drive = r.str()?;
        let volume_serial = r.u32()?;
        let journal_id = r.u64()?;
        let next_usn = r.u64()? as i64;
        let root_frn = r.u64()?;
        let root = EntryId(r.u32()?);
        let orphans = match r.u32()? {
            u32::MAX => None,
            o => Some(EntryId(o)),
        };
        let count = r.u64()?;
        if count > (payload.len() / 12) as u64 {
            return Err(SnapshotError::Truncated);
        }
        let mut frns = Vec::with_capacity(count as usize);
        for _ in 0..count {
            let id = r.u32()?;
            frns.push((id, r.u64()?));
        }
        let mut rules = Vec::new();
        for _ in 0..r.u32()? {
            let depth = r.u8()?;
            if depth as usize > crate::cut::KEEP.len() {
                return Err(SnapshotError::Checksum);
            }
            let n = r.u32()?;
            if n as usize > payload.len() - r.at {
                return Err(SnapshotError::Truncated);
            }
            let mut path = Vec::with_capacity(n as usize);
            for _ in 0..n {
                path.push(r.str()?);
            }
            rules.push(crate::cut::Rule { path, depth });
        }
        let count = r.u64()?;
        if count > ((payload.len() - r.at) / 9) as u64 {
            return Err(SnapshotError::Truncated);
        }
        let mut cut = Vec::with_capacity(count as usize);
        for _ in 0..count {
            let frn = r.u64()?;
            let depth = r.u8()?;
            if depth as usize > crate::cut::KEEP.len() {
                return Err(SnapshotError::Checksum);
            }
            cut.push((frn, depth));
        }
        let count = r.u64()?;
        if count > ((payload.len() - r.at) / 8) as u64 {
            return Err(SnapshotError::Truncated);
        }
        let mut hidden = Vec::with_capacity(count as usize);
        for _ in 0..count {
            hidden.push(r.u64()?);
        }
        vols.push(VolumeImage {
            drive,
            volume_serial,
            journal_id,
            next_usn,
            parts: crate::volindex::VolumeParts {
                root_frn,
                root,
                orphans,
                frns,
                rules,
                cut,
                hidden,
            },
        });
    }
    if r.at != payload.len() {
        return Err(SnapshotError::Checksum);
    }
    Ok((ix, vols))
}

/// The snapshot of one volume as `v` holds it in `ix` now.
pub fn capture(
    v: &VolumeIndex,
    ix: &Index,
    drive: &str,
    volume_serial: u32,
    journal_id: u64,
    next_usn: i64,
) -> VolumeSnapshot {
    let records = v
        .entries(ix)
        .map(|(frn, parent, id)| UsnRecord {
            frn,
            parent,
            usn: 0,
            time: ix.last_change(id),
            reason: 0,
            attrs: attrs_of(ix, id),
            security_id: 0,
            name: ix.name(id).to_string(),
        })
        .collect();
    VolumeSnapshot {
        drive: drive.to_string(),
        volume_serial,
        journal_id,
        next_usn,
        root_frn: v.root_frn(),
        records,
        cut: v.cut().clone(),
    }
}

/// Folder bit and restriction bits of entry `id`.
fn attrs_of(ix: &Index, id: EntryId) -> u32 {
    if !ix.is_dir(id) {
        return 0;
    }
    FILE_ATTRIBUTE_DIRECTORY
        | match ix.restriction(id) {
            Some(Restriction::NoList) => ATTR_NO_LIST,
            Some(Restriction::Untrusted) => ATTR_UNTRUSTED,
            None => 0,
        }
}

/// Snapshots of a walked index (user mode: no FRNs), one per drive root;
/// entry ids stand in for FRNs. Journal fields are zero.
pub fn capture_walk(ix: &Index) -> Vec<VolumeSnapshot> {
    let root_of = |mut id: EntryId| {
        while let Some(p) = ix.parent(id) {
            id = p;
        }
        id
    };
    let mut out: Vec<(EntryId, VolumeSnapshot)> = Vec::new();
    for id in (0..ix.raw_len() as u32).map(EntryId) {
        if !ix.is_live(id) {
            continue;
        }
        let Some(parent) = ix.parent(id) else {
            out.push((
                id,
                VolumeSnapshot {
                    drive: ix.name(id).to_string(),
                    volume_serial: 0,
                    journal_id: 0,
                    next_usn: 0,
                    root_frn: id.0 as u64 + 1,
                    records: Vec::new(),
                    cut: crate::cut::Cut::default(),
                },
            ));
            continue;
        };
        let root = root_of(id);
        let rec = UsnRecord {
            frn: id.0 as u64 + 1,
            parent: parent.0 as u64 + 1,
            usn: 0,
            time: ix.last_change(id),
            reason: 0,
            attrs: attrs_of(ix, id),
            security_id: 0,
            name: ix.name(id).to_string(),
        };
        match out.iter_mut().find(|(r, _)| *r == root) {
            Some((_, s)) => s.records.push(rec),
            None => {
                // a child listed before its root (ids are not ordered by
                // depth after a move): keep it until the root comes
                out.push((
                    root,
                    VolumeSnapshot {
                        drive: ix.name(root).to_string(),
                        volume_serial: 0,
                        journal_id: 0,
                        next_usn: 0,
                        root_frn: root.0 as u64 + 1,
                        records: vec![rec],
                        cut: crate::cut::Cut::default(),
                    },
                ));
            }
        }
    }
    // a root seen after its children was pushed twice: merge
    let mut merged: Vec<(EntryId, VolumeSnapshot)> = Vec::new();
    for (r, s) in out {
        match merged.iter_mut().find(|(m, _)| *m == r) {
            Some((_, m)) => m.records.extend(s.records),
            None => merged.push((r, s)),
        }
    }
    merged.into_iter().map(|(_, s)| s).collect()
}

/// Adds the snapshot's volume to `ix`, with its folder restrictions.
pub fn restore(ix: &mut Index, s: &VolumeSnapshot) -> VolumeIndex {
    restore_owned(ix, s.clone())
}

/// [`restore`] that consumes the snapshot (no copy of its names: the
/// warm start restores millions).
pub fn restore_owned(ix: &mut Index, s: VolumeSnapshot) -> VolumeIndex {
    let marks: Vec<(u64, Restriction)> = s
        .records
        .iter()
        .filter_map(|r| {
            if r.attrs & ATTR_UNTRUSTED != 0 {
                Some((r.frn, Restriction::Untrusted))
            } else if r.attrs & ATTR_NO_LIST != 0 {
                Some((r.frn, Restriction::NoList))
            } else {
                None
            }
        })
        .collect();
    let mut v = VolumeIndex::build(ix, &s.drive, s.root_frn, s.records);
    v.set_cut(s.cut);
    for (frn, mark) in marks {
        if let Some(id) = v.id(frn) {
            ix.set_restriction(id, Some(mark));
        }
    }
    v
}

fn put_str(out: &mut Vec<u8>, s: &str) {
    // NTFS names are at most 255 UTF-16 units, so at most 765 UTF-8 bytes
    let b = s.as_bytes();
    let n = b.len().min(u16::MAX as usize);
    out.extend_from_slice(&(n as u16).to_le_bytes());
    out.extend_from_slice(&b[..n]);
}

pub fn encode(snaps: &[VolumeSnapshot]) -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(&(snaps.len() as u32).to_le_bytes());
    for s in snaps {
        put_str(&mut p, &s.drive);
        p.extend_from_slice(&s.volume_serial.to_le_bytes());
        p.extend_from_slice(&s.journal_id.to_le_bytes());
        p.extend_from_slice(&s.next_usn.to_le_bytes());
        p.extend_from_slice(&s.root_frn.to_le_bytes());
        p.extend_from_slice(&(s.records.len() as u64).to_le_bytes());
        for r in &s.records {
            p.extend_from_slice(&r.frn.to_le_bytes());
            p.extend_from_slice(&r.parent.to_le_bytes());
            p.extend_from_slice(&r.time.to_le_bytes());
            p.extend_from_slice(&r.attrs.to_le_bytes());
            put_str(&mut p, &r.name);
        }
    }
    let mut out = Vec::with_capacity(HEADER + p.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&SNAPSHOT_VERSION.to_le_bytes());
    out.extend_from_slice(&(p.len() as u64).to_le_bytes());
    out.extend_from_slice(&fnv1a(&p).to_le_bytes());
    out.extend_from_slice(&p);
    out
}

/// Bounds-checked little-endian reader over the payload.
struct Reader<'a> {
    b: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], SnapshotError> {
        let end = self.at.checked_add(n).ok_or(SnapshotError::Truncated)?;
        let s = self.b.get(self.at..end).ok_or(SnapshotError::Truncated)?;
        self.at = end;
        Ok(s)
    }
    fn arr<const N: usize>(&mut self) -> Result<[u8; N], SnapshotError> {
        Ok(self.take(N)?.try_into().expect("N bytes"))
    }
    fn u8(&mut self) -> Result<u8, SnapshotError> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16, SnapshotError> {
        self.arr().map(u16::from_le_bytes)
    }
    fn u32(&mut self) -> Result<u32, SnapshotError> {
        self.arr().map(u32::from_le_bytes)
    }
    fn u64(&mut self) -> Result<u64, SnapshotError> {
        self.arr().map(u64::from_le_bytes)
    }
    fn str(&mut self) -> Result<String, SnapshotError> {
        let n = self.u16()? as usize;
        let b = self.take(n)?;
        std::str::from_utf8(b)
            .map(str::to_string)
            .map_err(|_| SnapshotError::BadName)
    }
}

pub fn decode(bytes: &[u8]) -> Result<Vec<VolumeSnapshot>, SnapshotError> {
    let mut h = Reader { b: bytes, at: 0 };
    let magic = h.take(8).map_err(|_| {
        if bytes.is_empty() || MAGIC.starts_with(bytes) {
            SnapshotError::Truncated
        } else {
            SnapshotError::BadMagic
        }
    })?;
    if magic != MAGIC {
        return Err(SnapshotError::BadMagic);
    }
    let version = h.u32()?;
    if version != SNAPSHOT_VERSION {
        return Err(SnapshotError::Version(version));
    }
    let len = h.u64()?;
    let sum = h.u64()?;
    let payload = h.take(usize::try_from(len).map_err(|_| SnapshotError::Truncated)?)?;
    if h.at != bytes.len() {
        return Err(SnapshotError::Checksum); // trailing bytes: not what was written
    }
    if fnv1a(payload) != sum {
        return Err(SnapshotError::Checksum);
    }
    let mut r = Reader { b: payload, at: 0 };
    let volumes = r.u32()?;
    let mut out = Vec::new();
    for _ in 0..volumes {
        let drive = r.str()?;
        let volume_serial = r.u32()?;
        let journal_id = r.u64()?;
        let next_usn = r.u64()? as i64;
        let root_frn = r.u64()?;
        let n = r.u64()?;
        // each record is at least 26 bytes: a lying count fails here, not in an allocation
        let left = (payload.len() - r.at) as u64;
        if n > left / 26 {
            return Err(SnapshotError::Truncated);
        }
        let mut records = Vec::with_capacity(n as usize);
        for _ in 0..n {
            let frn = r.u64()?;
            let parent = r.u64()?;
            let time = r.u32()?;
            let attrs = r.u32()?;
            let name = r.str()?;
            records.push(UsnRecord {
                frn,
                parent,
                usn: 0,
                time,
                reason: 0,
                attrs,
                security_id: 0,
                name,
            });
        }
        out.push(VolumeSnapshot {
            drive,
            volume_serial,
            journal_id,
            next_usn,
            root_frn,
            records,
            cut: crate::cut::Cut::default(),
        });
    }
    if r.at != payload.len() {
        return Err(SnapshotError::Truncated);
    }
    Ok(out)
}

/// Writes `bytes` to `path` through a sibling temp file and a replacing
/// rename, so a crash leaves either the old or the new file, never half.
pub fn save_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = temp_path(path);
    let write = || -> io::Result<()> {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()
    };
    if let Err(e) = write() {
        if tmp.is_file() {
            let _ = std::fs::remove_file(&tmp);
        }
        return Err(e);
    }
    // std's rename replaces an existing file (MoveFileExW REPLACE_EXISTING)
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

pub fn load(path: &Path) -> Result<Vec<VolumeSnapshot>, SnapshotError> {
    decode(&std::fs::read(path).map_err(SnapshotError::Io)?)
}

fn temp_path(path: &Path) -> std::path::PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".tmp");
    path.with_file_name(name)
}

fn fnv1a(b: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &x in b {
        h ^= x as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOT: u64 = 5;

    fn rec(frn: u64, parent: u64, name: &str, dir: bool, time: u32) -> UsnRecord {
        UsnRecord {
            frn,
            parent,
            usn: 0,
            time,
            reason: 0,
            attrs: if dir { FILE_ATTRIBUTE_DIRECTORY } else { 0 },
            security_id: 0,
            name: name.into(),
        }
    }

    /// A volume of `n` entries in a 3-level tree with Turkish names.
    fn volume(n: u64) -> Vec<UsnRecord> {
        let mut r = Vec::new();
        for d in 0..50 {
            r.push(rec(100 + d, ROOT, &format!("Klasör {d}"), true, 0));
            for s in 0..4 {
                r.push(rec(
                    1000 + d * 10 + s,
                    100 + d,
                    &format!("alt-ğüşıöç {s}"),
                    true,
                    7,
                ));
            }
        }
        let mut i = 0;
        while (r.len() as u64) < n {
            r.push(rec(
                100_000 + i,
                1000 + (i % 200) / 4 * 10 + i % 4,
                &format!("Bütçe {i}.xlsx"),
                false,
                1_700_000_000 + i as u32,
            ));
            i += 1;
        }
        r
    }

    fn sorted_paths(ix: &Index) -> Vec<String> {
        let mut p: Vec<String> = (0..ix.raw_len() as u32)
            .map(flint_core::EntryId)
            .filter(|&id| ix.is_live(id))
            .map(|id| format!("{}|{}|{}", ix.path(id), ix.is_dir(id), ix.last_change(id)))
            .collect();
        p.sort();
        p
    }

    #[test]
    fn roundtrip_rebuilds_same_paths() {
        let mut a = Index::new();
        let va = VolumeIndex::build(&mut a, "C:", ROOT, volume(5_000));
        let snap = capture(&va, &a, "C:", 0xABCD, 42, 9_999);
        assert_eq!(snap.records.len(), 5_000);
        let bytes = encode(std::slice::from_ref(&snap));
        let back = decode(&bytes).unwrap();
        assert_eq!(back, vec![snap]);
        let mut b = Index::new();
        let s = &back[0];
        let vb = VolumeIndex::build(&mut b, &s.drive, s.root_frn, s.records.iter().cloned());
        assert_eq!(sorted_paths(&a), sorted_paths(&b));
        assert_eq!(va.len(), vb.len());
        assert_eq!(
            (s.volume_serial, s.journal_id, s.next_usn),
            (0xABCD, 42, 9_999)
        );
    }

    /// A folder's restriction is part of the privacy state, so
    /// a warm start must not show what the scan had hidden; a version-1
    /// file (which had no restrictions) is refused, so it is rescanned.
    #[test]
    fn restrictions_survive_the_roundtrip() {
        use flint_core::Restriction;
        let mut a = Index::new();
        let va = VolumeIndex::build(&mut a, "C:", ROOT, volume(300));
        let d0 = va.id(100).unwrap();
        let d1 = va.id(1001).unwrap();
        a.set_restriction(d0, Some(Restriction::NoList));
        a.set_restriction(d1, Some(Restriction::Untrusted));
        let snap = capture(&va, &a, "C:", 1, 2, 3);
        let back = decode(&encode(std::slice::from_ref(&snap))).unwrap();
        let mut b = Index::new();
        let vb = restore(&mut b, &back[0]);
        assert_eq!(
            b.restriction(vb.id(100).unwrap()),
            Some(Restriction::NoList)
        );
        assert_eq!(
            b.restriction(vb.id(1001).unwrap()),
            Some(Restriction::Untrusted)
        );
        assert_eq!(b.restriction(vb.id(101).unwrap()), None);
        assert!(b.is_dir(vb.id(100).unwrap()));
        assert_eq!(sorted_paths(&a), sorted_paths(&b));
        let mut v1 = encode(&[snap]);
        v1[8..12].copy_from_slice(&1u32.to_le_bytes());
        assert!(matches!(decode(&v1), Err(SnapshotError::Version(1))));
    }

    /// The service's image: the same index and FRN map come back, and the
    /// journal applies on top as before; a flipped byte is refused.
    #[test]
    fn image_roundtrip_and_damage() {
        use flint_core::Restriction;
        let mut a = Index::new();
        let mut va = VolumeIndex::build(&mut a, "C:", ROOT, volume(300));
        a.set_restriction(va.id(100).unwrap(), Some(Restriction::NoList));
        va.apply(&mut a, &rec(5000, 100, "yeni-dosya.txt", false, 1));
        // what the volume leaves out comes back too
        va.set_cut(crate::cut::Cut::from_parts(
            vec![crate::cut::Rule {
                path: vec![
                    "users".into(),
                    "*".into(),
                    "appdata".into(),
                    "roaming".into(),
                ],
                depth: 0,
            }],
            &[(100, 0), (101, 3)],
            &[7000, 7001],
        ));
        let img = VolumeImage {
            drive: "C:".into(),
            volume_serial: 7,
            journal_id: 8,
            next_usn: 9,
            parts: va.parts(),
        };
        let bytes = encode_image(&a, std::slice::from_ref(&img));
        let (mut b, vols) = decode_image(&bytes).unwrap();
        assert_eq!(vols, vec![img.clone()]);
        let mut vb = VolumeIndex::from_parts(&b, vols[0].parts.clone()).unwrap();
        assert_eq!(vb.cut(), va.cut());
        // compaction copies a volume through capture and restore: what it
        // leaves out goes along
        let mut c = Index::new();
        let vc = restore_owned(&mut c, capture(&va, &a, "C:", 1, 2, 3));
        assert_eq!(vc.cut(), va.cut());
        assert_eq!(sorted_paths(&a), sorted_paths(&b));
        assert_eq!(vb.id(5000), va.id(5000));
        assert_eq!(
            b.restriction(vb.id(100).unwrap()),
            Some(Restriction::NoList)
        );
        let mut del = rec(5000, 100, "yeni-dosya.txt", false, 0);
        del.reason = crate::usn::USN_REASON_FILE_DELETE;
        vb.apply(&mut b, &del);
        assert!(vb.id(5000).is_none());
        // damage anywhere in the payload fails the checksum
        let mut bad = bytes.clone();
        let k = bad.len() / 2;
        bad[k] ^= 1;
        assert!(matches!(decode_image(&bad), Err(SnapshotError::Checksum)));
        assert!(matches!(decode(&bytes), Err(SnapshotError::BadMagic)));
        // an image of an older version is refused (read again from the disk)
        let mut old = bytes.clone();
        old[8..12].copy_from_slice(&1u32.to_le_bytes());
        assert!(matches!(decode_image(&old), Err(SnapshotError::Version(1))));
        // parts that do not fit the index are refused
        let mut wrong = img.parts.clone();
        wrong.root = EntryId(b.raw_len() as u32 + 5);
        assert!(VolumeIndex::from_parts(&b, wrong).is_none());
        let mut twice = img.parts.clone();
        let (id, frn) = twice.frns[1];
        twice.frns.push((id + 1, frn)); // one FRN on two entries
        assert!(VolumeIndex::from_parts(&b, img.parts.clone()).is_some());
        assert!(VolumeIndex::from_parts(&b, twice).is_none());
    }

    /// The image Flint 0.1.3 saved (version 2) still loads after an
    /// update: a warm start, not a scan from scratch.
    #[test]
    fn an_image_from_0_1_3_still_loads() {
        let mut a = Index::new();
        let mut va = VolumeIndex::build(&mut a, "C:", ROOT, volume(300));
        va.apply(&mut a, &rec(5000, 100, "Yeni Dosya.txt", false, 1));
        let img = VolumeImage {
            drive: "C:".into(),
            volume_serial: 7,
            journal_id: 8,
            next_usn: 9,
            parts: va.parts(),
        };
        let old = encode_image_as(&a, std::slice::from_ref(&img), 2);
        let (b, vols) = decode_image(&old).unwrap();
        assert_eq!(vols, vec![img]);
        assert_eq!(sorted_paths(&a), sorted_paths(&b));
        let vb = VolumeIndex::from_parts(&b, vols[0].parts.clone()).unwrap();
        assert_eq!(vb.id(5000).map(|id| b.folded(id)), Some("yeni dosya.txt"));
        // and is saved again in the current layout
        let new = encode_image(&b, &vols);
        assert_eq!(new[8..12], IMAGE_VERSION.to_le_bytes());
        assert!(new.len() < old.len());
    }

    /// A walked tree (no FRNs) is saved with entry ids as FRNs and comes
    /// back with the same paths.
    #[test]
    fn walked_tree_roundtrip() {
        use flint_core::EntryInput;
        let mut a = Index::new();
        let c = a.insert(EntryInput {
            name: "C:",
            parent: None,
            is_dir: true,
            last_change: 0,
        });
        let u = a.insert(EntryInput {
            name: "Users",
            parent: Some(c),
            is_dir: true,
            last_change: 1,
        });
        let x = a.insert(EntryInput {
            name: "Bütçe.xlsx",
            parent: Some(u),
            is_dir: false,
            last_change: 9,
        });
        let gone = a.insert(EntryInput {
            name: "silinen.txt",
            parent: Some(u),
            is_dir: false,
            last_change: 0,
        });
        a.remove(gone);
        let d = a.insert(EntryInput {
            name: "D:",
            parent: None,
            is_dir: true,
            last_change: 0,
        });
        a.insert(EntryInput {
            name: "veri",
            parent: Some(d),
            is_dir: true,
            last_change: 0,
        });
        let _ = x;
        let snaps = capture_walk(&a);
        assert_eq!(snaps.len(), 2, "one per drive");
        let back = decode(&encode(&snaps)).unwrap();
        let mut b = Index::new();
        for s in &back {
            restore(&mut b, s);
        }
        assert_eq!(sorted_paths(&a), sorted_paths(&b));
    }

    #[test]
    fn two_volumes_roundtrip() {
        let mut ix = Index::new();
        let c = VolumeIndex::build(&mut ix, "C:", ROOT, volume(300));
        let d = VolumeIndex::build(&mut ix, "D:", ROOT, volume(100));
        let snaps = vec![
            capture(&c, &ix, "C:", 1, 2, 3),
            capture(&d, &ix, "D:", 4, 5, 6),
        ];
        assert_eq!(decode(&encode(&snaps)).unwrap(), snaps);
    }

    #[test]
    fn rejects_bad_magic_version_checksum_truncation() {
        let mut ix = Index::new();
        let v = VolumeIndex::build(&mut ix, "C:", ROOT, volume(300));
        let good = encode(&[capture(&v, &ix, "C:", 1, 2, 3)]);
        let mut bad = good.clone();
        bad[0] = b'X';
        assert!(matches!(decode(&bad), Err(SnapshotError::BadMagic)));
        let mut bad = good.clone();
        bad[8] = 9;
        assert!(matches!(decode(&bad), Err(SnapshotError::Version(9))));
        let mut bad = good.clone();
        let last = bad.len() - 1;
        bad[last] ^= 1;
        assert!(matches!(decode(&bad), Err(SnapshotError::Checksum)));
        for cut in [0, 5, HEADER - 1, HEADER + 3, good.len() - 1] {
            assert!(
                matches!(decode(&good[..cut]), Err(SnapshotError::Truncated)),
                "cut at {cut}"
            );
        }
        // a payload that passes the checksum but lies about its counts
        let mut lying = good.clone();
        let count_at = HEADER; // u32 volume count
        lying[count_at..count_at + 4].copy_from_slice(&7u32.to_le_bytes());
        let sum = fnv1a(&lying[HEADER..]);
        lying[20..28].copy_from_slice(&sum.to_le_bytes());
        assert!(matches!(decode(&lying), Err(SnapshotError::Truncated)));
    }

    fn tmp_dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("flint-snap-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn save_atomic_replaces_and_load_reads() {
        let d = tmp_dir("replace");
        let p = d.join("index-user.bin");
        let mut ix = Index::new();
        let v = VolumeIndex::build(&mut ix, "C:", ROOT, volume(50));
        let one = vec![capture(&v, &ix, "C:", 1, 1, 1)];
        let two = vec![capture(&v, &ix, "C:", 2, 2, 2)];
        save_atomic(&p, &encode(&one)).unwrap();
        save_atomic(&p, &encode(&two)).unwrap();
        assert_eq!(load(&p).unwrap(), two);
        assert_eq!(
            std::fs::read_dir(&d).unwrap().count(),
            1,
            "no temp file left"
        );
        assert!(matches!(
            load(&d.join("missing.bin")),
            Err(SnapshotError::Io(_))
        ));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn save_atomic_leaves_old_file_on_failure() {
        let d = tmp_dir("fail");
        let p = d.join("index-user.bin");
        std::fs::write(&p, b"old").unwrap();
        // the temp file's place is taken by a folder, so the write fails
        std::fs::create_dir(temp_path(&p)).unwrap();
        assert!(save_atomic(&p, b"new").is_err());
        assert_eq!(std::fs::read(&p).unwrap(), b"old");
        let _ = std::fs::remove_dir_all(&d);
    }
}
