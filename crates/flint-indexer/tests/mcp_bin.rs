//! The real `flint-indexer mcp` program, started as an assistant starts
//! it: stdout carries protocol lines only, a damaged settings file shows
//! nothing, and closing stdin ends it.
#![cfg(windows)]

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[test]
fn the_program_speaks_only_protocol_on_stdout_fails_closed_and_ends_with_stdin() {
    let appdata = std::env::temp_dir().join(format!("flint-mcp-bin-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&appdata);
    std::fs::create_dir_all(appdata.join("Flint")).unwrap();
    std::fs::write(
        appdata.join("Flint").join("settings.json"),
        b"{\"excludedFolders\":[",
    )
    .unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_flint-indexer"))
        .arg("mcp")
        .env("APPDATA", &appdata)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let msgs = [
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}"#,
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"search","arguments":{"query":"rapor"}}}"#,
    ];
    for m in msgs {
        writeln!(stdin, "{m}").unwrap();
    }
    drop(stdin); // the assistant goes away
    let lines: Vec<serde_json::Value> = BufReader::new(child.stdout.take().unwrap())
        .lines()
        .map(|l| serde_json::from_str(&l.unwrap()).expect("every stdout line is JSON-RPC"))
        .collect();
    let t = Instant::now();
    while child.try_wait().unwrap().is_none() {
        assert!(
            t.elapsed() < Duration::from_secs(10),
            "ends when stdin closes"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(lines.len(), 3, "{lines:?}");
    assert_eq!(lines[0]["result"]["serverInfo"]["name"], "flint");
    assert_eq!(lines[1]["result"]["tools"][0]["name"], "search");
    let r = &lines[2]["result"];
    assert_eq!(r["isError"], true, "{r}");
    assert!(
        r["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("nothing is shown"),
        "{r}"
    );
    assert!(r.get("structuredContent").is_none());
    let _ = std::fs::remove_dir_all(&appdata);
}

#[test]
fn a_bad_settings_entry_is_not_repeated_to_the_assistant() {
    let appdata = std::env::temp_dir().join(format!("flint-mcp-leak-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&appdata);
    std::fs::create_dir_all(appdata.join("Flint")).unwrap();
    let bodies = [
        r#"{"aiHiddenFolders":["Bosanma-Belgeleri"]}"#,
        r#"{"aiHiddenFolders":"C:\\Bosanma-Belgeleri"}"#,
    ];
    for (i, body) in bodies.iter().enumerate() {
        std::fs::write(appdata.join("Flint").join("settings.json"), body).unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_flint-indexer"))
            .arg("mcp")
            .env("APPDATA", &appdata)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        writeln!(stdin, r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":"2025-11-25","capabilities":{{}},"clientInfo":{{"name":"t","version":"1"}}}}}}"#).unwrap();
        writeln!(stdin, r#"{{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{{"name":"search","arguments":{{"query":"rapor"}}}}}}"#).unwrap();
        drop(stdin);
        let mut out = String::new();
        std::io::Read::read_to_string(&mut child.stdout.take().unwrap(), &mut out).unwrap();
        child.wait().unwrap();
        assert!(out.contains("nothing is shown"), "{i}");
        assert!(!out.contains("Bosanma"), "case {i} repeats the entry");
    }
    let _ = std::fs::remove_dir_all(&appdata);
}
