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
const BIN_HINT: &str = "niri-resume";

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
    pub is_floating: bool,
    /// Position/size info; present since niri 25.08.
    pub layout: Option<WindowLayout>,
}

/// Layout info of a window (all sizes in logical pixels).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct WindowLayout {
    /// 1-based (column index, tile index in column) for tiled windows.
    pub pos_in_scrolling_layout: Option<(usize, usize)>,
    /// Size of the tile including decorations like borders.
    pub tile_size: Option<(f64, f64)>,
    /// For floating windows: the stored position, absolute in the output.
    pub tile_pos_in_workspace_view: Option<(f64, f64)>,
}

/// Workspace as reported by niri's IPC.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Workspace {
    pub id: u64,
    /// 1-based index on its monitor.
    pub idx: u8,
    pub name: Option<String>,
    pub output: Option<String>,
    pub is_focused: bool,
    pub is_active: bool,
}

/// Fetch the list of currently open toplevel windows.
pub fn windows() -> Result<Vec<Window>, String> {
    // Testing hook: read windows from a file (raw array or full IPC reply)
    // instead of talking to a running compositor.
    if let Ok(path) = std::env::var("NIRI_RESUME_WINDOWS_JSON") {
        let text = std::fs::read_to_string(&path)
            .map_err(|e| format!("cannot read NIRI_RESUME_WINDOWS_JSON={path}: {e}"))?;
        return parse_items(&text, "Windows")
            .map_err(|e| format!("invalid NIRI_RESUME_WINDOWS_JSON data: {e}"));
    }

    match fetch_array("\"Windows\"", "Windows", Some("windows")) {
        Ok(items) => decode_items(items),
        Err(err) => {
            eprintln!("{BIN_HINT}: direct IPC failed ({err}); trying `niri msg --json windows`");
            let out = run_msg_json("windows")?;
            parse_items(&out, "Windows")
        }
    }
}

/// Fetch the list of workspaces.
pub fn workspaces() -> Result<Vec<Workspace>, String> {
    match fetch_array("\"Workspaces\"", "Workspaces", Some("workspaces")) {
        Ok(items) => decode_items(items),
        Err(err) => {
            eprintln!("{BIN_HINT}: direct IPC failed ({err}); trying `niri msg --json workspaces`");
            let out = run_msg_json("workspaces")?;
            parse_items(&out, "Workspaces")
        }
    }
}

/// Perform one niri action (the inner `Action` JSON value, e.g.
/// `json!({"FocusWindow": {"id": 3}})`).
pub fn send_action(action: Value) -> Result<(), String> {
    let request = serde_json::json!({ "Action": action }).to_string();
    let reply = send(&request)?;
    parse_reply(&reply).map(|_| ())
}

fn run_msg_json(subcommand: &str) -> Result<String, String> {
    let out = Command::new("niri")
        .args(["msg", "--json", subcommand])
        .output()
        .map_err(|e| format!("cannot run `niri msg`: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "`niri msg --json {subcommand}` failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    String::from_utf8(out.stdout).map_err(|e| format!("bad utf-8: {e}"))
}

/// Send one JSON request line over the socket, return the reply line.
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

fn socket_path() -> Result<String, String> {
    std::env::var(SOCKET_ENV)
        .ok()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("{SOCKET_ENV} is not set; are we running inside a niri session?"))
}

/// Send a request directly over the socket and dig the item array out of the
/// reply. `msg_subcommand` is the `niri msg --json` fallback, if any.
fn fetch_array(
    request: &str,
    key: &str,
    msg_subcommand: Option<&str>,
) -> Result<Vec<Value>, String> {
    let reply = send(request)?;
    let value: Value = serde_json::from_str(&reply).map_err(|e| format!("bad JSON: {e}"))?;
    let arr = reply_array(&value, key)?;
    let _ = msg_subcommand; // only used when the socket path fails entirely
    Ok(arr.clone())
}

/// Dig the array out of `{"Ok":{"<Key>":[...]}}`, `{"<Key>":[...]}` or a bare `[...]`.
fn reply_array<'a>(value: &'a Value, key: &str) -> Result<&'a Vec<Value>, String> {
    match value {
        Value::Array(arr) => Ok(arr),
        Value::Object(map) => {
            if let Some(ok) = map.get("Ok") {
                return reply_array(ok, key);
            }
            if let Some(arr) = map.get(key).and_then(Value::as_array) {
                return Ok(arr);
            }
            Err(format!("reply does not contain a `{key}` list"))
        }
        _ => Err("unexpected reply shape".to_string()),
    }
}

