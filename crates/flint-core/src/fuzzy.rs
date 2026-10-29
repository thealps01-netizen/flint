//! Fuzzy tier: nucleo-matcher over pre-folded names, with a path-only
//! fallback over ancestor folder names.

use crate::index::{EntryId, Index};
use crate::query::Term;
use nucleo_matcher::pattern::{Atom, AtomKind, CaseMatching, Normalization};
use nucleo_matcher::{Config, Matcher, Utf32Str};

pub struct TermMatch {
    pub score: u32,
    pub in_name: bool,
    /// Folded byte positions in the name (first byte of each matched char);
    /// empty for a path-only match.
    pub name_positions: Vec<u32>,
}

/// Upper bound of nucleo's per-char score: SCORE_MATCH (16) plus the
/// largest bonus (boundary-after-whitespace, 10) doubled for the first
/// char. Pinned by `max_score_bounds_nucleo`.
const MAX_CHAR_SCORE: u32 = 16 + 2 * 10;

/// The nucleo atom used for `term` (fuzzy, or substring for a phrase).
pub fn atom(term: &Term) -> Atom {
    let kind = if term.phrase {
        AtomKind::Substring
    } else {
        AtomKind::Fuzzy
    };
    Atom::new(
        &term.text,
        CaseMatching::Ignore,
        Normalization::Never,
        kind,
        false,
    )
}

/// Largest score `score_folded` can return for `term`.
pub fn max_score(term: &Term) -> u32 {
    term.text.chars().count() as u32 * MAX_CHAR_SCORE
}

/// Terms of up to this many chars must occur as a contiguous substring.
pub const SUBSTRING_ONLY_LEN: usize = 2;
/// A longer term's chars must occur in order inside a window of at most
/// `len * WINDOW_FACTOR` chars; letters spread over a long name are noise.
pub const WINDOW_FACTOR: usize = 3;

/// Acceptance rule of the fuzzy tier; nucleo only scores what this
/// accepts (it is stricter than nucleo, never looser). Phrases and short
/// terms need a substring; longer terms a compact subsequence. Both sides
/// are already folded. For an ASCII term the window is measured in bytes,
/// so non-ASCII chars inside a match make it slightly stricter.
pub fn quick_match(folded: &str, term: &Term) -> bool {
    let needle = term.text.as_bytes();
    if term.phrase || term.text.chars().count() <= SUBSTRING_ONLY_LEN {
        return memchr::memmem::find(folded.as_bytes(), needle).is_some();
    }
    if term.text.is_ascii() {
        // ASCII bytes never occur inside a multi-byte UTF-8 sequence.
        compact_subsequence(folded.as_bytes(), needle, needle.len() * WINDOW_FACTOR)
    } else {
        let hay: Vec<char> = folded.chars().collect();
        let needle: Vec<char> = term.text.chars().collect();
        compact_subsequence(&hay, &needle, needle.len() * WINDOW_FACTOR)
    }
}

/// `needle` occurs in order inside some window of `hay` at most `window`
/// long. A greedy forward scan from a start gives that start's shortest
/// window, so trying every start of `needle[0]` is exact.
fn compact_subsequence<T: PartialEq + Copy>(hay: &[T], needle: &[T], window: usize) -> bool {
    let Some((&first, rest)) = needle.split_first() else {
        return true;
    };
    // cheap reject: not a subsequence at all
    let mut j = 0;
    for &h in hay {
        if h == needle[j] {
            j += 1;
            if j == needle.len() {
                break;
            }
        }
    }
    if j < needle.len() {
        return false;
    }
    for start in 0..hay.len() {
        if hay[start] != first {
            continue;
        }
        let limit = (start + window).min(hay.len());
        let mut k = 0;
        for &h in &hay[start + 1..limit] {
            if k == rest.len() {
                break;
            }
            if h == rest[k] {
                k += 1;
            }
        }
        if k == rest.len() {
            return true;
        }
        if hay.len() - start < needle.len() {
            return false;
        }
    }
    false
}

const MAX_CACHED_ATOMS: usize = 64;

/// Path-only matches are worth this fraction of a name match.
const PATH_ONLY_DIVISOR: u32 = 4;

/// One per thread: owns the nucleo matcher, scratch buffers and the
/// compiled atoms of the terms seen so far.
pub struct FuzzyMatcher {
    matcher: Matcher,
    chars: Vec<char>,
    indices: Vec<u32>,
    atoms: Vec<(String, bool, Atom)>,
}

impl Default for FuzzyMatcher {
    fn default() -> Self {
        Self::new()
    }
}

