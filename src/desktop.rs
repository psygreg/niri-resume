//! Freedesktop desktop entry database.
//!
//! Lets niri-resume resolve windows/app ids to proper app names and launch
//! commands the same way Resources and Mission Center build their "Apps"
//! tabs: by desktop entry id, `StartupWMClass`, or the executable named in
//! `Exec`. It also knows which entries XDG autostart would launch on login,
//! so restore does not double-start them.

use crate::util;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

/// One parsed `[Desktop Entry]` section.
#[derive(Debug, Clone, Default)]
pub struct Entry {
    pub id: String,
    pub name: Option<String>,
    pub exec: Option<String>,
    /// Working directory requested by the entry (`Path=`).
    pub path: Option<String>,
    pub try_exec: Option<String>,
    pub wm_classes: Vec<String>,
    pub terminal: bool,
    pub hidden: bool,
    pub is_application: bool,
}

/// All discovered entries plus lookup indexes (first match wins, respecting
/// XDG data directory precedence).
#[derive(Debug, Default)]
pub struct Database {
    entries: Vec<Entry>,
    by_id: HashMap<String, usize>,
    by_wmclass: HashMap<String, usize>,
    by_exec: HashMap<String, usize>,
}

impl Database {
    /// Scan the standard XDG application directories plus Flatpak exports.
    pub fn load() -> Database {
        Self::from_dirs(&search_roots())
    }

    pub(crate) fn from_dirs(roots: &[PathBuf]) -> Database {
        let mut db = Database::default();
        let mut seen: HashSet<String> = HashSet::new();
        for root in roots {
            let apps = root.join("applications");
            walk_dir(&apps, "", &mut seen, &mut db);
        }
        db
    }

    fn add(&mut self, entry: Entry) {
        // Hidden=true means the entry was removed/uninstalled; Type must be
        // Application and Exec must exist for the entry to be usable.
        if !entry.is_application || entry.hidden || entry.exec.is_none() {
            return;
        }
        let idx = self.entries.len();
        self.by_id.entry(entry.id.to_lowercase()).or_insert(idx);
        for wm in &entry.wm_classes {
            self.by_wmclass.entry(wm.to_lowercase()).or_insert(idx);
        }
        if let Some(exec) = &entry.exec {
            if let Some(first) = exec_to_argv(exec).first() {
                let base = util::basename(first).to_lowercase();
                self.by_exec.entry(base).or_insert(idx);
            }
        }
        self.entries.push(entry);
    }

    /// Match a window's app-id: exact desktop id or `StartupWMClass`,
    /// case-insensitively.
    pub fn by_app_id(&self, app_id: &str) -> Option<&Entry> {
        let l = app_id.to_lowercase();
        self.by_id
            .get(&l)
            .or_else(|| self.by_wmclass.get(&l))
            .map(|&i| &self.entries[i])
    }

    /// Look up an entry by desktop id.
    pub fn by_id(&self, id: &str) -> Option<&Entry> {
        self.by_id
            .get(&id.to_lowercase())
            .map(|&i| &self.entries[i])
    }

    /// Match a process executable name against desktop entry `Exec` commands.
    pub fn by_exec_name(&self, exe_name: &str) -> Option<&Entry> {
        self.by_exec
            .get(&exe_name.to_lowercase())
            .map(|&i| &self.entries[i])
    }
}

fn walk_dir(dir: &Path, prefix: &str, seen: &mut HashSet<String>, db: &mut Database) {
    let Ok(rd) = fs::read_dir(dir) else {
        return;
    };
    let mut children: Vec<_> = rd.flatten().collect();
    children.sort_by_key(|e| e.file_name()); // deterministic order
    for child in children {
        let Ok(ft) = child.file_type() else { continue };
        let name = child.file_name().to_string_lossy().into_owned();
        if ft.is_dir() {
            let sub = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}-{name}")
            };
            walk_dir(&child.path(), &sub, seen, db);
        } else if name.ends_with(".desktop") {
            // Desktop entry ids join subdirectories with '-'.
            let stem = name.trim_end_matches(".desktop").to_string();
            let id = if prefix.is_empty() {
                stem
            } else {
                format!("{prefix}-{stem}")
            };
            if !seen.insert(id.clone()) {
                continue; // a higher-precedence directory already provided it
            }
            if let Ok(text) = fs::read_to_string(child.path()) {
                if let Some(mut entry) = parse_desktop_entry(&text) {
                    entry.id = id;
                    db.add(entry);
                }
            }
        }
    }
}

