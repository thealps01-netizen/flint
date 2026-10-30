//! NTFS on-disk structures the folder access checks read instead of
//! opening folders: the security id in a file record's
//! `$STANDARD_INFORMATION`, and the descriptors in `$Secure:$SDS`.
//! Opening a folder (even for its descriptor only) leaves kernel memory
//! behind in some file system filters, once per folder whose cached state
//! Windows has dropped; reading records and `$SDS` opens nothing.
//! Pure: parses bytes; the reads are in `volume`.

use std::collections::HashMap;

/// Attribute types used here.
pub const ATTR_STANDARD_INFORMATION: u32 = 0x10;
pub const ATTR_ATTRIBUTE_LIST: u32 = 0x20;
pub const ATTR_DATA: u32 = 0x80;

/// Record header flags.
const IN_USE: u16 = 0x1;
const DIRECTORY: u16 = 0x2;

fn u16_at(b: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(o..o + 2)?.try_into().ok()?))
}
fn u32_at(b: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(o..o + 4)?.try_into().ok()?))
}
fn u64_at(b: &[u8], o: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(o..o + 8)?.try_into().ok()?))
}

/// Undoes the update sequence of a record read from disk (the last two
/// bytes of each stride were replaced by a check value on write). False
/// when the record is not a file record or a stride does not carry the
/// check value (torn write); the record is then not to be used. Records
/// returned by `FSCTL_GET_NTFS_FILE_RECORD` are already in this form.
pub fn apply_fixup(rec: &mut [u8]) -> bool {
    if rec.get(0..4) != Some(b"FILE") {
        return false;
    }
    let (Some(usa), Some(n)) = (u16_at(rec, 4), u16_at(rec, 6)) else {
        return false;
    };
    let (usa, n) = (usa as usize, n as usize);
    if n < 2 || usa + 2 * n > rec.len() || !rec.len().is_multiple_of(n - 1) {
        return false;
    }
    // NTFS protects every 512 bytes, whatever the sector size
    let stride = rec.len() / (n - 1);
    if stride != 512 {
        return false;
    }
    let check = [rec[usa], rec[usa + 1]];
    for i in 1..n {
        let end = i * stride - 2;
        if rec[end..end + 2] != check {
            return false;
        }
    }
    for i in 1..n {
        let end = i * stride - 2;
        rec[end] = rec[usa + 2 * i];
        rec[end + 1] = rec[usa + 2 * i + 1];
    }
    true
}

/// One attribute of a record.
#[derive(Debug)]
pub struct Attribute<'a> {
    pub kind: u32,
    pub name: String,
    pub resident: bool,
    pub bytes: &'a [u8],
}

fn utf16_at(b: &[u8], o: usize, chars: usize) -> Option<String> {
    let w: Option<Vec<u16>> = (0..chars).map(|i| u16_at(b, o + 2 * i)).collect();
    Some(String::from_utf16_lossy(&w?))
}

/// The attributes of a (fixed-up) file record, in order; stops at the end
/// marker or at the first malformed header.
pub fn attributes(rec: &[u8]) -> Vec<Attribute<'_>> {
    let mut out = Vec::new();
    let Some(mut off) = u16_at(rec, 0x14).map(usize::from) else {
        return out;
    };
    while let Some(kind) = u32_at(rec, off) {
        if kind == 0xFFFF_FFFF {
            break;
        }
        let Some(len) = u32_at(rec, off + 4).map(|l| l as usize) else {
            break;
        };
        if len < 0x18 || off + len > rec.len() {
            break;
        }
        let bytes = &rec[off..off + len];
        let (name_len, name_off) = (bytes[9] as usize, u16_at(bytes, 10).unwrap_or(0) as usize);
        let Some(name) = utf16_at(bytes, name_off, name_len) else {
            break;
        };
        out.push(Attribute {
            kind,
            name,
            resident: bytes[8] == 0,
            bytes,
        });
        off += len;
    }
    out
}

/// The value of a resident attribute.
pub fn resident_value<'a>(a: &Attribute<'a>) -> Option<&'a [u8]> {
    if !a.resident {
        return None;
    }
    let len = u32_at(a.bytes, 0x10)? as usize;
    let off = u16_at(a.bytes, 0x14)? as usize;
    a.bytes.get(off..off.checked_add(len)?)
}

/// One run of a non-resident attribute: `lcn` None is a sparse run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Run {
    pub lcn: Option<u64>,
    pub clusters: u64,
}