/// Builds a haystack whose char indices are plain `char` indices of
/// `folded` (nucleo's own constructor groups graphemes, which would
/// shift indices).
fn haystack<'a>(folded: &'a str, chars: &'a mut Vec<char>) -> Utf32Str<'a> {
    if folded.is_ascii() {
        Utf32Str::Ascii(folded.as_bytes())
    } else {
        chars.clear();
        chars.extend(folded.chars());
        Utf32Str::Unicode(chars)
    }
}

impl FuzzyMatcher {
    pub fn new() -> Self {
        Self {
            matcher: Matcher::new(Config::DEFAULT),
            chars: Vec::new(),
            indices: Vec::new(),
            atoms: Vec::new(),
        }
    }

    fn atom_index(&mut self, term: &Term) -> usize {
        if let Some(i) = self
            .atoms
            .iter()
            .position(|(t, p, _)| *p == term.phrase && *t == term.text)
        {
            return i;
        }
        if self.atoms.len() >= MAX_CACHED_ATOMS {
            self.atoms.clear(); // long-lived per-thread matchers see every query
        }
        let kind = if term.phrase {
            AtomKind::Substring
        } else {
            AtomKind::Fuzzy
        };
        let atom = Atom::new(
            &term.text,
            CaseMatching::Ignore,
            Normalization::Never,
            kind,
            false,
        );
        self.atoms.push((term.text.clone(), term.phrase, atom));
        self.atoms.len() - 1
    }

    /// Score-only match of a prepared atom (see [`atom`]) against an
    /// already folded string; the hot path of `search`.
    pub fn score_atom(&mut self, folded: &str, atom: &Atom) -> Option<u32> {
        atom.score(haystack(folded, &mut self.chars), &mut self.matcher)
            .map(u32::from)
    }

    /// Score-only nucleo match of `term` against an already folded string
    /// (no positions, no compactness rule).
    pub fn score_folded(&mut self, folded: &str, term: &Term) -> Option<u32> {
        let ai = self.atom_index(term);
        let Self {
            matcher,
            chars,
            atoms,
            ..
        } = self;
        atoms[ai]
            .2
            .score(haystack(folded, chars), matcher)
            .map(u32::from)
    }