/// XDG data roots for application lookups, in precedence order.
fn search_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    let data_home = std::env::var("XDG_DATA_HOME")
        .ok()
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| util::home_dir().map(|h| h.join(".local/share")));
    if let Some(dh) = &data_home {
        roots.push(dh.clone());
        roots.push(dh.join("flatpak/exports/share"));
    }
    let data_dirs = std::env::var("XDG_DATA_DIRS")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".to_string());
    for dir in data_dirs.split(':') {
        if !dir.is_empty() {
            roots.push(PathBuf::from(dir));
        }
    }
    roots.push(PathBuf::from("/var/lib/flatpak/exports/share"));

    let mut uniq: Vec<PathBuf> = Vec::new();
    for root in roots {
        if !uniq.contains(&root) {
            uniq.push(root);
        }
    }
    uniq
}

/// Parse the `[Desktop Entry]` section of a .desktop file.
fn parse_desktop_entry(text: &str) -> Option<Entry> {
    let mut entry = Entry::default();
    let mut in_main = false;
    let mut saw_main = false;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') {
            in_main = line == "[Desktop Entry]";
            saw_main |= in_main;
            continue; // ignore [Desktop Action *] and friends
        }
        if !in_main {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key.contains('[') {
            continue; // localized keys like Name[de]
        }
        let value = value.trim();
        match key {
            "Type" => entry.is_application = value == "Application",
            "Name" => entry.name = Some(value.to_string()),
            "Exec" => entry.exec = Some(value.to_string()),
            "Path" => entry.path = Some(value.to_string()),
            "TryExec" => entry.try_exec = Some(value.to_string()),
            "StartupWMClass" => entry.wm_classes.push(value.to_string()),
            "Terminal" => entry.terminal = value.eq_ignore_ascii_case("true"),
            "Hidden" => entry.hidden = value.eq_ignore_ascii_case("true"),
            _ => {}
        }
    }
    saw_main.then_some(entry)
}

/// Whether XDG autostart would show (and thus launch) this entry in a niri
/// session: not hidden, and its OnlyShowIn/NotShowIn doesn't exclude niri.
fn autostart_entry_showable(text: &str) -> bool {
    let mut in_main = false;
    let mut saw_main = false;
    let mut hidden = false;
    let mut only: Option<String> = None;
    let mut not: Option<String> = None;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') {
            in_main = line == "[Desktop Entry]";
            saw_main |= in_main;
            continue;
        }
        if !in_main {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim().to_string();
        match key.trim() {
            "Hidden" => hidden = value.eq_ignore_ascii_case("true"),
            "OnlyShowIn" => only = Some(value),
            "NotShowIn" => not = Some(value),
            _ => {}
        }
    }
    if !saw_main || hidden {
        return false;
    }
    if let Some(list) = only {
        if !list.split(';').any(|d| d.eq_ignore_ascii_case("niri")) {
            return false;
        }
    }
    if let Some(list) = not {
        if list.split(';').any(|d| d.eq_ignore_ascii_case("niri")) {
            return false;
        }
    }
    true
}