fn parse_items<T: for<'de> Deserialize<'de>>(text: &str, key: &str) -> Result<Vec<T>, String> {
    let value: Value = serde_json::from_str(text).map_err(|e| format!("bad JSON: {e}"))?;
    reply_array(&value, key)?
        .iter()
        .map(|item| serde_json::from_value(item.clone()).map_err(|e| format!("bad entry: {e}")))
        .collect()
}

fn decode_items<T: for<'de> Deserialize<'de>>(items: Vec<Value>) -> Result<Vec<T>, String> {
    items
        .into_iter()
        .map(|item| serde_json::from_value(item).map_err(|e| format!("bad entry: {e}")))
        .collect()
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

/// Ask niri to quit immediately (skipping its confirmation dialog).
pub fn quit() -> Result<(), String> {
    send_action(serde_json::json!({
        "Quit": { "skip_confirmation": true }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bare_array() {
        let text = r#"[{"id":1,"title":"w","app_id":"foot","pid":42,"workspace_id":2,"is_focused":false}]"#;
        let windows: Vec<Window> = parse_items(text, "Windows").unwrap();
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].app_id.as_deref(), Some("foot"));
        assert_eq!(windows[0].pid, Some(42));
    }

    #[test]
    fn parses_full_reply_with_layout() {
        let text = r#"{"Ok":{"Windows":[{"id":12,"title":"t","app_id":"Alacritty","workspace_id":6,
            "is_focused":true,"is_floating":false,"is_urgent":false,
            "layout":{"pos_in_scrolling_layout":[2,1],"tile_size":[500.0,300.0],
                      "window_size":[500,300],"tile_pos_in_workspace_view":null,
                      "window_offset_in_tile":[0.0,0.0]}}]}}"#;
        let windows: Vec<Window> = parse_items(text, "Windows").unwrap();
        let layout = windows[0].layout.as_ref().unwrap();
        assert_eq!(layout.pos_in_scrolling_layout, Some((2, 1)));
        assert_eq!(layout.tile_size, Some((500.0, 300.0)));
        assert_eq!(layout.tile_pos_in_workspace_view, None);
    }

    #[test]
    fn parses_workspaces() {
        let text = r#"{"Ok":{"Workspaces":[{"id":2,"idx":2,"name":null,"output":"HDMI-A-1",
            "is_urgent":false,"is_active":false,"is_focused":false,"active_window_id":3}]}}"#;
        let workspaces: Vec<Workspace> = parse_items(text, "Workspaces").unwrap();
        assert_eq!(workspaces[0].idx, 2);
        assert_eq!(workspaces[0].name, None);
        assert_eq!(workspaces[0].output.as_deref(), Some("HDMI-A-1"));
    }

    #[test]
    fn tolerates_unknown_fields() {
        let text = r#"{"Ok":{"Windows":[{"id":1,"some_future_field":{"x":1}},{"id":2}]}}"#;
        let windows: Vec<Window> = parse_items(text, "Windows").unwrap();
        assert_eq!(windows.len(), 2);
        assert_eq!(windows[1].app_id, None);
    }

    #[test]
    fn rejects_bad_shapes() {
        assert!(parse_items::<Window>(r#"{"Ok":{"Workspaces":[]}}"#, "Windows").is_err());
        assert!(parse_items::<Window>("not json", "Windows").is_err());
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
