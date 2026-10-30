//! Parsing of NTFS change-journal records (`USN_RECORD_V2` / `V3`) from the
//! byte buffers that `FSCTL_ENUM_USN_DATA` and `FSCTL_READ_USN_JOURNAL`
//! fill. Pure: the caller does the I/O.

pub const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
pub const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
pub const USN_REASON_FILE_CREATE: u32 = 0x100;
pub const USN_REASON_FILE_DELETE: u32 = 0x200;
pub const USN_REASON_SECURITY_CHANGE: u32 = 0x800;
pub const USN_REASON_RENAME_OLD_NAME: u32 = 0x1000;
pub const USN_REASON_RENAME_NEW_NAME: u32 = 0x2000;
pub const USN_REASON_CLOSE: u32 = 0x8000_0000;

/// One journal or MFT-enumeration record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UsnRecord {
    /// File reference number: MFT record index (low 48 bits) plus
    /// sequence number (high 16 bits).
    pub frn: u64,
    pub parent: u64,
    pub usn: i64,
    /// Unix seconds of the record's timestamp; 0 when it has none.
    pub time: u32,
    pub reason: u32,
    pub attrs: u32,
    /// NTFS security id: entries with the same id share one security
    /// descriptor (0 when the record has none).
    pub security_id: u32,
    pub name: String,
}

impl UsnRecord {
    pub fn is_dir(&self) -> bool {
        self.attrs & FILE_ATTRIBUTE_DIRECTORY != 0
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum UsnError {
    /// The buffer ends inside a record or its cursor.
    Truncated,
    /// A record length below the fixed header, or not a multiple of 8.
    BadLength(u32),
    /// A record version other than 2 or 3.
    Version(u16),
    /// A V3 reference whose high 64 bits are set (ReFS-style 128-bit ids).
    WideReference,
}

/// Seconds between 1601-01-01 and 1970-01-01.
const EPOCH_DIFF: i64 = 11_644_473_600;

/// Unix seconds from a FILETIME (100 ns ticks since 1601), clamped to u32;
/// 0 for anything at or before 1970.
pub fn filetime_to_unix(ft: i64) -> u32 {
    let secs = ft / 10_000_000 - EPOCH_DIFF;
    secs.clamp(0, u32::MAX as i64) as u32
}

fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}
fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().expect("4 bytes"))
}
fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().expect("8 bytes"))
}

/// Field offsets of one record version.
struct Layout {
    header: usize,
    frn: usize,
    frn_high: Option<usize>,
    parent: usize,
    parent_high: Option<usize>,
    usn: usize,
    time: usize,
    reason: usize,
    attrs: usize,
    name_len: usize,
    name_off: usize,
}

const V2: Layout = Layout {
    header: 60,
    frn: 8,
    frn_high: None,
    parent: 16,
    parent_high: None,
    usn: 24,
    time: 32,
    reason: 40,
    attrs: 52,
    name_len: 56,
    name_off: 58,
};

const V3: Layout = Layout {
    header: 76,
    frn: 8,
    frn_high: Some(16),
    parent: 24,
    parent_high: Some(32),
    usn: 40,
    time: 48,
    reason: 56,
    attrs: 68,
    name_len: 72,
    name_off: 74,
};

/// Parses consecutive records filling `buf`.
pub fn parse_records(buf: &[u8]) -> Result<Vec<UsnRecord>, UsnError> {
    let mut out = Vec::new();
    let mut at = 0;
    while at < buf.len() {
        if buf.len() - at < 8 {
            return Err(UsnError::Truncated);
        }
        let len = u32_at(buf, at);
        let l = match u16_at(buf, at + 4) {
            2 => &V2,
            3 => &V3,
            v => return Err(UsnError::Version(v)),
        };
        if (len as usize) < l.header || !len.is_multiple_of(8) {
            return Err(UsnError::BadLength(len));
        }
        if len as usize > buf.len() - at {
            return Err(UsnError::Truncated);
        }
        let r = &buf[at..at + len as usize];
        for high in [l.frn_high, l.parent_high].into_iter().flatten() {
            if u64_at(r, high) != 0 {
                return Err(UsnError::WideReference);
            }
        }
        let (no, nl) = (
            u16_at(r, l.name_off) as usize,
            u16_at(r, l.name_len) as usize,
        );
        if no + nl > r.len() || nl % 2 != 0 {
            return Err(UsnError::Truncated);
        }
        let units: Vec<u16> = r[no..no + nl]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        out.push(UsnRecord {
            frn: u64_at(r, l.frn),
            parent: u64_at(r, l.parent),
            usn: u64_at(r, l.usn) as i64,
            time: filetime_to_unix(u64_at(r, l.time) as i64),
            reason: u32_at(r, l.reason),
            attrs: u32_at(r, l.attrs),
            // SecurityId sits just before FileAttributes in V2 and V3
            security_id: u32_at(r, l.attrs - 4),
            name: String::from_utf16_lossy(&units),
        });
        at += len as usize;
    }
    Ok(out)
}

