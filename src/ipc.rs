//! Minimal client for niri's IPC socket (`$NIRI_SOCKET`), with a
//! `niri msg --json` fallback in case the direct handshake ever fails.
//!
//! Wire protocol (from the `niri-ipc` crate docs): requests are JSON-encoded
//! on a single line, and each reply is a JSON-encoded `Result<Response, String>`
//! on a single line, i.e. `{"Ok":...}` or `{"Err":"..."}`. Unknown fields and
//! variants may appear across niri versions, so parsing stays tolerant.

use serde::Deserialize;
use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::process::Command;

const SOCKET_ENV: &str = "NIRI_SOCKET";

/// Toplevel window as reported by niri's IPC.
///
/// Fields tolerate missing data so the tool keeps working across niri
/// releases; extra fields are ignored by serde.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Window {
    pub id: u64,
    pub title: Option<String>,
    pub app_id: Option<String>,
    /// PID that created the Wayland connection for this window, when known.
    pub pid: Option<i32>,
    pub workspace_id: Option<u64>,
    pub is_focused: bool,
}

/// Fetch the list of currently open toplevel windows.
pub fn windows() -> Result<Vec<Window>, String> {
    // Testing hook: read windows from a file (raw array or full IPC reply)
    // instead of talking to a running compositor.
    if let Ok(path) = std::env::var("NIRI_RESUME_WINDOWS_JSON") {
        let text = std::fs::read_to_string(&path)
            .map_err(|e| format!("cannot read NIRI_RESUME_WINDOWS_JSON={path}: {e}"))?;
        return parse_windows(&text)
            .map_err(|e| format!("invalid NIRI_RESUME_WINDOWS_JSON data: {e}"));
    }

    match windows_via_socket() {
        Ok(windows) => Ok(windows),
        Err(err) => {
            eprintln!("{BIN_HINT}: direct IPC failed ({err}); trying `niri msg --json windows`");
            windows_via_msg()
        }
    }
}

/// Ask niri to quit immediately (skipping its confirmation dialog).
pub fn quit() -> Result<(), String> {
    let request = r#"{"Action":{"Quit":{"skip_confirmation":true}}}"#;
    match send(request) {
        Ok(reply) => parse_reply(&reply).map(|_| ()),
        Err(err) => {
            eprintln!("{BIN_HINT}: direct IPC failed ({err}); trying `niri msg action quit`");
            let status = Command::new("niri")
                .args(["msg", "action", "quit", "--skip-confirmation"])
                .status()
                .map_err(|e| format!("cannot run `niri msg action quit`: {e}"))?;
            if status.success() {
                Ok(())
            } else {
                Err(format!("`niri msg action quit` exited with {status}"))
            }
        }
    }
}

const BIN_HINT: &str = "niri-resume";

fn socket_path() -> Result<String, String> {
    std::env::var(SOCKET_ENV)
        .ok()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("{SOCKET_ENV} is not set; are we running inside a niri session?"))
}

/// Send one JSON request line, return the reply line.
fn send(request: &str) -> Result<String, String> {
    let path = socket_path()?;
    let stream =
        UnixStream::connect(&path).map_err(|e| format!("cannot connect to {path}: {e}"))?;
    let mut writer = stream.try_clone().map_err(|e| format!("socket: {e}"))?;
    writer
        .write_all(format!("{request}\n").as_bytes())
        .and_then(|_| writer.flush())
        .map_err(|e| format!("cannot write to niri socket: {e}"))?;
    let mut line = String::new();
    BufReader::new(stream)
        .read_line(&mut line)
        .map_err(|e| format!("cannot read from niri socket: {e}"))?;
    if line.trim().is_empty() {
        return Err("empty reply from niri socket".to_string());
    }
    Ok(line)
}

fn windows_via_socket() -> Result<Vec<Window>, String> {
    let reply = send("\"Windows\"")?;
    parse_windows(&reply)
}

fn windows_via_msg() -> Result<Vec<Window>, String> {
    let out = Command::new("niri")
        .args(["msg", "--json", "windows"])
        .output()
        .map_err(|e| format!("cannot run `niri msg`: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "`niri msg --json windows` failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let stdout = std::str::from_utf8(&out.stdout).map_err(|e| format!("bad utf-8: {e}"))?;
    parse_windows(stdout)
}

/// Parse a reply (or a bare array) into a window list.
fn parse_windows(text: &str) -> Result<Vec<Window>, String> {
    let value: Value = serde_json::from_str(text).map_err(|e| format!("bad JSON: {e}"))?;
    let arr = windows_array(&value)?;
    arr.iter()
        .map(|w| serde_json::from_value(w.clone()).map_err(|e| format!("bad window entry: {e}")))
        .collect()
}

/// Dig the windows array out of `{"Ok":{"Windows":[...]}}`, `{"Windows":[...]}`
/// or a bare `[...]`.
fn windows_array(value: &Value) -> Result<&Vec<Value>, String> {
    match value {
        Value::Array(arr) => Ok(arr),
        Value::Object(map) => {
            if let Some(ok) = map.get("Ok") {
                return windows_array(ok);
            }
            if let Some(arr) = map.get("Windows").and_then(Value::as_array) {
                return Ok(arr);
            }
            Err("reply does not contain a windows list".to_string())
        }
        _ => Err("unexpected reply shape".to_string()),
    }
}

/// Unwrap `{"Ok":X}` / `{"Err":msg}`.
fn parse_reply(text: &str) -> Result<Value, String> {
    let value: Value = serde_json::from_str(text).map_err(|e| format!("bad JSON: {e}"))?;
    match &value {
        Value::Object(map) if map.contains_key("Ok") => {
            Ok(map.get("Ok").cloned().unwrap_or(Value::Null))
        }
        Value::Object(map) if map.contains_key("Err") => {
            let msg = map
                .get("Err")
                .and_then(Value::as_str)
                .unwrap_or("unknown error");
            Err(msg.to_string())
        }
        _ => Err("unexpected reply shape".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bare_array() {
        let text = r#"[{"id":1,"title":"w","app_id":"foot","pid":42,"workspace_id":2,"is_focused":false}]"#;
        let windows = parse_windows(text).unwrap();
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].app_id.as_deref(), Some("foot"));
        assert_eq!(windows[0].pid, Some(42));
    }

    #[test]
    fn parses_full_reply() {
        let text = r#"{"Ok":{"Windows":[{"id":12,"title":"t","app_id":"Alacritty","workspace_id":6,"is_focused":true}]}}"#;
        let windows = parse_windows(text).unwrap();
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].app_id.as_deref(), Some("Alacritty"));
        assert_eq!(windows[0].pid, None); // older niri replies have no pid
    }

    #[test]
    fn tolerates_unknown_fields() {
        let text = r#"{"Ok":{"Windows":[{"id":1,"some_future_field":{"x":1}},{"id":2}]}}"#;
        let windows = parse_windows(text).unwrap();
        assert_eq!(windows.len(), 2);
        assert_eq!(windows[1].app_id, None);
    }

    #[test]
    fn rejects_bad_shapes() {
        assert!(parse_windows(r#"{"Ok":{"Workspaces":[]}}"#).is_err());
        assert!(parse_windows("not json").is_err());
    }

    #[test]
    fn unwraps_replies() {
        assert!(parse_reply(r#"{"Ok":"Handled"}"#).is_ok());
        assert_eq!(
            parse_reply(r#"{"Err":"nope"}"#).unwrap_err(),
            "nope".to_string()
        );
    }
}
