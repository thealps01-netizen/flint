//! MCP server (Model Context Protocol) over stdio: lets an AI assistant
//! (Hermes, Claude Code, Codex, ...) find files by name through Flint.
//!
//! One read-only tool, `search`: names and full paths of files and folders
//! the current Windows user may see, never their contents. No network: the
//! assistant starts `flint-indexer mcp` and talks over its stdin/stdout;
//! it ends when stdin closes. Folders excluded in Flint's settings, and
//! those hidden from assistants there, are never returned. Logs go to
//! stderr and never name a file.
//!
//! Both protocol eras are served: the `initialize` handshake (2024-11-05
//! to 2025-11-25) and per-request `_meta` versions (2026-07-28).

use crate::ask::Found;
use crate::protocol::WireHit;
use serde_json::{Map, Value, json};
use std::collections::VecDeque;
use std::io::{self, BufRead, Write};
use std::time::{Duration, Instant};

/// The per-request (modern) protocol version served.
pub const MODERN: &str = "2026-07-28";
/// Handshake (legacy) versions served, newest first.
pub const LEGACY: [&str; 4] = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];
pub const TOOL: &str = "search";
pub const DEFAULT_LIMIT: u32 = 20;
pub const MAX_RESULTS: u32 = 50;
/// A longer input line is refused (and skipped) without being parsed.
pub const MAX_LINE: usize = 1 << 20;
/// At most this many searches in [`RATE_WINDOW`]; more are refused.
pub const RATE_LIMIT: usize = 60;
pub const RATE_WINDOW: Duration = Duration::from_secs(60);

/// What answers a search: `text` as typed, at most `limit` hits, hidden
/// folders already dropped.
pub trait Searcher {
    fn search(&self, text: &str, limit: u32) -> Result<Found, String>;
}

const VERSION_KEY: &str = "io.modelcontextprotocol/protocolVersion";
const MAX_QUERY_CHARS: usize = 256;
/// With a kind or folder filter, this many times the limit is asked for,
/// so filtering still leaves enough.
const FILTER_FETCH: u32 = 4;
/// With a folder to search under, this many of the query's best matches
/// are asked for (the most an index answers).
const FILTER_FETCH_MAX: u32 = 200;

const DESCRIPTION: &str = "Find files and folders on this Windows PC by name, instantly, from \
Flint's index of the local drives (use it before walking folders or running a file search). \
Forgives typos, missing accents and Turkish letters; words match in any order; \"a quoted \
phrase\" matches as one; ext:pdf keeps one extension; C: keeps one drive; a word ending in / \
matches folders. Returns names, full paths, kind and last change only, never file contents, \
and only what this Windows user may open.";

const INSTRUCTIONS: &str = "Use the search tool to locate local files or folders by name; it \
answers in milliseconds for the whole PC, much faster than walking directories.";

pub struct Server<S> {
    searcher: S,
    /// `initialize` was answered (legacy era).
    initialized: bool,
    calls: VecDeque<Instant>,
}

/// How a request is answered.
#[derive(Clone, Copy, PartialEq)]
enum Era {
    Legacy,
    Modern,
}

fn rpc_error(id: &Value, code: i64, message: &str, data: Option<Value>) -> Value {
    let mut e = json!({"code": code, "message": message});
    if let Some(d) = data {
        e["data"] = d;
    }
    json!({"jsonrpc": "2.0", "id": id, "error": e})
}

fn tool() -> Value {
    json!({
        "name": TOOL,
        "title": "Flint file search",
        "description": DESCRIPTION,
        "inputSchema": {
            "type": "object",
            "properties": {
                "query": {"type": "string", "minLength": 1, "maxLength": MAX_QUERY_CHARS,
                          "description": "Part of the name, as you would type it in Flint."},
                "limit": {"type": "integer", "minimum": 1, "maximum": MAX_RESULTS, "default": DEFAULT_LIMIT,
                          "description": "Most results to return."},
                "kind": {"type": "string", "enum": ["file", "folder"],
                         "description": "Only files or only folders."},
                "under": {"type": "string",
                          "description": "Only results inside this absolute folder, e.g. C:\\Users\\me\\Projects."}
            },
            "required": ["query"],
            "additionalProperties": false
        },
        "outputSchema": {
            "type": "object",
            "properties": {
                "results": {"type": "array", "items": {
                    "type": "object",
                    "properties": {
                        "name": {"type": "string"},
                        "path": {"type": "string"},
                        "kind": {"type": "string", "enum": ["file", "folder"]},
                        "modified": {"type": "string", "description": "Last change, UTC (ISO 8601)."},
                        "cleaned": {"type": "boolean", "description": "Invisible or direction characters in the name or path were replaced by U+FFFD; the path will not open as shown."}
                    },
                    "required": ["name", "path", "kind"]
                }},
                "shown": {"type": "integer"},
                "matches": {"type": "integer", "description": "Matches counted before the limit; when matchesExact is false, there are at least this many."},
                "matchesExact": {"type": "boolean", "description": "Whether matches is the full count (false: a lower bound, as folders hidden from assistants are left out or only the best matches were checked)."},
                "partial": {"type": "boolean", "description": "One of Flint's indexes did not answer, so results may be missing."},
                "note": {"type": "string"}
            },
            "required": ["results", "shown", "matches", "matchesExact", "partial"]
        },
        "annotations": {
            "readOnlyHint": true,
            "destructiveHint": false,
            "idempotentHint": true,
            "openWorldHint": false
        }
    })
}

