//! Typo tier: edit distance <= 1 (insertion, deletion, substitution or
//! adjacent transposition) at the start of a word.
//!
//! Follows common search-engine practice (Meilisearch's defaults): one
//! typo only in terms of 5+ chars, and never in the first letter. The
//! match must also start a word; a typo match in the middle of a word is
//! almost always noise ("node" would find "code"), and anchoring keeps
//! the check to a few positions per name. A camelCase boundary in the
//! original name ("QuarterlyReport") starts a word too.

/// Shortest needle (in bytes) the typo tier accepts.
pub const MIN_TYPO_LEN: usize = 5;

use memchr::memmem::Finder;

/// Byte span of the leftmost word-start substring of `hay` within distance
/// 1 of `needle` and sharing its first byte, or `None` when `needle` is
/// shorter than [`MIN_TYPO_LEN`] or occurs exactly (an exact hit belongs
/// to the fuzzy tier).
///
/// A span equal to `needle` minus its last byte is not a typo hit: every
/// name containing "rapo" would otherwise match "rapor".
///
/// Without the original name, only separators and letter/digit changes
/// start words; see [`ApproxFinder::find`] for camelCase.
pub fn approx_find(hay: &[u8], needle: &[u8]) -> Option<(usize, usize)> {
    ApproxFinder::new(needle)?.find(hay, "")
}

/// A word starts at the beginning, after a non-alphanumeric ASCII byte,
/// or at a letter/digit change ("2026rapor"). Bytes of non-ASCII chars
/// count as letters.
fn word_start(hay: &[u8], p: usize) -> bool {
    if p == 0 {
        return true;
    }
    let (a, b) = (hay[p - 1], hay[p]);
    if !a.is_ascii() {
        return false;
    }
    !a.is_ascii_alphanumeric() || a.is_ascii_digit() != b.is_ascii_digit()
}

/// [`approx_find`] with its searchers built once per query.
pub struct ApproxFinder {
    needle: Vec<u8>,
    whole: Finder<'static>,
}

impl ApproxFinder {
    /// `None` when `needle` is shorter than [`MIN_TYPO_LEN`].
    pub fn new(needle: &[u8]) -> Option<Self> {
        let m = needle.len();
        if m < MIN_TYPO_LEN {
            return None;
        }
        Some(Self {
            needle: needle.to_vec(),
            whole: Finder::new(needle).into_owned(),
        })
    }

    /// `hay` is the folded name, `orig` the original one (for camelCase
    /// word starts; `""` when unknown).
    pub fn find(&self, hay: &[u8], orig: &str) -> Option<(usize, usize)> {
        let needle = &self.needle[..];
        // candidates first: a word start with the right first letter is
        // rare, so the full exact scan runs only for names that have one
        let mut camel = crate::fold::CamelStarts::new(orig);
        let hit = memchr::memchr_iter(needle[0], hay)
            .filter(|&p| word_start(hay, p) || camel.at(p))
            .find_map(|p| self.at(hay, p).map(|len| (p, p + len)))?;
        self.whole.find(hay).is_none().then_some(hit)
    }

    /// Same as [`find`](Self::find) restricted to a span starting at byte
    /// 0. The exact-occurrence check is left to the caller (in `search`,
    /// an exact occurrence is a fuzzy match and never reaches this).
    pub fn find_at_start(&self, hay: &[u8]) -> Option<(usize, usize)> {
        (hay.first() == Some(&self.needle[0]))
            .then(|| self.at(hay, 0).map(|len| (0, len)))
            .flatten()
    }

    /// Length of the span at `p` within distance 1 of the needle.
    fn at(&self, hay: &[u8], p: usize) -> Option<usize> {
        let needle = &self.needle[..];
        let m = needle.len();
        [m, m + 1, m - 1].into_iter().find(|&len| {
            hay.get(p..p + len)
                .is_some_and(|span| span != &needle[..m - 1] && within_one(span, needle))
        })
    }
}

