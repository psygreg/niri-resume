//! The saved session state file (`$XDG_STATE_HOME/niri-resume/session.json`).
//!
//! The file is a full snapshot of the detected apps at the last end-of-session
//! event; each save overwrites it atomically, so stale entries never
//! accumulate.

use crate::util;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

pub const STATE_VERSION: u32 = 1;

/// One launchable instance of an app: the exact command line and the working
/// directory the process had at save time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Launch {
    #[serde(default)]
    pub argv: Vec<String>,
    #[serde(default)]
    pub cwd: Option<String>,
}

/// A detected app (the unit of the "Apps" tab in process monitors) together
/// with everything needed to relaunch it.
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
    use crate::state::AppEntry;

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
    fn roundtrip() {
        let path = tmp_path("roundtrip.json");
        let session = Session {
            event: "shutdown".to_string(),
            saved_at_epoch_ms: 1234,
            apps: vec![AppEntry {
                name: "Firefox".into(),
                desktop_id: Some("org.mozilla.firefox".into()),
                app_id: Some("org.mozilla.firefox".into()),
                windows: 3,
                instances: vec![Launch {
                    argv: vec!["firefox".into(), "--profile".into(), "work".into()],
                    cwd: Some("/home/u".into()),
                }],
            }],
            ..Default::default()
        };
        save_to(&path, &session).unwrap();

        // The written file must be complete and load back identically.
        assert!(path.to_string_lossy().ends_with("roundtrip.json"));
        let loaded = load_from(&path).unwrap().unwrap();
        assert_eq!(loaded.event, "shutdown");
        assert_eq!(loaded.apps.len(), 1);
        assert_eq!(loaded.apps[0].name, "Firefox");
        assert_eq!(loaded.apps[0].windows, 3);
        assert_eq!(loaded.apps[0].instances[0].argv.len(), 3);
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn load_missing_returns_none() {
        let path = tmp_path("missing.json");
        let _ = fs::remove_file(&path);
        assert!(load_from(&path).unwrap().is_none());
    }

    #[test]
    fn older_schema_parses_with_defaults() {
        let path = tmp_path("old.json");
        fs::write(
            &path,
            r#"{"version":1,"saved_at_epoch_ms":9,"event":"logout"}"#,
        )
        .unwrap();
        let session = load_from(&path).unwrap().unwrap();
        assert!(session.apps.is_empty());
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }
}
