//! Turkish-aware case and diacritic folding with a map back to the
//! original string's UTF-16 offsets (the UI highlights in JavaScript).

/// Folded text plus, for every folded byte, the UTF-16 offset and UTF-16
/// width of the original char that produced it.
pub struct Folded {
    pub text: String,
    pub orig_utf16: Vec<u32>,
    pub orig_len16: Vec<u8>,
}

fn is_combining_mark(c: char) -> bool {
    ('\u{300}'..='\u{36F}').contains(&c)
}

/// Folds one original char, calling `emit` for every folded char.
fn fold_char(c: char, mut emit: impl FnMut(char)) {
    match c {
        'İ' | 'I' | 'ı' => emit('i'),
        'Ç' | 'ç' => emit('c'),
        'Ş' | 'ş' => emit('s'),
        'Ğ' | 'ğ' => emit('g'),
        'Ö' | 'ö' => emit('o'),
        'Ü' | 'ü' => emit('u'),
        _ if c.is_ascii() => emit(c.to_ascii_lowercase()),
        _ => unicode_normalization::char::decompose_canonical(c, |d| {
            if !is_combining_mark(d) {
                d.to_lowercase().for_each(&mut emit);
            }
        }),
    }
}

pub fn fold(s: &str) -> Folded {
    let mut text = String::with_capacity(s.len());
    let mut orig_utf16 = Vec::with_capacity(s.len());
    let mut orig_len16 = Vec::with_capacity(s.len());
    let mut off16: u32 = 0;
    for c in s.chars() {
        let w = c.len_utf16() as u8;
        fold_char(c, |f| {
            text.push(f);
            for _ in 0..f.len_utf8() {
                orig_utf16.push(off16);
                orig_len16.push(w);
            }
        });
        off16 += w as u32;
    }
    Folded {
        text,
        orig_utf16,
        orig_len16,
    }
}

pub fn fold_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        fold_char(c, |f| out.push(f));
    }
    out
}

/// Which byte classes occur in a folded string: bits 0-25 `a`-`z`, 26
/// digits, 27 common separators (`_ - . space`), 28 other ASCII, 29
/// non-ASCII. If `char_mask(needle) & !char_mask(hay) != 0`, `hay` cannot
/// contain all of `needle`'s bytes.
/// Whether folded byte `p` of `fold_str(orig)` begins a camelCase word in
/// `orig`: a capital after a lowercase letter ("QuarterlyReport") or the
/// last capital of an acronym before a lowercase letter ("XMLRapor").
/// Folding lowercases names, so this reads the case from the original.
/// `p == 0` is never a camel start.
pub fn camel_start(orig: &str, p: usize) -> bool {
    CamelStarts::new(orig).at(p)
}

fn is_camel(a: char, c: char, next: Option<char>) -> bool {
    c.is_uppercase()
        && (a.is_lowercase() || (a.is_uppercase() && next.is_some_and(char::is_lowercase)))
}

/// [`camel_start`] for many positions of one name: O(1) per position for
/// ASCII names; other names are walked once, on the first query.
pub struct CamelStarts<'a> {
    orig: &'a str,
    ascii: Option<bool>,
    starts: Option<Vec<u32>>,
}

impl<'a> CamelStarts<'a> {
    pub fn new(orig: &'a str) -> Self {
        Self {
            orig,
            ascii: None,
            starts: None,
        }
    }

    pub fn at(&mut self, p: usize) -> bool {
        if p == 0 {
            return false;
        }
        let orig = self.orig;
        if *self.ascii.get_or_insert_with(|| orig.is_ascii()) {
            // ASCII folds byte for byte
            let b = orig.as_bytes();
            return p < b.len()
                && is_camel(
                    b[p - 1] as char,
                    b[p] as char,
                    b.get(p + 1).map(|&n| n as char),
                );
        }
        self.starts
            .get_or_insert_with(|| camel_offsets(orig))
            .binary_search(&(p as u32))
            .is_ok()
    }
}

/// Whether `orig` has any camelCase word start (see [`camel_start`]).
pub fn has_camel(orig: &str) -> bool {
    !camel_scan(orig, |_| false)
}

/// Folded byte offsets of every camelCase word start in `orig`, ascending.
fn camel_offsets(orig: &str) -> Vec<u32> {
    let mut out = Vec::new();
    camel_scan(orig, |off| {
        out.push(off);
        true
    });
    out
}