/// A non-resident attribute's first VCN, real size in bytes and runs.
pub fn data_runs(a: &Attribute) -> Option<(u64, u64, Vec<Run>)> {
    if a.resident || a.bytes.len() < 0x40 {
        return None;
    }
    let b = a.bytes;
    let (start_vcn, real) = (u64_at(b, 0x10)?, u64_at(b, 0x30)?);
    let mut p = u16_at(b, 0x20)? as usize;
    let mut lcn = 0i64;
    let mut runs = Vec::new();
    while let Some(&head) = b.get(p) {
        if head == 0 {
            return Some((start_vcn, real, runs));
        }
        let (ls, os) = ((head & 0xF) as usize, (head >> 4) as usize);
        if ls == 0 || ls > 8 || os > 8 {
            return None;
        }
        let len_bytes = b.get(p + 1..p + 1 + ls)?;
        let clusters = len_bytes
            .iter()
            .rev()
            .fold(0u64, |v, &x| (v << 8) | u64::from(x));
        let off_bytes = b.get(p + 1 + ls..p + 1 + ls + os)?;
        p += 1 + ls + os;
        if os == 0 {
            runs.push(Run {
                lcn: None,
                clusters,
            });
            continue;
        }
        let mut delta = off_bytes
            .iter()
            .rev()
            .fold(0i64, |v, &x| (v << 8) | i64::from(x));
        if os < 8 && off_bytes[os - 1] & 0x80 != 0 {
            delta -= 1i64 << (8 * os);
        }
        lcn = lcn.checked_add(delta)?;
        runs.push(Run {
            lcn: Some(u64::try_from(lcn).ok()?),
            clusters,
        });
    }
    None
}

/// The security id of a record that is an in-use base record of a folder
/// (None for files, free records, extension records, or a
/// `$STANDARD_INFORMATION` too short to hold one).
pub fn folder_security_id(rec: &[u8]) -> Option<u32> {
    if rec.get(0..4) != Some(b"FILE") {
        return None;
    }
    let flags = u16_at(rec, 0x16)?;
    if flags & (IN_USE | DIRECTORY) != IN_USE | DIRECTORY || u64_at(rec, 0x20)? != 0 {
        return None;
    }
    let attrs = attributes(rec);
    let si = attrs
        .iter()
        .find(|a| a.kind == ATTR_STANDARD_INFORMATION && a.resident)?;
    u32_at(resident_value(si)?, 0x34)
}

/// The FRN (sequence number << 48 | record number) of record `number`.
pub fn frn_of(rec: &[u8], number: u64) -> Option<u64> {
    Some((u64::from(u16_at(rec, 0x10)?) << 48) | (number & 0xFFFF_FFFF_FFFF))
}

/// Records other than `base` that an `$ATTRIBUTE_LIST` value names as
/// holding attribute `kind` named `name`.
pub fn listed_records(list: &[u8], kind: u32, name: &str, base: u64) -> Vec<u64> {
    let mut out = Vec::new();
    let mut p = 0;
    while let (Some(t), Some(len)) = (u32_at(list, p), u16_at(list, p + 4)) {
        let len = len as usize;
        if len < 0x1A || p + len > list.len() {
            break;
        }
        let e = &list[p..p + len];
        let (nl, no) = (e[6] as usize, e[7] as usize);
        let record = u64_at(e, 0x10).map(|r| r & 0xFFFF_FFFF_FFFF);
        if t == kind
            && utf16_at(e, no, nl).as_deref() == Some(name)
            && let Some(r) = record
            && r != base
            && !out.contains(&r)
        {
            out.push(r);
        }
        p += len;
    }
    out
}

/// NTFS's hash of a security descriptor (stored with it in `$SDS`):
/// for each little-endian dword `d`, `hash = d + rotl(hash, 3)`.
pub fn descriptor_hash(sd: &[u8]) -> u32 {
    sd.as_chunks::<4>().0.iter().fold(0u32, |h, d| {
        u32::from_le_bytes(*d).wrapping_add(h.rotate_left(3))
    })
}

/// `$SDS` is written in 256 KiB blocks, each followed by its mirror copy
/// (whose entries carry the original's offset, so they never match).
const SDS_BLOCK: usize = 0x40000;
/// An entry's header: hash, id, offset (8), length (4).
const SDS_HEADER: usize = 20;

