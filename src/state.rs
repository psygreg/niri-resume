//! The saved session state file (`$XDG_STATE_HOME/niri-resume/session.json`).
//!
//! The file is a full snapshot of the detected apps at the last end-of-session
//! event; each save overwrites it atomically, so stale entries never
//! accumulate. Version 2 adds per-window records (workspace, size, state,
//! position, column placement) so restore can also rebuild the layout.

use crate::util;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

pub const STATE_VERSION: u32 = 2;

/// How a workspace is referenced for restore. Named workspaces are preferred;
/// numbered ones fall back to (output, 1-based index).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct WorkspaceRef {
    #[serde(default)]
    pub output: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub idx: Option<u8>,
}

/// Recorded display state of a window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum WindowState {
    #[default]
    #[serde(rename = "normal")]
    Normal,
    #[serde(rename = "maximized")]
    Maximized,
    #[serde(rename = "fullscreen")]
    Fullscreen,
}

/// Everything known about one window at save time, for layout restoration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct WindowRecord {
    pub title: Option<String>,
    pub workspace: WorkspaceRef,
    pub floating: bool,
    pub state: WindowState,
    /// Absolute floating position (`tile_pos_in_workspace_view`), floating windows only.
    pub pos: Option<(f64, f64)>,
    /// Tile size in logical pixels.
    pub size: Option<(f64, f64)>,
    /// 1-based column index in the workspace's scrolling layout (tiled windows).
    pub column: Option<usize>,
    /// 1-based tile index within its column (tiled windows).
    pub tile: Option<usize>,
    pub focused: bool,
}

impl Default for WindowRecord {
    fn default() -> Self {
        Self {
            title: None,
            workspace: WorkspaceRef::default(),
            floating: false,
            state: WindowState::Normal,
            pos: None,
            size: None,
            column: None,
            tile: None,
            focused: false,
        }
    }
}

/// One launchable instance of an app: the exact command line and working
/// directory the process had at save time, plus the windows it owned.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Launch {
    #[serde(default)]
    pub argv: Vec<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    /// Windows owned by this instance at save time (v2+; empty for older files).
    #[serde(default)]
    pub windows: Vec<WindowRecord>,
}

/// A detected app (the unit of the "Apps" tab in process monitors) together
/// with everything needed to relaunch and re-place it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AppEntry {
    /// Human-readable name (desktop entry name, app id, or binary name).
    pub name: String,
    /// Matched desktop entry id, if any.
    pub desktop_id: Option<String>,
    /// The window app-id the app was seen with, if any.
    pub app_id: Option<String>,
    /// How many toplevel windows belonged to this app at save time.
    pub windows: u32,
    /// Distinct launch commands for this app (one per unique argv+cwd).
    pub instances: Vec<Launch>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Session {
    pub version: u32,
    pub saved_at_epoch_ms: u64,
    /// Which end-of-session event produced this snapshot.
    pub event: String,
    pub apps: Vec<AppEntry>,
}

impl Default for Session {
    fn default() -> Self {
        Self {
            version: STATE_VERSION,
            saved_at_epoch_ms: 0,
            event: String::new(),
            apps: Vec::new(),
        }
    }
}

impl Session {
    /// Whether this snapshot carries per-window layout info worth restoring.
    pub fn has_window_records(&self) -> bool {
        self.apps.iter().any(|app| {
            app.instances
                .iter()
                .any(|instance| !instance.windows.is_empty())
        })
    }
}

/// Where the state file lives (overridable for tests via `NIRI_RESUME_STATE`).
pub fn state_path() -> PathBuf {
    if let Ok(p) = std::env::var("NIRI_RESUME_STATE") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    let base = std::env::var("XDG_STATE_HOME")
        .ok()
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| util::home_dir().map(|h| h.join(".local/state")));
    base.unwrap_or_else(|| PathBuf::from("."))
        .join("niri-resume")
        .join("session.json")
}

/// Atomically write the session snapshot (tmp file + fsync + rename, so the
/// file is complete even if the machine dies mid-shutdown).
pub fn save(session: &Session) -> Result<PathBuf, String> {
    let path = state_path();
    save_to(&path, session).map(|_| path)
}