/// Calls `f` with the folded offset of each camelCase word start until it
/// returns `false`; returns `false` when stopped early.
fn camel_scan(orig: &str, mut f: impl FnMut(u32) -> bool) -> bool {
    let mut prev: Option<char> = None;
    let mut off: u32 = 0;
    let mut chars = orig.chars().peekable();
    while let Some(c) = chars.next() {
        let n = if c.is_ascii() {
            1
        } else {
            let mut n = 0;
            fold_char(c, |f| n += f.len_utf8() as u32);
            n
        };
        if n == 0 {
            continue; // a combining mark folds away and does not split words
        }
        if prev.is_some_and(|a| is_camel(a, c, chars.peek().copied())) && !f(off) {
            return false;
        }
        off += n;
        prev = Some(c);
    }
    true
}

pub fn char_mask(folded: &str) -> u32 {
    folded.bytes().fold(0, |m, b| {
        m | 1
            << match b {
                b'a'..=b'z' => (b - b'a') as u32,
                b'0'..=b'9' => 26,
                b'_' | b'-' | b'.' | b' ' => 27,
                0..=127 => 28,
                _ => 29,
            }
    })
}

/// Merges folded byte positions into half-open UTF-16 ranges on the
/// original string. Out-of-range positions are ignored.
pub fn utf16_ranges(f: &Folded, folded_byte_positions: &[u32]) -> Vec<(u32, u32)> {
    let mut spans: Vec<(u32, u32)> = folded_byte_positions
        .iter()
        .filter_map(|&p| {
            let p = p as usize;
            Some((
                *f.orig_utf16.get(p)?,
                f.orig_utf16[p] + f.orig_len16[p] as u32,
            ))
        })
        .collect();
    spans.sort_unstable();
    let mut out: Vec<(u32, u32)> = Vec::with_capacity(spans.len());
    for (a, b) in spans {
        match out.last_mut() {
            Some(last) if a <= last.1 => last.1 = last.1.max(b),
            _ => out.push((a, b)),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn camel_starts() {
        assert!(camel_start("QuarterlyReport", 9));
        assert!(!camel_start("Quarterlyreport", 9));
        assert!(camel_start("XMLRapor", 3));
        assert!(!camel_start("XMLRapor", 1)); // inside the acronym
        assert!(!camel_start("Report", 0));
        // Turkish letters before the boundary: folded offsets, not bytes
        let o = "tipiYıllıkTalep";
        let f = fold_str(o);
        assert!(camel_start(o, f.find("talep").unwrap()));
        assert!(camel_start(o, f.find("yil").unwrap()));
        assert!(!camel_start(o, f.find("lik").unwrap()));
        // a combining mark before the capital does not hide the boundary
        let o = "Cafe\u{301}Rapor";
        assert!(camel_start(o, fold_str(o).find("rapor").unwrap()));
    }

    #[test]
    fn turkish_i_family_folds_to_i() {
        assert_eq!(fold_str("İIıi"), "iiii");
        assert_eq!(fold_str("IŞIK"), "isik");
        assert_eq!(fold_str("Işık_Raporu"), "isik_raporu");
    }
    #[test]
    fn turkish_letters() {
        assert_eq!(fold_str("ÇçŞşĞğÖöÜü"), "ccssggoouu");
    }
    #[test]
    fn butce() {
        assert_eq!(fold_str("Bütçe_2026_Q3.xlsx"), "butce_2026_q3.xlsx");
    }
    #[test]
    fn nfd_input() {
        assert_eq!(fold_str("Cafe\u{301}.txt"), "cafe.txt");
    }
    #[test]
    fn precomposed_other_latin() {
        assert_eq!(fold_str("Ñandú Ålborg"), "nandu alborg");
    }
    #[test]
    fn astral_kept_and_mapped() {
        let f = fold("🎵 müzik");
        assert!(f.text.ends_with(" muzik"));
        let m = f.text.find('m').unwrap();
        assert_eq!(f.orig_utf16[m], 3);
    }
    #[test]
    fn ranges_merge_and_cover_astral() {
        let f = fold("Bütçe");
        assert_eq!(utf16_ranges(&f, &[0, 1, 2, 3, 4]), vec![(0, 5)]);
        let g = fold("🎵a");
        let e: Vec<u32> = (0..g.text.find('a').unwrap() as u32).collect();
        assert_eq!(utf16_ranges(&g, &e), vec![(0, 2)]);
    }
    #[test]
    fn mask_rejects_missing_letters() {
        let hay = char_mask("butce_2026.xlsx");
        assert_eq!(char_mask("butce") & !hay, 0);
        assert_ne!(char_mask("zq") & !hay, 0);
        assert_eq!(char_mask("26") & !hay, 0);
        assert_ne!(char_mask("müzik") & !char_mask("muzik"), 0); // unfolded ü is non-ASCII
    }

    #[test]
    fn decomposed_mark_maps_to_base_char() {
        let f = fold("Cafe\u{301}");
        assert_eq!(utf16_ranges(&f, &[3]), vec![(3, 4)]);
    }
}