/// Desktop ids that XDG autostart would launch on login in a niri session.
/// Restore skips saved apps matching these to avoid double launches (niri as
/// a session runs xdg-desktop-autostart itself).
pub fn autostart_desktop_ids() -> HashSet<String> {
    let mut ids = HashSet::new();
    let mut dirs: Vec<PathBuf> = Vec::new();
    let cfg_dir = std::env::var("XDG_CONFIG_HOME")
        .ok()
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(util::home_dir)
        .map(|p| p.join("autostart"));
    if let Some(d) = cfg_dir {
        dirs.push(d);
    }
    dirs.push(PathBuf::from("/etc/xdg/autostart"));

    for dir in dirs {
        let Ok(rd) = fs::read_dir(&dir) else {
            continue;
        };
        for child in rd.flatten() {
            let name = child.file_name().to_string_lossy().into_owned();
            if !name.ends_with(".desktop") {
                continue;
            }
            if let Ok(text) = fs::read_to_string(child.path()) {
                if autostart_entry_showable(&text) {
                    ids.insert(name.trim_end_matches(".desktop").to_lowercase());
                }
            }
        }
    }
    ids
}

/// Whether an entry's `TryExec` binary exists (checked before using `Exec`).
pub fn try_exec_exists(entry: &Entry) -> bool {
    let Some(t) = entry.try_exec.as_ref() else {
        return true;
    };
    if t.is_empty() {
        return true;
    }
    if t.starts_with('/') {
        return Path::new(t).exists();
    }
    if let Ok(path) = std::env::var("PATH") {
        for dir in path.split(':') {
            if Path::new(dir).join(t).exists() {
                return true;
            }
        }
    }
    false
}