/// Optimal-string-alignment distance(a, b) <= 1, in linear time.
fn within_one(a: &[u8], b: &[u8]) -> bool {
    let (long, short) = if a.len() >= b.len() { (a, b) } else { (b, a) };
    let i = long.iter().zip(short).take_while(|(x, y)| x == y).count();
    match long.len() - short.len() {
        0 => {
            i == long.len()
                || long[i + 1..] == short[i + 1..]
                || (i + 1 < long.len()
                    && long[i] == short[i + 1]
                    && long[i + 1] == short[i]
                    && long[i + 2..] == short[i + 2..])
        }
        1 => long[i + 1..] == short[i..],
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A camelCase boundary in the original name starts a word, although
    /// folding lowercased it; a lowercase letter inside a word does not.
    #[test]
    fn camel_case_word_starts() {
        let f = ApproxFinder::new(b"reprot").unwrap();
        assert_eq!(
            f.find(b"quarterlyreport.xlsx", "QuarterlyReport.xlsx"),
            Some((9, 15))
        );
        assert_eq!(f.find(b"unreported.txt", "Unreported.txt"), None);
        let r = ApproxFinder::new(b"rapro").unwrap();
        assert_eq!(r.find(b"xmlrapor.txt", "XMLRapor.txt"), Some((3, 8)));
        assert_eq!(r.find(b"xmlrapor.txt", "xmlrapor.txt"), None);
        // non-ASCII letters before the boundary shift folded vs original bytes
        let orig = "tipiYıllıkTalep.txt";
        let hay = crate::fold::fold_str(orig);
        let at = hay.find("talep").unwrap();
        let t = ApproxFinder::new(b"talpe").unwrap();
        assert_eq!(t.find(hay.as_bytes(), orig), Some((at, at + 5)));
    }
    #[test]
    fn transposition() {
        assert_eq!(approx_find(b"rapor_2026.xlsx", b"rapro"), Some((0, 5)));
    }
    #[test]
    fn substitution() {
        assert!(approx_find(b"butce_ozet.pdf", b"bitce").is_some());
    }
    #[test]
    fn insertion() {
        assert!(approx_find(b"sunum.pptx", b"sunnum").is_some());
    }
    #[test]
    fn deletion() {
        assert_eq!(approx_find(b"taslak.docx", b"tslak"), Some((0, 6)));
        assert!(approx_find(b"sunumlar.pptx", b"sunmlar").is_some());
    }
    #[test]
    fn first_letter_must_match() {
        assert!(approx_find(b"tapor.txt", b"rapor").is_none());
        assert!(approx_find(b"arpor.txt", b"rapor").is_none());
    }
    #[test]
    fn only_at_word_starts() {
        assert!(approx_find(b"brapro.txt", b"rapor").is_none());
        assert_eq!(approx_find(b"eski_rapro.txt", b"rapor"), Some((5, 10)));
        assert_eq!(approx_find(b"2026rapro.pdf", b"rapor"), Some((4, 9)));
    }
    #[test]
    fn four_letter_terms_have_no_typo_tier() {
        assert!(approx_find(b"code.txt", b"node").is_none());
    }
    #[test]
    fn two_edits_rejected() {
        assert!(approx_find(b"rapor.pdf", b"rpaor_").is_none());
    }
    #[test]
    fn short_needle_rejected() {
        assert!(approx_find(b"abc.txt", b"abd").is_none());
    }
    #[test]
    fn exact_is_not_typo() {
        assert!(approx_find(b"rapor.pdf", b"rapor").is_none());
    }
    #[test]
    fn span_is_within_hay() {
        let (a, b) = approx_find(b"x_sunm", b"sunum").unwrap();
        assert!(a < b && b <= 6);
        assert_eq!(&b"x_sunm"[a..b], b"sunm");
    }
    #[test]
    fn find_at_start_agrees() {
        let f = ApproxFinder::new(b"rapor").unwrap();
        assert_eq!(f.find_at_start(b"rapro_x"), f.find(b"rapro_x", ""));
        assert_eq!(f.find_at_start(b"x_rapro"), None);
        assert_eq!(f.find_at_start(b""), None);
    }
    #[test]
    fn short_hay_at_the_end() {
        assert_eq!(approx_find(b"a_rapr", b"rapor"), Some((2, 6)));
        assert!(approx_find(b"r", b"rapor").is_none());
    }
}