/// The descriptors in a `$SDS` stream by security id. Entries are found by
/// their header (offset field equal to their position, hash matching the
/// descriptor), so holes inside a block and the mirror copy of each
/// 256 KiB block are passed over.
pub fn sds_descriptors(sds: &[u8]) -> HashMap<u32, Vec<u8>> {
    let mut out: HashMap<u32, Vec<u8>> = HashMap::new();
    let mut doubtful = std::collections::HashSet::new();
    let mut p = 0usize;
    while p + SDS_HEADER <= sds.len() {
        // entries start 16-byte aligned
        let at = |o| u32_at(sds, p + o).unwrap_or(0);
        let (hash, id, len) = (at(0), at(4), at(16) as usize);
        if u64_at(sds, p + 8) == Some(p as u64)
            && id != 0
            && len > SDS_HEADER
            && p % SDS_BLOCK + len <= SDS_BLOCK
            && p + len <= sds.len()
        {
            let sd = &sds[p + SDS_HEADER..p + len];
            // a hash over whole dwords only checks a length of them
            if sd.len().is_multiple_of(4) && descriptor_hash(sd) == hash {
                match out.get(&id) {
                    Some(seen) if seen.as_slice() != sd => {
                        doubtful.insert(id);
                    }
                    Some(_) => {}
                    None => {
                        out.insert(id, sd.to_vec());
                    }
                }
            }
            // a torn entry is passed over whole: its body is not searched
            // for headers
            p += (len + 15) & !15;
            continue;
        }
        p += 16;
    }
    // one id with two different descriptors: its folders are opened
    out.retain(|id, _| !doubtful.contains(id));
    out
}

/// The record number a (fixed-up) record says it has (NTFS 3.1+).
pub fn record_number(rec: &[u8]) -> Option<u32> {
    u32_at(rec, 0x2C)
}

/// The real size and runs of an attribute held in several parts (first
/// VCN, real size, runs), or None when the parts leave a gap or overlap.
pub fn join_parts(mut parts: Vec<(u64, u64, Vec<Run>)>) -> Option<(u64, Vec<Run>)> {
    parts.sort_by_key(|p| p.0);
    let real = parts.first()?.1;
    let mut next = 0u64;
    let mut runs = Vec::new();
    for (vcn, _, part) in parts {
        if vcn != next {
            return None;
        }
        for r in &part {
            next = next.checked_add(r.clusters)?;
        }
        runs.extend(part);
    }
    Some((real, runs))
}