fn server_info() -> Value {
    json!({"name": "flint", "title": "Flint", "version": env!("CARGO_PKG_VERSION")})
}

fn tool_error(text: &str) -> Value {
    json!({"content": [{"type": "text", "text": text}], "isError": true})
}

/// Whether `c` could hide text or make a name read as something else:
/// controls, invisible format characters (zero-width, soft hyphen, word
/// joiners, byte-order mark), text-direction marks (`a\u{202E}txt.exe`
/// shows as `aexe.txt`), variation selectors, tag characters (which can
/// spell an instruction no one sees) and private-use characters.
fn hiding(c: char) -> bool {
    c.is_control()
        || matches!(c,
            '\u{00AD}' | '\u{034F}' | '\u{061C}' | '\u{115F}' | '\u{1160}' | '\u{17B4}' | '\u{17B5}'
            | '\u{180B}'..='\u{180F}' | '\u{200B}'..='\u{200F}' | '\u{2028}'..='\u{202E}'
            | '\u{2060}'..='\u{206F}' | '\u{3164}' | '\u{FE00}'..='\u{FE0F}' | '\u{FEFF}'
            | '\u{FFA0}' | '\u{FFF0}'..='\u{FFFB}' | '\u{E000}'..='\u{F8FF}'
            | '\u{E0000}'..='\u{E0FFF}' | '\u{F0000}'..='\u{10FFFF}')
}

/// `s` with [`hiding`] characters replaced by U+FFFD, and whether any was.
fn clean(s: &str) -> (String, bool) {
    let mut changed = false;
    let out = s
        .chars()
        .map(|c| {
            if hiding(c) {
                changed = true;
                '\u{FFFD}'
            } else {
                c
            }
        })
        .collect();
    (out, changed)
}

/// A checked `search` call.
struct Args {
    query: String,
    limit: u32,
    folders: Option<bool>,
    /// The folder as typed (`\` separators, no trailing `\`), and its key.
    under: Option<(String, Vec<String>)>,
}

fn args(v: Option<&Value>) -> Result<Args, String> {
    let empty = Map::new();
    let a = match v {
        None => &empty,
        Some(Value::Object(m)) => m,
        Some(_) => return Err("arguments must be an object".into()),
    };
    if let Some(k) = a
        .keys()
        .find(|k| !["query", "limit", "kind", "under"].contains(&k.as_str()))
    {
        return Err(format!(
            "unknown argument `{k}` (allowed: query, limit, kind, under)"
        ));
    }
    let query = match a.get("query") {
        Some(Value::String(q)) if !q.trim().is_empty() => q.trim().to_string(),
        _ => return Err("`query` is required: part of a file or folder name".into()),
    };
    if query.chars().count() > MAX_QUERY_CHARS {
        return Err(format!(
            "`query` is longer than {MAX_QUERY_CHARS} characters"
        ));
    }
    let limit = match a.get("limit") {
        None => DEFAULT_LIMIT,
        Some(l) => match l.as_f64() {
            Some(n) if n.fract() == 0.0 && (1.0..=f64::from(MAX_RESULTS)).contains(&n) => n as u32,
            _ => {
                return Err(format!(
                    "`limit` must be a whole number from 1 to {MAX_RESULTS}"
                ));
            }
        },
    };
    let folders = match a.get("kind").map(|k| k.as_str()) {
        None => None,
        Some(Some("file")) => Some(false),
        Some(Some("folder")) => Some(true),
        Some(_) => return Err("`kind` must be \"file\" or \"folder\"".into()),
    };
    let under = match a.get("under") {
        None => None,
        Some(Value::String(u)) => {
            let u = crate::ask::normalized(std::path::Path::new(u.trim())).map_err(|_| {
                "`under` must be an absolute folder such as C:\\Users\\me, without . or .. parts"
                    .to_string()
            })?;
            let key = crate::ask::folder_keys(&[u.clone().into()]);
            Some((u.trim_end_matches('\\').to_string(), key))
        }
        Some(_) => return Err("`under` must be a folder path".into()),
    };
    Ok(Args {
        query,
        limit,
        folders,
        under,
    })
}