    pub fn match_term(&mut self, ix: &Index, id: EntryId, term: &Term) -> Option<TermMatch> {
        let ai = self.atom_index(term);
        let Self {
            matcher,
            chars,
            indices,
            atoms,
        } = self;
        let atom = &atoms[ai].2;

        let folded = ix.folded(id);
        indices.clear();
        if !quick_match(folded, term) {
            // fall through to the folders
        } else if let Some(score) = atom.indices(haystack(folded, chars), matcher, indices) {
            indices.sort_unstable();
            indices.dedup();
            let name_positions = if folded.is_ascii() {
                indices.clone()
            } else {
                let starts: Vec<u32> = folded.char_indices().map(|(b, _)| b as u32).collect();
                indices
                    .iter()
                    .filter_map(|&c| starts.get(c as usize).copied())
                    .collect()
            };
            return Some(TermMatch {
                score: score as u32,
                in_name: true,
                name_positions,
            });
        }

        let mut best: Option<u32> = None;
        for anc in ix.ancestors(id) {
            if ix.parent(anc).is_none() {
                break; // the drive root is never a match target
            }
            if !quick_match(ix.folded(anc), term) {
                continue;
            }
            if let Some(s) = atom.score(haystack(ix.folded(anc), chars), matcher) {
                best = Some(best.map_or(s as u32, |b| b.max(s as u32)));
            }
        }
        best.map(|s| TermMatch {
            score: s / PATH_ONLY_DIVISOR,
            in_name: false,
            name_positions: Vec::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fold::{fold, utf16_ranges};
    use crate::index::EntryInput;
    use crate::query::parse;

    /// C:\Users\Deniz\Documents\Finans\2026\Bütçe_2026_Q3.xlsx and friends.
    fn fixture() -> Index {
        let mut ix = Index::new();
        let d = |ix: &mut Index, n: &str, p: Option<EntryId>| {
            ix.insert(EntryInput {
                name: n,
                parent: p,
                is_dir: true,
                last_change: 0,
            })
        };
        let c = d(&mut ix, "C:", None);
        let u = d(&mut ix, "Users", Some(c));
        let dz = d(&mut ix, "Deniz", Some(u));
        let docs = d(&mut ix, "Documents", Some(dz));
        let fin = d(&mut ix, "Finans", Some(docs));
        let y = d(&mut ix, "2026", Some(fin));
        for (n, p) in [
            ("Bütçe_2026_Q3.xlsx", y),
            ("Işık_Raporu.docx", docs),
            ("ışık.txt", docs),
            ("aile-butcesi.ods", docs),
            ("Finans_Rapor.pdf", docs),
            ("🎵 müzik.mp3", docs),
            ("x.txt", c),
        ] {
            ix.insert(EntryInput {
                name: n,
                parent: Some(p),
                is_dir: false,
                last_change: 0,
            });
        }
        let long = "a".repeat(250) + ".txt";
        ix.insert(EntryInput {
            name: &long,
            parent: Some(docs),
            is_dir: false,
            last_change: 0,
        });
        ix
    }

    fn id_of(ix: &Index, name: &str) -> EntryId {
        (0..ix.raw_len() as u32)
            .map(EntryId)
            .find(|&i| ix.name(i) == name)
            .unwrap()
    }
    fn mt_opt(q: &str, name: &str) -> Option<TermMatch> {
        let ix = fixture();
        let t = parse(q).terms.remove(0);
        FuzzyMatcher::new().match_term(&ix, id_of(&ix, name), &t)
    }
    fn mt(q: &str, name: &str) -> TermMatch {
        mt_opt(q, name).expect("expected a match")
    }

    #[test]
    fn ascii_query_matches_turkish_name() {
        let m = mt("butce", "Bütçe_2026_Q3.xlsx");
        assert!(m.in_name);
        assert_eq!(m.name_positions, vec![0, 1, 2, 3, 4]);
    }
    #[test]
    fn turkish_caps_query() {
        assert!(mt("IŞIK", "Işık_Raporu.docx").in_name);
        assert!(mt("işık", "ışık.txt").in_name);
    }
    #[test]
    fn fuzzy_gaps() {
        assert!(mt("bt26", "Bütçe_2026_Q3.xlsx").in_name);
    }
    #[test]
    fn path_only_match() {
        let m = mt("finans", "Bütçe_2026_Q3.xlsx");
        assert!(!m.in_name);
        assert!(m.name_positions.is_empty());
    }
    #[test]
    fn root_not_matched() {
        assert!(mt_opt("c", "x.txt").is_none());
    }
    #[test]
    fn phrase_requires_contiguous() {
        assert!(mt_opt("\"aile butcesi\"", "aile-butcesi.ods").is_none());
        assert!(mt_opt("\"aile-butcesi\"", "aile-butcesi.ods").is_some());
    }
    #[test]
    fn name_beats_path_score() {
        let named = mt("finans", "Finans_Rapor.pdf");
        let path_only = mt("finans", "Bütçe_2026_Q3.xlsx");
        assert!(named.in_name && !path_only.in_name);
        assert!(named.score > path_only.score);
    }
    #[test]
    fn long_name_no_panic() {
        let n = "a".repeat(250) + ".txt";
        assert!(mt("aaaa", &n).in_name);
    }
    fn qm(q: &str, name: &str) -> bool {
        let term = parse(q).terms.remove(0);
        quick_match(&crate::fold::fold_str(name), &term)
    }

    #[test]
    fn scattered_subsequence_rejected() {
        assert!(!qm("rapor", "rapid-alpha-photo-renderer.exe"));
        assert!(qm("rapro", "rapor.ods")); // r-a-p-o-r-.-o: compact
        assert!(qm("bt26", "Bütçe_2026_Q3.xlsx"));
        assert!(mt_opt("rapor", "Finans_Rapor.pdf").is_some());
    }

    #[test]
    fn short_terms_need_a_substring() {
        assert!(!qm("ra", "rxa.txt"));
        assert!(qm("ra", "abra.txt"));
        assert!(qm("r", "x_r.txt"));
    }

    #[test]
    fn quick_match_agrees_with_nucleo_and_bound_holds() {
        let names = [
            "bütçe_2026_q3.xlsx",
            "aile-butcesi.ods",
            "🎵 müzik.mp3",
            "rapor_proje.docx",
            "a b c d e",
            "zzz",
            "işık raporu",
            "日本語ファイル.txt",
            "budget cut eh.pdf",
        ];
        let queries = [
            "butce",
            "bt26",
            "muzik",
            "rapro",
            "\"aile-butcesi\"",
            "e",
            "日本",
            "isik r",
            "zz",
        ];
        let mut m = FuzzyMatcher::new();
        for n in names {
            let folded = crate::fold::fold_str(n);
            for q in queries {
                for term in parse(q).terms {
                    let s = m.score_folded(&folded, &term);
                    // the acceptance rule is stricter than nucleo, never looser
                    if quick_match(&folded, &term) {
                        assert!(s.is_some(), "{n} / {q}");
                    }
                    if let Some(s) = s {
                        assert!(
                            s <= max_score(&term),
                            "{n} / {q}: {s} > {}",
                            max_score(&term)
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn astral_highlight_roundtrip() {
        let name = "🎵 müzik.mp3";
        let m = mt("muzik", name);
        assert_eq!(utf16_ranges(&fold(name), &m.name_positions), vec![(3, 8)]);
    }
}