/// Parse a desktop entry `Exec` value into argv, per the Desktop Entry Spec:
/// shell-like quoting, `%%` → literal `%`, and field codes (`%f %F %u %U %i
/// %c %k` and the deprecated ones) dropped — we launch without file arguments.
/// Field codes are recognized even inside quoted arguments.
pub fn exec_to_argv(exec: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut chars = exec.chars().peekable();
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;

    while let Some(c) = chars.next() {
        if !in_single && !in_double && escaped {
            cur.push(c);
            escaped = false;
            continue;
        }
        if c == '%' {
            match chars.next() {
                Some('%') => cur.push('%'),
                Some(n) if n.is_ascii_alphabetic() => {} // field code: drop
                Some(n) => {
                    cur.push('%');
                    cur.push(n);
                }
                None => cur.push('%'),
            }
            continue;
        }
        if in_double {
            match c {
                '\\' => match chars.peek() {
                    Some(&n) if matches!(n, '"' | '\\' | '$' | '`') => {
                        cur.push(n);
                        chars.next();
                    }
                    _ => cur.push('\\'),
                },
                '"' => in_double = false,
                _ => cur.push(c),
            }
            continue;
        }
        if in_single {
            if c == '\'' {
                in_single = false;
            } else {
                cur.push(c);
            }
            continue;
        }
        match c {
            '\\' => escaped = true,
            '\'' => in_single = true,
            '"' => in_double = true,
            ' ' | '\t' => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            _ => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db_from_files(dir: &Path, files: &[(&str, &str)]) -> Database {
        let apps = dir.join("applications");
        fs::create_dir_all(&apps).unwrap();
        for (name, content) in files {
            fs::write(apps.join(name), content).unwrap();
        }
        Database::from_dirs(&[dir.to_path_buf()])
    }

    #[test]
    fn parses_and_indexes_entries() {
        let dir = std::env::temp_dir().join(format!("niri-resume-db-{}", std::process::id()));
        let db = db_from_files(
            &dir,
            &[(
                "testapp.desktop",
                "[Desktop Entry]\nType=Application\nName=Test App\nExec=testapp --flag\nStartupWMClass=TestAppClass\nPath=/tmp\n",
            )],
        );
        let e = db.by_app_id("testapp").unwrap();
        assert_eq!(e.name.as_deref(), Some("Test App"));
        assert_eq!(db.by_app_id("testappclass").unwrap().id, "testapp"); // StartupWMClass
        assert_eq!(db.by_exec_name("testapp").unwrap().id, "testapp");
        assert_eq!(db.by_id("TESTAPP").unwrap().id, "testapp"); // case-insensitive
        assert!(db.by_app_id("nosuchthing").is_none());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn user_dir_overrides_system_dir() {
        let dir = std::env::temp_dir().join(format!("niri-resume-prec-{}", std::process::id()));
        let user = dir.join("user");
        let sys = dir.join("sys");
        for d in [&user, &sys] {
            fs::create_dir_all(d.join("applications")).unwrap();
        }
        fs::write(
            user.join("applications").join("app.desktop"),
            "[Desktop Entry]\nType=Application\nName=User version\nExec=app-user\n",
        )
        .unwrap();
        fs::write(
            sys.join("applications").join("app.desktop"),
            "[Desktop Entry]\nType=Application\nName=System version\nExec=app-sys\n",
        )
        .unwrap();
        let db = Database::from_dirs(&[user, sys]);
        assert_eq!(
            db.by_id("app").unwrap().name.as_deref(),
            Some("User version")
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn subdirectory_ids_use_dashes() {
        let dir = std::env::temp_dir().join(format!("niri-resume-sub-{}", std::process::id()));
        let apps = dir.join("applications").join("wine");
        fs::create_dir_all(&apps).unwrap();
        fs::write(
            apps.join("Programs.desktop"),
            "[Desktop Entry]\nType=Application\nName=Wine prog\nExec=wine\n",
        )
        .unwrap();
        let db = Database::from_dirs(std::slice::from_ref(&dir));
        assert_eq!(db.by_id("wine-Programs").unwrap().id, "wine-Programs");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn skips_non_applications_and_hidden() {
        let dir = std::env::temp_dir().join(format!("niri-resume-skip-{}", std::process::id()));
        let db = db_from_files(
            &dir,
            &[
                (
                    "link.desktop",
                    "[Desktop Entry]\nType=Link\nURL=https://x\nExec=nope\n",
                ),
                (
                    "gone.desktop",
                    "[Desktop Entry]\nType=Application\nHidden=true\nExec=nope\n",
                ),
                (
                    "noexec.desktop",
                    "[Desktop Entry]\nType=Application\nName=No exec\n",
                ),
            ],
        );
        assert!(db.by_id("link").is_none());
        assert!(db.by_id("gone").is_none());
        assert!(db.by_id("noexec").is_none());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn autostart_visibility_rules() {
        let show = "[Desktop Entry]\nType=Application\nExec=x\n";
        assert!(autostart_entry_showable(show));
        assert!(!autostart_entry_showable(
            "[Desktop Entry]\nHidden=true\nExec=x\n"
        ));
        assert!(!autostart_entry_showable(
            "[Desktop Entry]\nExec=x\nNotShowIn=niri\n"
        ));
        assert!(!autostart_entry_showable(
            "[Desktop Entry]\nExec=x\nOnlyShowIn=GNOME;X-Cinnamon;\n"
        ));
        assert!(autostart_entry_showable(
            "[Desktop Entry]\nExec=x\nOnlyShowIn=niri;GNOME;\n"
        ));
    }

    #[test]
    fn exec_field_codes() {
        assert_eq!(exec_to_argv("firefox %u"), vec!["firefox"]);
        assert_eq!(
            exec_to_argv("code --new-window %F"),
            vec!["code", "--new-window"]
        );
        assert_eq!(exec_to_argv("app %%literal"), vec!["app", "%literal"]);
        assert_eq!(
            exec_to_argv("sh -c \"echo 'a b'\""),
            vec!["sh", "-c", "echo 'a b'"]
        );
        assert_eq!(exec_to_argv("app --flag='a b'"), vec!["app", "--flag=a b"]);
        assert_eq!(exec_to_argv("vi +%f"), vec!["vi", "+"]);
        assert_eq!(exec_to_argv("/opt/App/bin/app"), vec!["/opt/App/bin/app"]);
        assert_eq!(
            exec_to_argv("term -e \"cmd %u\""),
            vec!["term", "-e", "cmd "]
        );
    }

    #[test]
    fn try_exec_check() {
        let mut e = Entry::default();
        assert!(try_exec_exists(&e)); // no TryExec
        e.try_exec = Some("/bin/sh".into());
        assert!(try_exec_exists(&e));
        e.try_exec = Some("/definitely/not/here".into());
        assert!(!try_exec_exists(&e));
    }
}