/// The text the indexes are asked: the query, with what the filters let
/// them narrow themselves (Flint's query language: `C:` keeps a drive, a
/// word may match a folder in the path, a lone `/` keeps folders).
fn narrowed(a: &Args) -> String {
    let mut parts = Vec::new();
    if let Some((u, _)) = &a.under {
        let b = u.as_bytes();
        if b.len() >= 2 && b[1] == b':' {
            parts.push(format!("{}:", (b[0] as char).to_ascii_uppercase()));
        }
    }
    parts.push(a.query.clone());
    // a word for the folder only when it can narrow: next to the query's
    // own words (one word alone must match the name) and within the
    // indexes' word limit
    let words = flint_core::parse(&a.query).terms.len();
    let room = (1..crate::server::MAX_QUERY_TERMS).contains(&words);
    if let Some((u, _)) = a.under.as_ref().filter(|_| room) {
        let last = u.rsplit('\\').next().unwrap_or("");
        if !last.is_empty() && !last.ends_with(':') && !last.contains('"') {
            parts.push(format!("\"{last}\""));
        }
    }
    if a.folders == Some(true) {
        parts.push("/".into());
    }
    parts.join(" ")
}

impl<S: Searcher> Server<S> {
    pub fn new(searcher: S) -> Server<S> {
        Server {
            searcher,
            initialized: false,
            calls: VecDeque::new(),
        }
    }

    /// Answers one input line (a message or a batch of them); `None` when
    /// nothing is to be answered (notifications and responses).
    pub fn handle(&mut self, line: &[u8], now: Instant) -> Option<String> {
        let msg: Value = match serde_json::from_slice(line) {
            Ok(m) => m,
            Err(_) => {
                return Some(rpc_error(&Value::Null, -32700, "Parse error", None).to_string());
            }
        };
        match msg {
            Value::Array(items) if !items.is_empty() => {
                let answers: Vec<Value> =
                    items.into_iter().filter_map(|m| self.one(m, now)).collect();
                (!answers.is_empty()).then(|| Value::Array(answers).to_string())
            }
            m => self.one(m, now).map(|a| a.to_string()),
        }
    }

    fn one(&mut self, msg: Value, now: Instant) -> Option<Value> {
        let Value::Object(msg) = msg else {
            return Some(rpc_error(
                &Value::Null,
                -32600,
                "Invalid request: not a JSON-RPC object",
                None,
            ));
        };
        let method = msg.get("method").and_then(Value::as_str);
        let Some(id) = msg.get("id").cloned() else {
            return None; // a notification: nothing to answer
        };
        if !(id.is_string() || id.is_i64() || id.is_u64()) {
            return Some(rpc_error(
                &Value::Null,
                -32600,
                "Invalid request: the id must be a string or an integer",
                None,
            ));
        }
        let Some(method) = method else {
            if msg.contains_key("result") || msg.contains_key("error") {
                return None; // a response to nothing we asked
            }
            return Some(rpc_error(&id, -32600, "Invalid request: no method", None));
        };
        if msg.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
            return Some(rpc_error(
                &id,
                -32600,
                "Invalid request: jsonrpc must be \"2.0\"",
                None,
            ));
        }
        let params = msg.get("params");
        let asked = params
            .and_then(|p| p.get("_meta"))
            .and_then(|m| m.get(VERSION_KEY));
        let era = match (method, asked) {
            ("initialize", _) => {
                let v = params
                    .and_then(|p| p.get("protocolVersion"))
                    .and_then(Value::as_str)
                    .filter(|v| LEGACY.contains(v))
                    .unwrap_or(LEGACY[0]);
                self.initialized = true;
                let r = json!({
                    "protocolVersion": v,
                    "capabilities": {"tools": {}},
                    "serverInfo": server_info(),
                    "instructions": INSTRUCTIONS
                });
                return Some(json!({"jsonrpc": "2.0", "id": id, "result": r}));
            }
            (_, Some(v)) if v.as_str() != Some(MODERN) => {
                return Some(rpc_error(
                    &id,
                    -32022,
                    "Unsupported protocol version",
                    Some(json!({"supported": [MODERN], "requested": v})),
                ));
            }
            (_, Some(_)) => Era::Modern,
            ("server/discover", None) => Era::Modern,
            (_, None) if self.initialized || method == "ping" => Era::Legacy,
            (_, None) => {
                return Some(rpc_error(
                    &id,
                    -32602,
                    "No protocol version: send initialize first, or the version in _meta",
                    None,
                ));
            }
        };
        let result = match method {
            "ping" => Ok(json!({})),
            "server/discover" => Ok(json!({
                "supportedVersions": [MODERN],
                "capabilities": {"tools": {}},
                "_meta": {"io.modelcontextprotocol/serverInfo": server_info()},
                "instructions": INSTRUCTIONS
            })),
            "tools/list" => Ok(json!({"tools": [tool()]})),
            "tools/call" => match params.and_then(|p| p.get("name")).and_then(Value::as_str) {
                Some(TOOL) => Ok(self.call(params.and_then(|p| p.get("arguments")), now)),
                Some(other) => Err((-32602, format!("Unknown tool: {other}"))),
                None => Err((-32602, "tools/call needs a tool name".to_string())),
            },
            other => Err((-32601, format!("Method not found: {other}"))),
        };
        Some(match result {
            Ok(mut r) => {
                if era == Era::Modern {
                    r["resultType"] = json!("complete");
                }
                json!({"jsonrpc": "2.0", "id": id, "result": r})
            }
            Err((code, m)) => rpc_error(&id, code, &m, None),
        })
    }

    fn call(&mut self, a: Option<&Value>, now: Instant) -> Value {
        let a = match args(a) {
            Ok(a) => a,
            Err(e) => return tool_error(&e),
        };
        while self
            .calls
            .front()
            .is_some_and(|t| now.saturating_duration_since(*t) >= RATE_WINDOW)
        {
            self.calls.pop_front();
        }
        if self.calls.len() >= RATE_LIMIT {
            return tool_error(&format!(
                "Too many searches: at most {RATE_LIMIT} a minute. Wait a moment and search again."
            ));
        }
        self.calls.push_back(now);
        // the indexes narrow by drive, folder name and kind themselves; the
        // exact folder and the kind "file" are checked here, on a window
        // wide enough to fill the limit behind them
        let fetch = if a.under.is_some() {
            FILTER_FETCH_MAX
        } else if a.folders == Some(false) {
            a.limit * FILTER_FETCH
        } else {
            a.limit
        };
        let found = match self.searcher.search(&narrowed(&a), fetch) {
            Ok(f) => f,
            Err(e) => return tool_error(&format!("Search failed: {e}")),
        };
        let filtered = a.folders.is_some() || a.under.is_some();
        let passing: Vec<&WireHit> = found
            .hits
            .iter()
            .filter(|h| a.folders.is_none_or(|f| h.is_dir == f))
            .filter(|h| {
                a.under
                    .as_ref()
                    .is_none_or(|(_, u)| crate::ask::is_within(&h.path, u))
            })
            .collect();
        let matches = if filtered {
            passing.len() as u64
        } else {
            found.total
        };
        let results: Vec<Value> = passing
            .iter()
            .take(a.limit as usize)
            .map(|h| {
                let (name, n) = clean(&h.name);
                let (path, p) = clean(&h.path);
                let mut r = json!({
                    "name": name,
                    "path": path,
                    "kind": if h.is_dir { "folder" } else { "file" },
                });
                if let Some(m) = iso_utc(h.last_change) {
                    r["modified"] = json!(m);
                }
                if n || p {
                    r["cleaned"] = json!(true);
                }
                r
            })
            .collect();
        let mut notes = Vec::new();
        if found.failed > 0 {
            notes.push(format!(
                "{} of Flint's indexes did not answer in time, so results may be missing.",
                found.failed
            ));
        }
        if filtered && found.window_full && (results.len() as u32) < a.limit {
            notes.push(format!(
                "Only the best {fetch} matches of the query were checked against kind/under; add more words from the name to find others."
            ));
        }
        let mut sc = json!({
            "shown": results.len(),
            "matches": matches,
            "matchesExact": found.exact && !(filtered && found.window_full),
            "partial": found.failed > 0,
            "results": results
        });
        if !notes.is_empty() {
            sc["note"] = json!(notes.join(" "));
        }
        json!({
            "content": [{"type": "text", "text": sc.to_string()}],
            "structuredContent": sc,
            "isError": false
        })
    }
}