/// Whether a self-relative descriptor's parts (owner, group, SACL, DACL)
/// all lie inside `sd`, so the OS can be given it.
pub fn descriptor_in_bounds(sd: &[u8]) -> bool {
    /// `SE_SELF_RELATIVE`, `SE_DACL_PRESENT`, `SE_SACL_PRESENT`.
    const SELF_RELATIVE: u16 = 0x8000;
    const DACL_PRESENT: u16 = 0x0004;
    const SACL_PRESENT: u16 = 0x0010;
    let (Some(&revision), Some(control)) = (sd.first(), u16_at(sd, 2)) else {
        return false;
    };
    let offsets: Option<Vec<usize>> = [4, 8, 12, 16]
        .iter()
        .map(|&o| u32_at(sd, o).map(|v| v as usize))
        .collect();
    let Some(&[owner, group, sacl, dacl]) = offsets.as_deref() else {
        return false;
    };
    if revision != 1 || control & SELF_RELATIVE == 0 {
        return false;
    }
    let sid_fits = |o: usize| {
        o == 0
            || sd
                .get(o + 1)
                .is_some_and(|&n| n <= 15 && o + 8 + 4 * n as usize <= sd.len())
    };
    let acl_fits = |present: bool, o: usize| {
        !present
            || o == 0
            || u16_at(sd, o + 2).is_some_and(|n| n >= 8 && o + n as usize <= sd.len())
    };
    sid_fits(owner)
        && sid_fits(group)
        && acl_fits(control & SACL_PRESENT != 0, sacl)
        && acl_fits(control & DACL_PRESENT != 0, dacl)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RS: usize = 1024;

    fn put16(b: &mut [u8], o: usize, v: u16) {
        b[o..o + 2].copy_from_slice(&v.to_le_bytes());
    }
    fn put32(b: &mut [u8], o: usize, v: u32) {
        b[o..o + 4].copy_from_slice(&v.to_le_bytes());
    }
    fn put64(b: &mut [u8], o: usize, v: u64) {
        b[o..o + 8].copy_from_slice(&v.to_le_bytes());
    }

    fn resident(kind: u32, name: &str, value: &[u8]) -> Vec<u8> {
        let name16: Vec<u16> = name.encode_utf16().collect();
        let name_off = 0x18;
        let value_off = (name_off + 2 * name16.len() + 7) & !7;
        let len = (value_off + value.len() + 7) & !7;
        let mut a = vec![0u8; len];
        put32(&mut a, 0, kind);
        put32(&mut a, 4, len as u32);
        a[8] = 0;
        a[9] = name16.len() as u8;
        put16(&mut a, 10, name_off as u16);
        put32(&mut a, 0x10, value.len() as u32);
        put16(&mut a, 0x14, value_off as u16);
        for (i, c) in name16.iter().enumerate() {
            put16(&mut a, name_off + 2 * i, *c);
        }
        a[value_off..value_off + value.len()].copy_from_slice(value);
        a
    }

    fn non_resident(kind: u32, name: &str, start_vcn: u64, real: u64, runs: &[u8]) -> Vec<u8> {
        let name16: Vec<u16> = name.encode_utf16().collect();
        let name_off = 0x40;
        let runs_off = (name_off + 2 * name16.len() + 7) & !7;
        let len = (runs_off + runs.len() + 1 + 7) & !7;
        let mut a = vec![0u8; len];
        put32(&mut a, 0, kind);
        put32(&mut a, 4, len as u32);
        a[8] = 1;
        a[9] = name16.len() as u8;
        put16(&mut a, 10, name_off as u16);
        put64(&mut a, 0x10, start_vcn);
        put16(&mut a, 0x20, runs_off as u16);
        put64(&mut a, 0x30, real);
        for (i, c) in name16.iter().enumerate() {
            put16(&mut a, name_off + 2 * i, *c);
        }
        a[runs_off..runs_off + runs.len()].copy_from_slice(runs);
        a
    }

    /// `$STANDARD_INFORMATION` (NTFS 3.x, 0x48 bytes) with this id.
    fn standard_info(security_id: u32) -> Vec<u8> {
        let mut v = vec![0u8; 0x48];
        put32(&mut v, 0x34, security_id);
        v
    }

    /// A file record as in memory (no update sequence applied yet).
    fn record(seq: u16, flags: u16, base: u64, attrs: &[Vec<u8>]) -> Vec<u8> {
        let mut r = vec![0u8; RS];
        put32(&mut r, 0x2C, 40);
        r[0..4].copy_from_slice(b"FILE");
        put16(&mut r, 4, 0x30); // update sequence array offset
        put16(&mut r, 6, 3); // 1 check value + 2 strides of 512
        put16(&mut r, 0x10, seq);
        put16(&mut r, 0x14, 0x38); // first attribute
        put16(&mut r, 0x16, flags);
        put64(&mut r, 0x20, base);
        let mut o = 0x38;
        for a in attrs {
            r[o..o + a.len()].copy_from_slice(a);
            o += a.len();
        }
        put32(&mut r, o, 0xFFFF_FFFF);
        r
    }

    /// The record as written to disk: each stride's last two bytes moved
    /// into the update sequence array, replaced by the check value.
    fn on_disk(mut r: Vec<u8>, check: u16) -> Vec<u8> {
        put16(&mut r, 0x30, check);
        for i in 1..3 {
            let end = i * 512 - 2;
            let saved = u16::from_le_bytes([r[end], r[end + 1]]);
            put16(&mut r, 0x30 + 2 * i, saved);
            put16(&mut r, end, check);
        }
        r
    }

    #[test]
    fn a_record_read_from_disk_gets_its_stride_ends_back() {
        let mut mem = record(
            3,
            IN_USE | DIRECTORY,
            0,
            &[resident(0x10, "", &standard_info(7))],
        );
        // data that ends up under a stride end
        put16(&mut mem, 510, 0xBEEF);
        put16(&mut mem, 1022, 0xCAFE);
        let mut disk = on_disk(mem.clone(), 0x0042);
        assert_ne!(disk, mem);
        assert!(apply_fixup(&mut disk));
        assert_eq!(&disk[..0x30], &mem[..0x30]);
        assert_eq!(&disk[0x38..], &mem[0x38..]);
    }

    #[test]
    fn a_record_says_its_number() {
        assert_eq!(record_number(&record(1, IN_USE, 0, &[])), Some(40));
        assert_eq!(record_number(&[0u8; 0x2E]), None);
    }

    #[test]
    fn parts_join_only_without_gaps() {
        let run = |c| Run {
            lcn: Some(100),
            clusters: c,
        };
        let joined = join_parts(vec![(8, 0, vec![run(2)]), (0, 999, vec![run(5), run(3)])]);
        assert_eq!(joined, Some((999, vec![run(5), run(3), run(2)])));
        assert_eq!(
            join_parts(vec![(0, 9, vec![run(5)]), (6, 0, vec![run(1)])]),
            None,
            "gap"
        );
        assert_eq!(
            join_parts(vec![(0, 9, vec![run(5)]), (4, 0, vec![run(1)])]),
            None,
            "overlap"
        );
        assert_eq!(
            join_parts(vec![(1, 9, vec![run(5)])]),
            None,
            "no first part"
        );
        assert_eq!(join_parts(Vec::new()), None);
    }

    #[test]
    fn a_torn_or_foreign_record_is_refused() {
        let mem = record(3, IN_USE, 0, &[]);
        let mut torn = on_disk(mem.clone(), 0x0042);
        put16(&mut torn, 1022, 0x0043); // second stride written later
        assert!(!apply_fixup(&mut torn));
        let mut bad = on_disk(mem.clone(), 0x0042);
        bad[0..4].copy_from_slice(b"BAAD");
        assert!(!apply_fixup(&mut bad));
        let mut wild = on_disk(mem.clone(), 0x0042);
        put16(&mut wild, 6, 900); // update sequence past the record
        assert!(!apply_fixup(&mut wild));
        // one stride of 1024 bytes: only the last two bytes would be checked
        let mut one = mem;
        put16(&mut one, 6, 2);
        put16(&mut one, 0x30, 0x0042);
        put16(&mut one, 1022, 0x0042);
        assert!(!apply_fixup(&mut one), "strides are 512 bytes");
    }

    #[test]
    fn an_in_use_folder_record_gives_its_security_id() {
        let folder = record(
            5,
            IN_USE | DIRECTORY,
            0,
            &[resident(0x10, "", &standard_info(0x1234))],
        );
        assert_eq!(folder_security_id(&folder), Some(0x1234));
        assert_eq!(frn_of(&folder, 40), Some((5u64 << 48) | 40));
    }

    #[test]
    fn files_free_records_and_extension_records_give_none() {
        let si = resident(0x10, "", &standard_info(9));
        assert_eq!(
            folder_security_id(&record(1, IN_USE, 0, std::slice::from_ref(&si))),
            None,
            "a file"
        );
        assert_eq!(
            folder_security_id(&record(1, DIRECTORY, 0, std::slice::from_ref(&si))),
            None,
            "free"
        );
        assert_eq!(
            folder_security_id(&record(1, IN_USE | DIRECTORY, 77, &[si])),
            None,
            "extension record of record 77"
        );
        // NTFS 1.2 sized $STANDARD_INFORMATION: no security id field
        let old = resident(0x10, "", &[0u8; 0x30]);
        assert_eq!(
            folder_security_id(&record(1, IN_USE | DIRECTORY, 0, &[old])),
            None
        );
        assert_eq!(folder_security_id(&[0u8; 8]), None, "short buffer");
    }

    #[test]
    fn attributes_are_listed_with_their_names_and_malformed_ones_end_the_list() {
        let r = record(
            1,
            IN_USE,
            0,
            &[
                resident(0x10, "", &standard_info(1)),
                non_resident(0x80, "$SDS", 0, 4096, &[0x11, 0x01, 0x20]),
            ],
        );
        let a = attributes(&r);
        assert_eq!(a.len(), 2);
        assert_eq!(
            (a[0].kind, a[0].resident, a[0].name.as_str()),
            (0x10, true, "")
        );
        assert_eq!(
            (a[1].kind, a[1].resident, a[1].name.as_str()),
            (0x80, false, "$SDS")
        );
        assert_eq!(resident_value(&a[0]).map(<[u8]>::len), Some(0x48));
        let mut broken = r.clone();
        put32(&mut broken, 0x38 + 4, 0); // zero length: would loop forever
        assert_eq!(attributes(&broken).len(), 0);
        let mut long = r;
        put32(&mut long, 0x38 + 4, 5000); // past the record
        assert_eq!(attributes(&long).len(), 0);
    }

    #[test]
    fn data_runs_decode_relative_and_sparse_runs() {
        // 0x20 clusters at LCN 0x1000; 0x10 at 0x1000 - 0x100; 8 sparse
        let runs = [0x21, 0x20, 0x00, 0x10, 0x21, 0x10, 0x00, 0xFF, 0x01, 0x08];
        let r = record(1, IN_USE, 0, &[non_resident(0x80, "", 6, 123_456, &runs)]);
        let a = attributes(&r);
        let (vcn, real, got) = data_runs(&a[0]).unwrap();
        assert_eq!((vcn, real), (6, 123_456));
        assert_eq!(
            got,
            vec![
                Run {
                    lcn: Some(0x1000),
                    clusters: 0x20
                },
                Run {
                    lcn: Some(0x0F00),
                    clusters: 0x10
                },
                Run {
                    lcn: None,
                    clusters: 8
                },
            ]
        );
        let res = record(1, IN_USE, 0, &[resident(0x80, "", b"x")]);
        assert!(data_runs(&attributes(&res)[0]).is_none(), "resident");
        // a run whose sizes run past the attribute
        let cut = record(1, IN_USE, 0, &[non_resident(0x80, "", 0, 1, &[0x48, 1, 2])]);
        assert!(data_runs(&attributes(&cut)[0]).is_none());
    }

    fn list_entry(kind: u32, name: &str, record: u64) -> Vec<u8> {
        let name16: Vec<u16> = name.encode_utf16().collect();
        let len = (0x1A + 2 * name16.len() + 7) & !7;
        let mut e = vec![0u8; len];
        put32(&mut e, 0, kind);
        put16(&mut e, 4, len as u16);
        e[6] = name16.len() as u8;
        e[7] = 0x1A;
        put64(&mut e, 0x10, (3u64 << 48) | record);
        for (i, c) in name16.iter().enumerate() {
            put16(&mut e, 0x1A + 2 * i, *c);
        }
        e
    }

    #[test]
    fn the_attribute_list_names_the_records_holding_sds() {
        let list: Vec<u8> = [
            list_entry(0x10, "", 9),
            list_entry(0x80, "$SDS", 2141),
            list_entry(0x90, "$SDH", 9),
            list_entry(0x80, "$SDS", 291_150),
            list_entry(0x80, "$SDS", 9),
            list_entry(0x80, "$SDS", 2141),
        ]
        .concat();
        assert_eq!(listed_records(&list, 0x80, "$SDS", 9), vec![2141, 291_150]);
        assert!(
            listed_records(&list[..10], 0x80, "$SDS", 9).is_empty(),
            "cut list"
        );
    }

    /// A descriptor: revision 1, self-relative, owner SID only.
    fn descriptor(n: u8) -> Vec<u8> {
        let mut sd = vec![
            1, 0, 0x00, 0x80, 20, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ];
        sd.extend([1, 1, 0, 0, 0, 0, 0, 5, n, 0, 0, 0]);
        sd
    }

    fn put_entry(sds: &mut [u8], at: usize, id: u32, sd: &[u8]) -> usize {
        put32(sds, at, descriptor_hash(sd));
        put32(sds, at + 4, id);
        put64(sds, at + 8, at as u64);
        put32(sds, at + 16, (20 + sd.len()) as u32);
        sds[at + 20..at + 20 + sd.len()].copy_from_slice(sd);
        at + ((20 + sd.len() + 15) & !15)
    }

    /// Owner + DACL (one ACE granting Everyone), self-relative.
    fn owner_and_dacl() -> Vec<u8> {
        let owner = [1u8, 1, 0, 0, 0, 0, 0, 5, 18, 0, 0, 0];
        let ace = [
            0u8, 0x03, 20, 0, 0xFF, 0x01, 0x1F, 0x00, 1, 1, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0,
        ];
        let mut acl = vec![2u8, 0, 0, 0, 1, 0, 0, 0];
        acl.extend(ace);
        let acl_len = acl.len() as u16;
        acl[2..4].copy_from_slice(&acl_len.to_le_bytes());
        let mut sd = vec![1u8, 0, 0x04, 0x80];
        sd.extend(20u32.to_le_bytes()); // owner
        sd.extend(0u32.to_le_bytes()); // group
        sd.extend(0u32.to_le_bytes()); // SACL
        sd.extend(32u32.to_le_bytes()); // DACL
        sd.extend(owner);
        sd.extend(acl);
        sd
    }

    #[test]
    fn a_descriptor_whose_parts_lie_outside_it_is_refused() {
        let sd = owner_and_dacl();
        assert!(descriptor_in_bounds(&sd));
        assert!(descriptor_in_bounds(&descriptor(4)), "owner only");
        assert!(!descriptor_in_bounds(&sd[..sd.len() - 1]), "DACL cut short");
        assert!(!descriptor_in_bounds(&sd[..24]), "owner cut short");
        assert!(!descriptor_in_bounds(&sd[..10]), "header cut short");
        let mut far = sd.clone();
        far[16..20].copy_from_slice(&900u32.to_le_bytes());
        assert!(!descriptor_in_bounds(&far), "DACL offset past the end");
        let mut big = sd.clone();
        big[20 + 1] = 15; // owner claims 15 sub-authorities
        assert!(!descriptor_in_bounds(&big));
        let mut absolute = sd.clone();
        absolute[3] = 0; // not self-relative: offsets would be pointers
        assert!(!descriptor_in_bounds(&absolute));
        let mut rev = sd;
        rev[0] = 2;
        assert!(!descriptor_in_bounds(&rev));
    }

    #[test]
    fn the_descriptor_hash_is_ntfs_s() {
        // dword d: hash = d + rotl(hash, 3)
        let sd = [1u8, 0, 0, 0, 2, 0, 0, 0];
        assert_eq!(descriptor_hash(&sd), 2 + (1u32 << 3));
        assert_eq!(descriptor_hash(&[]), 0);
        assert_eq!(
            descriptor_hash(&[0xFF, 0xFF, 0xFF, 0xFF, 0, 0, 0, 0]),
            0xFFFF_FFFF
        );
    }

    #[test]
    fn sds_entries_are_found_past_holes_and_mirrors_are_not_counted_twice() {
        let mut sds = vec![0u8; 0xC0000];
        let mut at = put_entry(&mut sds, 0, 0x100, &descriptor(1));
        at = put_entry(&mut sds, at, 0x101, &descriptor(2));
        // a hole of zeros, then more entries in the same block
        at += 0x200;
        put_entry(&mut sds, at, 0x102, &descriptor(3));
        // the mirror of the first block
        let first: Vec<u8> = sds[..0x40000].to_vec();
        sds[0x40000..0x80000].copy_from_slice(&first);
        // the next block
        put_entry(&mut sds, 0x80000, 0x103, &descriptor(4));
        // a header-like pattern whose hash does not match
        let junk = put_entry(&mut sds, 0x80100, 0x104, &descriptor(5));
        sds[0x80100 + 20] ^= 0xFF;
        let _ = junk;
        let d = sds_descriptors(&sds);
        let mut ids: Vec<u32> = d.keys().copied().collect();
        ids.sort_unstable();
        assert_eq!(ids, vec![0x100, 0x101, 0x102, 0x103]);
        assert_eq!(d[&0x102], descriptor(3));
        assert!(
            sds_descriptors(&sds[..30]).is_empty(),
            "too short for an entry"
        );
        // one id with two different descriptors: neither is trusted
        let mut two = vec![0u8; 0x1000];
        let at = put_entry(&mut two, 0, 0x200, &descriptor(1));
        let at = put_entry(&mut two, at, 0x200, &descriptor(2));
        put_entry(&mut two, at, 0x201, &descriptor(3));
        let d = sds_descriptors(&two);
        assert!(!d.contains_key(&0x200));
        assert_eq!(d[&0x201], descriptor(3));
        // a length that is not whole dwords: the hash leaves bytes unchecked
        let mut odd = vec![0u8; 0x1000];
        let mut sd = descriptor(4);
        sd.push(7);
        put_entry(&mut odd, 0, 0x300, &sd);
        assert!(sds_descriptors(&odd).is_empty());
        // a torn entry is passed over whole, not searched inside for headers
        let mut torn = vec![0u8; 0x1000];
        let mut inner = descriptor(5);
        inner.resize(0x60, 0);
        put_entry(&mut torn, 0, 0x400, &inner);
        let fake = descriptor(6);
        put_entry(&mut torn, 0x20, 0x401, &fake); // a header inside its body
        put32(&mut torn, 0, 0xDEAD); // the outer entry's hash no longer matches
        assert!(!sds_descriptors(&torn).contains_key(&0x401));
    }
}