fn cursor(buf: &[u8]) -> Result<(u64, Vec<UsnRecord>), UsnError> {
    if buf.len() < 8 {
        return Err(UsnError::Truncated);
    }
    Ok((u64_at(buf, 0), parse_records(&buf[8..])?))
}

/// `FSCTL_ENUM_USN_DATA` output: the next start FRN, then records.
pub fn parse_enum_output(buf: &[u8]) -> Result<(u64, Vec<UsnRecord>), UsnError> {
    cursor(buf)
}

/// `FSCTL_READ_USN_JOURNAL` output: the next USN, then records.
pub fn parse_read_output(buf: &[u8]) -> Result<(i64, Vec<UsnRecord>), UsnError> {
    cursor(buf).map(|(u, r)| (u as i64, r))
}

/// Offsets in a `FILE_ID_BOTH_DIR_INFO` entry (what
/// `GetFileInformationByHandleEx(FileIdBothDirectoryInfo)` lists).
mod dir_info {
    pub const NEXT: usize = 0;
    pub const LAST_WRITE: usize = 24;
    pub const ATTRS: usize = 56;
    pub const NAME_LEN: usize = 60;
    pub const FILE_ID: usize = 96;
    pub const NAME: usize = 104;
}

/// The entries of one folder listing (`FILE_ID_BOTH_DIR_INFO` entries
/// chained by their next offset) as creation records under `parent`;
/// `.` and `..` are left out. The file id is the FRN on NTFS.
pub fn parse_dir_info(buf: &[u8], parent: u64) -> Result<Vec<UsnRecord>, UsnError> {
    use dir_info::*;
    let mut out = Vec::new();
    let mut at = 0usize;
    loop {
        let e = buf.get(at..).ok_or(UsnError::Truncated)?;
        if e.len() < NAME {
            return Err(UsnError::Truncated);
        }
        let nl = u32_at(e, NAME_LEN) as usize;
        if NAME + nl > e.len() || !nl.is_multiple_of(2) {
            return Err(UsnError::Truncated);
        }
        let units: Vec<u16> = e[NAME..NAME + nl]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        let name = String::from_utf16_lossy(&units);
        if name != "." && name != ".." {
            out.push(UsnRecord {
                frn: u64_at(e, FILE_ID),
                parent,
                usn: 0,
                time: filetime_to_unix(u64_at(e, LAST_WRITE) as i64),
                reason: USN_REASON_FILE_CREATE,
                attrs: u32_at(e, ATTRS),
                security_id: 0,
                name,
            });
        }
        match u32_at(e, NEXT) as usize {
            0 => return Ok(out),
            n if n < NAME || !n.is_multiple_of(8) => return Err(UsnError::BadLength(n as u32)),
            n => at += n,
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn dir_entry(next: u32, id: u64, attrs: u32, name: &str) -> Vec<u8> {
        let units: Vec<u8> = name.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let mut e = vec![0u8; 104];
        e[0..4].copy_from_slice(&next.to_le_bytes());
        e[24..32].copy_from_slice(&116_444_736_100_000_000u64.to_le_bytes()); // 1970 + 10 s
        e[56..60].copy_from_slice(&attrs.to_le_bytes());
        e[60..64].copy_from_slice(&(units.len() as u32).to_le_bytes());
        e[96..104].copy_from_slice(&id.to_le_bytes());
        e.extend(units);
        if next != 0 {
            e.resize(next as usize, 0);
        }
        e
    }

    #[test]
    fn folder_listing_parses_and_skips_dot_entries() {
        let mut buf = dir_entry(112, 1, FILE_ATTRIBUTE_DIRECTORY, ".");
        buf.extend(dir_entry(112, 2, FILE_ATTRIBUTE_DIRECTORY, ".."));
        buf.extend(dir_entry(
            120,
            0x0001_0000_0000_0042,
            FILE_ATTRIBUTE_DIRECTORY,
            "Bütçe",
        ));
        buf.extend(dir_entry(0, 0x43, 0x20, "rapor.txt"));
        let r = parse_dir_info(&buf, 7).unwrap();
        assert_eq!(
            r.iter()
                .map(|r| (r.frn, r.name.as_str(), r.is_dir(), r.parent, r.time))
                .collect::<Vec<_>>(),
            [
                (0x0001_0000_0000_0042, "Bütçe", true, 7, 10),
                (0x43, "rapor.txt", false, 7, 10)
            ]
        );
        assert!(r.iter().all(|r| r.reason == USN_REASON_FILE_CREATE));
        // damage is reported, not read past
        assert!(matches!(
            parse_dir_info(&buf[..buf.len() - 2], 7),
            Err(UsnError::Truncated)
        ));
        let mut bad = buf.clone();
        bad[0..4].copy_from_slice(&3u32.to_le_bytes());
        assert!(matches!(
            parse_dir_info(&bad, 7),
            Err(UsnError::BadLength(3))
        ));
        assert!(matches!(parse_dir_info(&[], 7), Err(UsnError::Truncated)));
    }

    /// The offsets match Windows' own structure.
    #[cfg(windows)]
    #[test]
    fn dir_info_offsets_match_windows() {
        use std::mem::offset_of;
        use windows_sys::Win32::Storage::FileSystem::FILE_ID_BOTH_DIR_INFO as I;
        assert_eq!(offset_of!(I, NextEntryOffset), dir_info::NEXT);
        assert_eq!(offset_of!(I, LastWriteTime), dir_info::LAST_WRITE);
        assert_eq!(offset_of!(I, FileAttributes), dir_info::ATTRS);
        assert_eq!(offset_of!(I, FileNameLength), dir_info::NAME_LEN);
        assert_eq!(offset_of!(I, FileId), dir_info::FILE_ID);
        assert_eq!(offset_of!(I, FileName), dir_info::NAME);
    }

    fn put16(b: &mut [u8], at: usize, v: u16) {
        b[at..at + 2].copy_from_slice(&v.to_le_bytes());
    }
    fn put32(b: &mut [u8], at: usize, v: u32) {
        b[at..at + 4].copy_from_slice(&v.to_le_bytes());
    }
    fn put64(b: &mut [u8], at: usize, v: u64) {
        b[at..at + 8].copy_from_slice(&v.to_le_bytes());
    }

    /// A `USN_RECORD_V2` built byte by byte (layout from winioctl.h).
    pub(crate) fn v2(
        frn: u64,
        parent: u64,
        name: &str,
        attrs: u32,
        reason: u32,
        ft: i64,
    ) -> Vec<u8> {
        let n: Vec<u16> = name.encode_utf16().collect();
        let len = (60 + n.len() * 2).div_ceil(8) * 8;
        let mut b = vec![0u8; len];
        put32(&mut b, 0, len as u32);
        put16(&mut b, 4, 2);
        put64(&mut b, 8, frn);
        put64(&mut b, 16, parent);
        put64(&mut b, 24, 77);
        put64(&mut b, 32, ft as u64);
        put32(&mut b, 40, reason);
        put32(&mut b, 52, attrs);
        put16(&mut b, 56, (n.len() * 2) as u16);
        put16(&mut b, 58, 60);
        for (i, u) in n.iter().enumerate() {
            put16(&mut b, 60 + i * 2, *u);
        }
        b
    }

    fn v3(frn: u64, parent: u64, name: &str, high: u64) -> Vec<u8> {
        let n: Vec<u16> = name.encode_utf16().collect();
        let len = (76 + n.len() * 2).div_ceil(8) * 8;
        let mut b = vec![0u8; len];
        put32(&mut b, 0, len as u32);
        put16(&mut b, 4, 3);
        put64(&mut b, 8, frn);
        put64(&mut b, 16, high);
        put64(&mut b, 24, parent);
        put64(&mut b, 40, 9);
        put32(&mut b, 56, USN_REASON_FILE_CREATE);
        put32(&mut b, 68, FILE_ATTRIBUTE_DIRECTORY);
        put16(&mut b, 72, (n.len() * 2) as u16);
        put16(&mut b, 74, 76);
        for (i, u) in n.iter().enumerate() {
            put16(&mut b, 76 + i * 2, *u);
        }
        b
    }

    #[test]
    fn parses_v2_record() {
        let mut buf = v2(
            30,
            5,
            "Bütçe.xlsx",
            0x20,
            USN_REASON_FILE_CREATE,
            133_000_000_000_000_000,
        );
        buf.extend(v2(31, 30, "a", FILE_ATTRIBUTE_DIRECTORY, 0, 0));
        let r = parse_records(&buf).unwrap();
        assert_eq!(r.len(), 2);
        assert_eq!(
            r[0],
            UsnRecord {
                frn: 30,
                parent: 5,
                usn: 77,
                time: 1_655_526_400,
                reason: USN_REASON_FILE_CREATE,
                attrs: 0x20,
                security_id: 0,
                name: "Bütçe.xlsx".into(),
            }
        );
        assert!(r[1].is_dir());
        assert_eq!(r[1].time, 0, "a zero FILETIME is no time");
        // SecurityId (offset 48 in V2, 64 in V3)
        let mut b = v2(32, 5, "g", FILE_ATTRIBUTE_DIRECTORY, 0, 0);
        put32(&mut b, 48, 0x1234);
        assert_eq!(parse_records(&b).unwrap()[0].security_id, 0x1234);
        let mut b = v3(33, 5, "g", 0);
        put32(&mut b, 64, 0x0567);
        assert_eq!(parse_records(&b).unwrap()[0].security_id, 0x0567);
    }

    #[test]
    fn parses_v3_low_half() {
        let r = parse_records(&v3(40, 5, "Rapor", 0)).unwrap();
        assert_eq!((r[0].frn, r[0].parent, r[0].usn), (40, 5, 9));
        assert_eq!(r[0].name, "Rapor");
        assert!(r[0].is_dir());
        assert_eq!(
            parse_records(&v3(40, 5, "x", 1)),
            Err(UsnError::WideReference)
        );
    }

    #[test]
    fn rejects_truncated_and_zero_length() {
        let good = v2(30, 5, "rapor.pdf", 0, 0, 0);
        assert_eq!(
            parse_records(&good[..good.len() - 8]),
            Err(UsnError::Truncated)
        );
        let mut zero = good.clone();
        zero[0..4].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(parse_records(&zero), Err(UsnError::BadLength(0)));
        let mut odd = good.clone();
        odd[0..4].copy_from_slice(&61u32.to_le_bytes());
        assert_eq!(parse_records(&odd), Err(UsnError::BadLength(61)));
        let mut ver = good.clone();
        ver[4..6].copy_from_slice(&4u16.to_le_bytes());
        assert_eq!(parse_records(&ver), Err(UsnError::Version(4)));
        let mut name_past = good;
        name_past[56..58].copy_from_slice(&200u16.to_le_bytes());
        assert_eq!(parse_records(&name_past), Err(UsnError::Truncated));
        assert_eq!(parse_records(&[]), Ok(vec![]));
    }

    #[test]
    fn outputs_read_cursor() {
        let mut buf = 0x1234u64.to_le_bytes().to_vec();
        buf.extend(v2(30, 5, "x", 0, 0, 0));
        let (next, r) = parse_enum_output(&buf).unwrap();
        assert_eq!((next, r.len()), (0x1234, 1));
        let (usn, r) = parse_read_output(&buf).unwrap();
        assert_eq!((usn, r.len()), (0x1234, 1));
        assert_eq!(parse_enum_output(&[1, 2, 3]), Err(UsnError::Truncated));
    }

    #[test]
    fn filetime_edges() {
        assert_eq!(filetime_to_unix(0), 0);
        assert_eq!(filetime_to_unix(-5), 0);
        assert_eq!(filetime_to_unix(116_444_736_000_000_000), 0, "1970-01-01");
        assert_eq!(filetime_to_unix(i64::MAX), u32::MAX);
    }
}