/// Unix seconds as `YYYY-MM-DDTHH:MM:SSZ` (UTC); `None` for 0 (unknown).
pub fn iso_utc(secs: u32) -> Option<String> {
    if secs == 0 {
        return None;
    }
    let (days, rem) = (i64::from(secs) / 86_400, i64::from(secs) % 86_400);
    // civil-from-days (H. Hinnant)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    Some(format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem / 60 % 60,
        rem % 60
    ))
}

/// Reads one message per line from `input` and writes each answer as one
/// line to `output`, until `input` ends.
pub fn serve<S: Searcher>(
    mut input: impl BufRead,
    mut output: impl Write,
    server: &mut Server<S>,
) -> io::Result<()> {
    let mut line = Vec::new();
    loop {
        line.clear();
        let mut overlong = false;
        let mut ended = false;
        loop {
            let buf = input.fill_buf()?;
            if buf.is_empty() {
                ended = true;
                break;
            }
            let (part, done) = match buf.iter().position(|&b| b == b'\n') {
                Some(i) => (&buf[..i], i + 1),
                None => (buf, buf.len()),
            };
            if !overlong && line.len() + part.len() <= MAX_LINE {
                line.extend_from_slice(part);
            } else {
                overlong = true;
                line.clear();
            }
            let newline = done > part.len();
            input.consume(done);
            if newline {
                break;
            }
        }
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        let answer = if overlong {
            Some(rpc_error(&Value::Null, -32700, "Parse error: line too long", None).to_string())
        } else if line.iter().all(u8::is_ascii_whitespace) {
            None
        } else {
            server.handle(&line, Instant::now())
        };
        if let Some(a) = answer {
            output.write_all(a.as_bytes())?;
            output.write_all(b"\n")?;
            output.flush()?;
        }
        if ended {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::WireHit;
    use serde_json::{Value, json};
    use std::cell::RefCell;

    #[derive(Default)]
    struct Fake {
        hits: Vec<WireHit>,
        fail: Option<String>,
        failed: usize,
        exact: bool,
        asked: RefCell<Vec<(String, u32)>>,
    }

    impl Searcher for &Fake {
        fn search(&self, text: &str, limit: u32) -> Result<Found, String> {
            self.asked.borrow_mut().push((text.into(), limit));
            if let Some(e) = &self.fail {
                return Err(e.clone());
            }
            // like Flint's query language: a lone `/` keeps folders only
            let folders = text.ends_with(" /");
            let hits: Vec<WireHit> = self
                .hits
                .iter()
                .filter(|h| !folders || h.is_dir)
                .take(limit as usize)
                .cloned()
                .collect();
            Ok(Found {
                total: self.hits.len() as u64,
                window_full: hits.len() as u32 >= limit,
                hits,
                failed: self.failed,
                exact: self.exact,
            })
        }
    }

    fn hit(path: &str, is_dir: bool) -> WireHit {
        WireHit {
            path: path.into(),
            name: path.rsplit('\\').next().unwrap().into(),
            is_dir,
            typo: false,
            highlights: vec![(0, 1)],
            last_change: 1_793_352_748,
            score: 7,
        }
    }

    fn ask<S: Searcher>(s: &mut Server<S>, msg: Value) -> Value {
        let out = s
            .handle(msg.to_string().as_bytes(), Instant::now())
            .expect("an answer");
        assert!(!out.contains('\n'), "one line: {out}");
        serde_json::from_str(&out).unwrap()
    }

    fn modern(method: &str, params: Value) -> Value {
        let mut p = params;
        p["_meta"] = json!({
            "io.modelcontextprotocol/protocolVersion": MODERN,
            "io.modelcontextprotocol/clientInfo": {"name": "t", "version": "1"},
            "io.modelcontextprotocol/clientCapabilities": {}
        });
        json!({"jsonrpc": "2.0", "id": 9, "method": method, "params": p})
    }

    fn legacy_server(f: &Fake) -> Server<&Fake> {
        let mut s = Server::new(f);
        let r = ask(
            &mut s,
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}),
        );
        assert_eq!(r["result"]["protocolVersion"], "2025-06-18");
        assert!(
            s.handle(
                br#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
                Instant::now()
            )
            .is_none()
        );
        s
    }

    fn call(s: &mut Server<&Fake>, args: Value) -> Value {
        ask(
            s,
            json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":TOOL,"arguments":args}}),
        )["result"]
            .clone()
    }

    #[test]
    fn initialize_answers_the_clients_version_or_the_newest_it_knows() {
        let f = Fake::default();
        let mut s = Server::new(&f);
        let r = ask(
            &mut s,
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2099-01-01","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}),
        );
        assert_eq!(r["id"], 1);
        assert_eq!(r["result"]["protocolVersion"], LEGACY[0]);
        assert_eq!(r["result"]["serverInfo"]["name"], "flint");
        assert!(r["result"]["capabilities"]["tools"].is_object());
        assert!(r["result"]["capabilities"].get("resources").is_none());
        legacy_server(&f);
    }

    #[test]
    fn discover_names_the_modern_version() {
        let f = Fake::default();
        let r = ask(&mut Server::new(&f), modern("server/discover", json!({})));
        assert_eq!(r["result"]["resultType"], "complete");
        assert_eq!(r["result"]["supportedVersions"], json!([MODERN]));
        assert!(r["result"]["capabilities"]["tools"].is_object());
        assert_eq!(
            r["result"]["_meta"]["io.modelcontextprotocol/serverInfo"]["name"],
            "flint"
        );
    }

    #[test]
    fn the_only_tool_is_a_read_only_search() {
        let f = Fake::default();
        for r in [
            ask(
                &mut legacy_server(&f),
                json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
            ),
            ask(&mut Server::new(&f), modern("tools/list", json!({}))),
        ] {
            let tools = r["result"]["tools"].as_array().unwrap();
            assert_eq!(tools.len(), 1);
            let t = &tools[0];
            assert_eq!(t["name"], TOOL);
            assert_eq!(t["annotations"]["readOnlyHint"], true);
            assert_eq!(t["annotations"]["destructiveHint"], false);
            assert_eq!(t["annotations"]["openWorldHint"], false);
            assert_eq!(t["inputSchema"]["additionalProperties"], false);
            assert_eq!(t["inputSchema"]["required"], json!(["query"]));
            assert!(t["outputSchema"].is_object());
        }
    }

    #[test]
    fn modern_answers_carry_a_result_type_and_legacy_ones_do_not() {
        let f = Fake::default();
        let r = ask(&mut Server::new(&f), modern("tools/list", json!({})));
        assert_eq!(r["result"]["resultType"], "complete");
        let r = ask(
            &mut legacy_server(&f),
            json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
        );
        assert!(r["result"].get("resultType").is_none());
    }

    #[test]
    fn a_request_with_neither_a_handshake_nor_a_version_is_refused() {
        let f = Fake::default();
        let mut s = Server::new(&f);
        let r = ask(
            &mut s,
            json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":TOOL,"arguments":{"query":"a"}}}),
        );
        assert_eq!(r["error"]["code"], -32602);
        assert!(f.asked.borrow().is_empty(), "nothing searched");
        // ping needs neither
        let r = ask(&mut s, json!({"jsonrpc":"2.0","id":4,"method":"ping"}));
        assert_eq!(r["result"], json!({}));
    }

    #[test]
    fn an_unsupported_modern_version_is_named_with_the_supported_one() {
        let f = Fake::default();
        let mut m = modern("tools/list", json!({}));
        m["params"]["_meta"]["io.modelcontextprotocol/protocolVersion"] = json!("1900-01-01");
        let r = ask(&mut Server::new(&f), m);
        assert_eq!(r["error"]["code"], -32022);
        assert_eq!(r["error"]["data"]["supported"], json!([MODERN]));
        assert_eq!(r["error"]["data"]["requested"], "1900-01-01");
    }

    #[test]
    fn a_search_returns_names_paths_kinds_and_dates_only() {
        let f = Fake {
            hits: vec![
                hit(r"C:\Users\X\rapor.txt", false),
                hit(r"C:\Users\X\Raporlar", true),
            ],
            ..Fake::default()
        };
        let r = call(&mut legacy_server(&f), json!({"query": "  rapr  "}));
        assert_eq!(r["isError"], false);
        let sc = &r["structuredContent"];
        assert_eq!(
            sc["results"],
            json!([
                {"name":"rapor.txt","path":r"C:\Users\X\rapor.txt","kind":"file","modified":"2026-10-30T09:32:28Z"},
                {"name":"Raporlar","path":r"C:\Users\X\Raporlar","kind":"folder","modified":"2026-10-30T09:32:28Z"}
            ])
        );
        assert_eq!(sc["shown"], 2);
        assert_eq!(sc["matches"], 2);
        // the same, as text, for clients that read only text
        let text: Value = serde_json::from_str(r["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(&text, sc);
        assert_eq!(f.asked.borrow()[0], ("rapr".to_string(), DEFAULT_LIMIT));
    }

    #[test]
    fn the_limit_defaults_to_20_and_must_be_1_to_50() {
        let f = Fake::default();
        let mut s = legacy_server(&f);
        for bad in [json!(500), json!(0), json!(-1), json!(2.5), json!("5")] {
            assert_eq!(
                call(&mut s, json!({"query":"a","limit":bad}))["isError"],
                true,
                "{bad}"
            );
        }
        call(&mut s, json!({"query":"a"}));
        call(&mut s, json!({"query":"a","limit":3}));
        call(&mut s, json!({"query":"a","limit":5.0}));
        call(&mut s, json!({"query":"a","limit":50}));
        let asked: Vec<u32> = f.asked.borrow().iter().map(|a| a.1).collect();
        assert_eq!(asked, [DEFAULT_LIMIT, 3, 5, MAX_RESULTS]);
    }

    #[test]
    fn kind_and_folder_filters_go_into_the_query_and_keep_the_limit() {
        let mut hits = Vec::new();
        for i in 0..10 {
            hits.push(hit(&format!(r"C:\A\f{i}.txt"), false));
            hits.push(hit(&format!(r"C:\B\d{i}"), true));
        }
        let f = Fake {
            hits,
            ..Fake::default()
        };
        let mut s = legacy_server(&f);
        let r = call(&mut s, json!({"query":"x","limit":3,"kind":"folder"}));
        let res = r["structuredContent"]["results"].as_array().unwrap();
        assert_eq!(res.len(), 3);
        assert!(res.iter().all(|h| h["kind"] == "folder"));
        let r = call(&mut s, json!({"query":"x","limit":5,"under":r"c:\a\"}));
        let res = r["structuredContent"]["results"].as_array().unwrap();
        assert_eq!(res.len(), 5);
        assert!(
            res.iter()
                .all(|h| h["path"].as_str().unwrap().starts_with(r"C:\A\"))
        );
        assert_eq!(
            r["structuredContent"]["matches"], 10,
            "matches counts what passed the filter"
        );
        let r = call(&mut s, json!({"query":"x","kind":"file","under":r"C:\B"}));
        assert_eq!(r["structuredContent"]["shown"], 0);
        let r = call(
            &mut s,
            json!({"query":"x","kind":"file","under":r"\\srv\share\Proje Dosyalari"}),
        );
        assert_eq!(r["structuredContent"]["shown"], 0);
        let asked = f.asked.borrow().clone();
        assert_eq!(
            asked,
            [
                ("x /".to_string(), 3),
                (r#"C: x "a""#.to_string(), FILTER_FETCH_MAX),
                (r#"C: x "B""#.to_string(), FILTER_FETCH_MAX),
                (r#"x "Proje Dosyalari""#.to_string(), FILTER_FETCH_MAX),
            ]
        );
    }

    #[test]
    fn narrowing_adds_no_word_to_a_full_or_a_filter_only_query() {
        let f = Fake::default();
        let mut s = legacy_server(&f);
        let sixteen = (0..16)
            .map(|i| format!("w{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        call(&mut s, json!({"query": sixteen, "under": r"C:\A"}));
        call(&mut s, json!({"query": "ext:pdf", "under": r"C:\Proje"}));
        call(&mut s, json!({"query": "rapor", "under": r"\\?\C:\Proje"}));
        let asked: Vec<String> = f.asked.borrow().iter().map(|a| a.0.clone()).collect();
        assert_eq!(
            asked,
            [
                format!("C: {sixteen}"),
                "C: ext:pdf".to_string(),
                r#"C: rapor "Proje""#.to_string()
            ]
        );
    }

    #[test]
    fn under_with_dot_segments_is_refused() {
        let f = Fake::default();
        let mut s = legacy_server(&f);
        for bad in [r"C:\a\..\x", r"C:\a\.\x", r"\\?\Volume{1}\x", "Proje"] {
            assert_eq!(
                call(&mut s, json!({"query":"a","under":bad}))["isError"],
                true,
                "{bad}"
            );
        }
    }

    #[test]
    fn the_count_is_marked_when_it_is_only_a_lower_bound() {
        let f = Fake {
            exact: false,
            ..Fake::default()
        };
        let r = call(&mut legacy_server(&f), json!({"query":"x"}));
        assert_eq!(r["structuredContent"]["matchesExact"], false);
        let f = Fake {
            exact: true,
            ..Fake::default()
        };
        let r = call(&mut legacy_server(&f), json!({"query":"x"}));
        assert_eq!(r["structuredContent"]["matchesExact"], true);
    }

    #[test]
    fn a_full_window_after_filtering_and_a_missing_index_are_said() {
        let hits: Vec<WireHit> = (0..FILTER_FETCH_MAX)
            .map(|i| hit(&format!(r"C:\Z\f{i}.txt"), false))
            .collect();
        let f = Fake {
            hits,
            failed: 1,
            ..Fake::default()
        };
        let mut s = legacy_server(&f);
        let r = call(&mut s, json!({"query":"x","under":r"C:\A"}));
        let sc = &r["structuredContent"];
        assert_eq!(sc["shown"], 0);
        assert_eq!(sc["partial"], true);
        let note = sc["note"].as_str().unwrap();
        assert!(
            note.contains("did not answer") && note.contains("more words"),
            "{note}"
        );
        let f = Fake::default();
        let r = call(&mut legacy_server(&f), json!({"query":"x"}));
        assert_eq!(r["structuredContent"]["partial"], false);
        assert!(r["structuredContent"].get("note").is_none());
    }

    #[test]
    fn bad_arguments_are_tool_errors_and_nothing_is_searched() {
        let f = Fake::default();
        let mut s = legacy_server(&f);
        for args in [
            json!({}),
            json!({"query":"   "}),
            json!({"query":"a".repeat(257)}),
            json!({"query":"a","limit":"5"}),
            json!({"query":"a","limit":2.5}),
            json!({"query":"a","kind":"pdf"}),
            json!({"query":"a","under":r"relative\dir"}),
            json!({"query":"a","contents":true}),
            json!("a"),
        ] {
            let r = call(&mut s, args.clone());
            assert_eq!(r["isError"], true, "{args}");
            assert!(!r["content"][0]["text"].as_str().unwrap().is_empty());
        }
        assert!(f.asked.borrow().is_empty());
    }

    #[test]
    fn protocol_errors_use_json_rpc_codes() {
        let f = Fake::default();
        let mut s = legacy_server(&f);
        let r = ask(
            &mut s,
            json!({"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"delete","arguments":{}}}),
        );
        assert_eq!(r["error"]["code"], -32602);
        let r = ask(
            &mut s,
            json!({"jsonrpc":"2.0","id":7,"method":"resources/list"}),
        );
        assert_eq!(r["error"]["code"], -32601);
        let r: Value =
            serde_json::from_str(&s.handle(b"{not json", Instant::now()).unwrap()).unwrap();
        assert_eq!(r["error"]["code"], -32700);
        assert_eq!(r["id"], Value::Null);
        for bad in [
            json!({"jsonrpc":"2.0","id":8}),
            json!({"jsonrpc":"2.0","id":null,"method":"ping"}),
            json!({"id":8,"method":"ping"}),
            json!({"jsonrpc":"1.0","id":8,"method":"ping"}),
            json!([]),
            json!(7),
        ] {
            assert_eq!(ask(&mut s, bad.clone())["error"]["code"], -32600, "{bad}");
        }
        let mut m = modern("tools/list", json!({}));
        m["params"]["_meta"]["io.modelcontextprotocol/protocolVersion"] = json!(20260728);
        let r = ask(&mut s, m);
        assert_eq!(r["error"]["code"], -32022);
        assert_eq!(r["error"]["data"]["requested"], 20260728);
    }

    #[test]
    fn a_batch_gets_one_array_of_its_answers() {
        let f = Fake::default();
        let mut s = legacy_server(&f);
        let r = ask(
            &mut s,
            json!([
                {"jsonrpc":"2.0","id":1,"method":"ping"},
                {"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":0}},
                {"jsonrpc":"2.0","id":2,"method":"tools/list"}
            ]),
        );
        let a = r.as_array().unwrap();
        assert_eq!(a.len(), 2);
        assert_eq!(a[0]["id"], 1);
        assert_eq!(a[1]["result"]["tools"][0]["name"], TOOL);
        let only_notes =
            json!([{"jsonrpc":"2.0","method":"notifications/initialized"}]).to_string();
        assert!(s.handle(only_notes.as_bytes(), Instant::now()).is_none());
    }

    #[test]
    fn notifications_and_responses_get_no_answer() {
        let f = Fake::default();
        let mut s = legacy_server(&f);
        for m in [
            json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":5}}),
            json!({"jsonrpc":"2.0","method":"tools/call","params":{"name":TOOL,"arguments":{"query":"a"}}}),
            json!({"jsonrpc":"2.0","id":3,"result":{}}),
        ] {
            assert!(
                s.handle(m.to_string().as_bytes(), Instant::now()).is_none(),
                "{m}"
            );
        }
        assert!(f.asked.borrow().is_empty());
    }

    #[test]
    fn a_failed_search_is_a_tool_error_with_its_reason() {
        let f = Fake {
            fail: Some("Flint is not running".into()),
            ..Fake::default()
        };
        let r = call(&mut legacy_server(&f), json!({"query":"a"}));
        assert_eq!(r["isError"], true);
        assert!(
            r["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("Flint is not running")
        );
    }

    #[test]
    fn searches_beyond_the_rate_limit_are_refused_until_the_window_passes() {
        let f = Fake::default();
        let mut s = legacy_server(&f);
        let t0 = Instant::now();
        let msg = json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":TOOL,"arguments":{"query":"a"}}}).to_string();
        let at = |s: &mut Server<&Fake>, t: Instant| -> Value {
            serde_json::from_str::<Value>(&s.handle(msg.as_bytes(), t).unwrap()).unwrap()["result"]
                ["isError"]
                .clone()
        };
        for i in 0..RATE_LIMIT {
            assert_eq!(at(&mut s, t0 + Duration::from_millis(i as u64)), false);
        }
        assert_eq!(at(&mut s, t0 + Duration::from_secs(1)), true);
        assert_eq!(f.asked.borrow().len(), RATE_LIMIT);
        assert_eq!(
            at(&mut s, t0 + RATE_WINDOW + Duration::from_millis(1)),
            false
        );
    }

    #[test]
    fn invisible_control_and_direction_characters_in_names_are_replaced() {
        let f = Fake {
            hits: vec![
                hit("C:\\x\\a\u{202E}txt.exe", false),
                hit("C:\\x\\b\u{7}.txt", false),
                hit(
                    "C:\\x\\c\u{E0049}\u{E0047}\u{200B}\u{FEFF}\u{AD}\u{2060}\u{FE0F}\u{E000}\u{F0001}.txt",
                    false,
                ),
                hit("C:\\x\\Bütçe ğüşıöç 日本.txt", false),
            ],
            ..Fake::default()
        };
        let r = call(&mut legacy_server(&f), json!({"query":"a"}));
        let res = &r["structuredContent"]["results"];
        assert_eq!(res[0]["name"], "a\u{FFFD}txt.exe");
        assert_eq!(res[0]["path"], "C:\\x\\a\u{FFFD}txt.exe");
        assert_eq!(res[0]["cleaned"], true);
        assert_eq!(res[1]["name"], "b\u{FFFD}.txt");
        assert_eq!(res[2]["name"], format!("c{}.txt", "\u{FFFD}".repeat(9)));
        assert_eq!(res[3]["name"], "Bütçe ğüşıöç 日本.txt");
        assert!(res[3].get("cleaned").is_none());
    }

    #[test]
    fn dates_are_utc_and_zero_is_unknown() {
        assert_eq!(iso_utc(0), None);
        assert_eq!(
            iso_utc(951_782_400).as_deref(),
            Some("2000-02-29T00:00:00Z")
        );
        assert_eq!(
            iso_utc(1_793_352_748).as_deref(),
            Some("2026-10-30T09:32:28Z")
        );
        assert_eq!(
            iso_utc(4_102_444_799).as_deref(),
            Some("2099-12-31T23:59:59Z")
        );
    }

    #[test]
    fn serve_answers_line_by_line_skips_an_overlong_line_and_ends_with_input() {
        let f = Fake::default();
        let mut s = Server::new(&f);
        let mut input = Vec::new();
        input.extend_from_slice(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\r\n");
        input.extend(std::iter::repeat_n(b'x', MAX_LINE + 10));
        input.extend_from_slice(b"\n\n{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"ping\"}");
        let mut out = Vec::new();
        serve(&input[..], &mut out, &mut s).unwrap();
        let lines: Vec<Value> = String::from_utf8(out)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert_eq!(lines[0]["id"], 1);
        assert_eq!(lines[1]["error"]["code"], -32700);
        assert_eq!(lines[2]["id"], 2);
    }
}
