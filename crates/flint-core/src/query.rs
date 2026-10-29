//! Query parsing: AND-ed terms, quoted phrases, and optional
//! `d:` / `ext:` / trailing `/` filters.

use crate::fold::fold_str;

pub struct Term {
    pub text: String,
    pub phrase: bool,
}

pub struct Query {
    pub terms: Vec<Term>,
    pub drive: Option<char>,
    pub ext: Option<String>,
    pub folders_only: bool,
}

impl Query {
    /// No terms, no drive and no extension. `folders_only` alone does
    /// not make a query searchable.
    pub fn is_empty(&self) -> bool {
        self.terms.is_empty() && self.drive.is_none() && self.ext.is_none()
    }
}

const MAX_QUERY_CHARS: usize = 256;

pub fn parse(q: &str) -> Query {
    let q: String = q.chars().take(MAX_QUERY_CHARS).collect();
    let mut out = Query {
        terms: Vec::new(),
        drive: None,
        ext: None,
        folders_only: false,
    };
    let mut chars = q.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
        } else if c == '"' {
            chars.next();
            let phrase: String = chars.by_ref().take_while(|&c| c != '"').collect();
            let text = fold_str(&phrase);
            if !text.trim().is_empty() {
                out.terms.push(Term { text, phrase: true });
            }
        } else {
            let mut tok = String::new();
            while let Some(&c) = chars.peek() {
                if c.is_whitespace() || c == '"' {
                    break;
                }
                tok.push(c);
                chars.next();
            }
            word(&mut out, &tok);
        }
    }
    out
}

fn word(out: &mut Query, tok: &str) {
    let b = tok.as_bytes();
    if b.len() == 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
        out.drive = Some(b[0].to_ascii_lowercase() as char);
        return;
    }
    if b.len() >= 4 && b[..4].eq_ignore_ascii_case(b"ext:") {
        let v = fold_str(tok[4..].trim_start_matches('.'));
        if !v.is_empty() {
            out.ext = Some(v);
        }
        return;
    }
    let mut t = tok;
    if t.ends_with('/') {
        out.folders_only = true;
        t = t.trim_end_matches('/');
    }
    let text = fold_str(t);
    if !text.is_empty() {
        out.terms.push(Term {
            text,
            phrase: false,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(q: &Query) -> Vec<&str> {
        q.terms.iter().map(|t| t.text.as_str()).collect()
    }

    #[test]
    fn words_are_and_terms() {
        let q = parse("Finans  Bütçe");
        assert_eq!(texts(&q), ["finans", "butce"]);
    }
    #[test]
    fn quoted_phrase_is_one_term() {
        let q = parse("\"aile bütçesi\" ods");
        assert_eq!(texts(&q), ["aile butcesi", "ods"]);
        assert!(q.terms[0].phrase);
    }
    #[test]
    fn unbalanced_quote_runs_to_end() {
        assert_eq!(texts(&parse("\"aile büt")), ["aile but"]);
    }
    #[test]
    fn drive_prefix() {
        let q = parse("D: rapor");
        assert_eq!(q.drive, Some('d'));
        assert_eq!(texts(&q), ["rapor"]);
    }
    #[test]
    fn ext_prefix() {
        let q = parse("butce ext:.XLSX");
        assert_eq!(q.ext.as_deref(), Some("xlsx"));
    }
    #[test]
    fn empty_ext_ignored() {
        let q = parse("ext: butce");
        assert_eq!(q.ext, None);
        assert_eq!(texts(&q), ["butce"]);
    }
    #[test]
    fn trailing_slash_folders_only() {
        let q = parse("yedek/");
        assert!(q.folders_only);
        assert_eq!(texts(&q), ["yedek"]);
    }
    #[test]
    fn degenerate_inputs_are_empty() {
        for s in ["", "   ", "\"", "\"\"", "ext:", "/"] {
            assert!(parse(s).is_empty(), "{s:?}");
        }
    }
    #[test]
    fn punctuation_survives_as_term() {
        assert_eq!(texts(&parse("c++")), ["c++"]);
    }
    #[test]
    fn long_query_truncated_to_256_chars() {
        let q = parse(&"a".repeat(300));
        assert_eq!(q.terms[0].text.len(), 256);
    }
}