pub(crate) fn save_to(path: &Path, session: &Session) -> Result<(), String> {
    let dir = path
        .parent()
        .ok_or_else(|| "invalid state file path".to_string())?
        .to_path_buf();
    fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;

    let json = serde_json::to_string_pretty(session)
        .map_err(|e| format!("cannot serialize session: {e}"))?;
    let tmp = dir.join(".session.json.tmp");
    {
        let mut file =
            fs::File::create(&tmp).map_err(|e| format!("cannot create {}: {e}", tmp.display()))?;
        file.write_all(json.as_bytes())
            .map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
        file.sync_all()
            .map_err(|e| format!("cannot fsync {}: {e}", tmp.display()))?;
    }
    fs::rename(&tmp, path).map_err(|e| format!("cannot rename into {}: {e}", path.display()))?;
    if let Ok(d) = fs::File::open(&dir) {
        let _ = d.sync_all(); // best effort: make the rename durable
    }
    Ok(())
}

/// Load the saved session, if one exists.
pub fn load() -> Result<Option<Session>, String> {
    let path = state_path();
    load_from(&path)
}

pub(crate) fn load_from(path: &Path) -> Result<Option<Session>, String> {
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    if text.trim().is_empty() {
        return Ok(None);
    }
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|e| format!("cannot parse {}: {e}", path.display()))
}

/// Delete the saved session. Returns whether a file was removed.
pub fn clear() -> Result<bool, String> {
    let path = state_path();
    match fs::remove_file(&path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(format!("cannot remove {}: {e}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_path(name: &str) -> PathBuf {
        // Each test gets its own directory: tests run in parallel threads.
        let dir = std::env::temp_dir().join(format!(
            "niri-resume-state-{}-{}",
            std::process::id(),
            name.trim_end_matches(".json")
        ));
        fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    #[test]
    fn roundtrip_with_window_records() {
        let path = tmp_path("roundtrip.json");
        let session = Session {
            event: "shutdown".to_string(),
            saved_at_epoch_ms: 1234,
            apps: vec![AppEntry {
                name: "Firefox".into(),
                desktop_id: Some("org.mozilla.firefox".into()),
                app_id: Some("org.mozilla.firefox".into()),
                windows: 2,
                instances: vec![Launch {
                    argv: vec!["firefox".into()],
                    cwd: Some("/home/u".into()),
                    windows: vec![
                        WindowRecord {
                            title: Some("Firefox".into()),
                            workspace: WorkspaceRef {
                                output: Some("HDMI-A-1".into()),
                                name: None,
                                idx: Some(2),
                            },
                            floating: false,
                            state: WindowState::Maximized,
                            pos: None,
                            size: Some((2528.0, 1374.0)),
                            column: Some(1),
                            tile: Some(1),
                            focused: true,
                        },
                        WindowRecord {
                            title: Some("PiP".into()),
                            workspace: WorkspaceRef {
                                output: Some("HDMI-A-1".into()),
                                name: Some("web".into()),
                                idx: None,
                            },
                            floating: true,
                            state: WindowState::Normal,
                            pos: Some((300.0, 200.0)),
                            size: Some((640.0, 360.0)),
                            column: None,
                            tile: None,
                            focused: false,
                        },
                    ],
                }],
            }],
            ..Default::default()
        };
        save_to(&path, &session).unwrap();

        let loaded = load_from(&path).unwrap().unwrap();
        assert_eq!(loaded.version, 2);
        assert!(loaded.has_window_records());
        let record = &loaded.apps[0].instances[0].windows[0];
        assert_eq!(record.state, WindowState::Maximized);
        assert_eq!(record.workspace.idx, Some(2));
        assert_eq!(record.size, Some((2528.0, 1374.0)));
        let floating = &loaded.apps[0].instances[0].windows[1];
        assert_eq!(floating.pos, Some((300.0, 200.0)));
        assert_eq!(floating.workspace.name.as_deref(), Some("web"));
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn v1_file_loads_with_empty_records() {
        let path = tmp_path("v1.json");
        fs::write(
            &path,
            r#"{"version":1,"saved_at_epoch_ms":9,"event":"logout",
                "apps":[{"name":"X","desktop_id":null,"app_id":null,"windows":1,
                "instances":[{"argv":["x"],"cwd":null}]}]}"#,
        )
        .unwrap();
        let session = load_from(&path).unwrap().unwrap();
        assert!(!session.has_window_records());
        assert_eq!(session.apps[0].instances[0].windows.len(), 0);
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn load_missing_returns_none() {
        let path = tmp_path("missing.json");
        let _ = fs::remove_file(&path);
        assert!(load_from(&path).unwrap().is_none());
    }
}
