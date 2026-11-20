//! Groups niri toplevel windows into "apps", the way the Apps tabs of
//! Resources and Mission Center do: an app is a process that owns at least
//! one toplevel window, resolved through desktop entries when possible.
//!
//! This criterion is what excludes "background processes and services" by
//! construction: daemons, D-Bus services and compositing infrastructure own
//! no toplevel windows, so they never appear. Layer-shell clients (bars,
//! notification daemons, wallpaper tools) are not toplevels either, so niri
//! never reports them here.
//!
//! On top of that, apps that niri itself starts from its config
//! (`spawn-at-startup` / `spawn-sh-at-startup`) are excluded explicitly.

use crate::desktop::{self, Database, Entry};
use crate::ipc::Window;
use crate::state::{AppEntry, Launch};
use crate::util;
use std::collections::{BTreeMap, HashSet};
use std::path::Path;

/// Names always excluded from the app list, even if not present in the niri
/// config: the compositor and this tool itself.
pub const HARD_EXCLUDED: &[&str] = &["niri", "niri-resume"];

/// Merge config-derived names with the hard-coded exclusions.
pub fn exclusion_set(config_names: HashSet<String>) -> HashSet<String> {
    let mut set = config_names;
    for name in HARD_EXCLUDED {
        set.insert(name.to_string());
    }
    set
}

struct ProcInfo {
    exe: Option<String>,
    argv: Vec<String>,
    cwd: Option<String>,
}

/// Read /proc for `pid`; returns None for kernel threads and zombies (no exe,
/// no argv), which conveniently excludes them as well.
fn read_proc(pid: i32) -> Option<ProcInfo> {
    let base = Path::new("/proc").join(pid.to_string());
    let exe = std::fs::read_link(base.join("exe"))
        .ok()
        .map(|p| p.to_string_lossy().into_owned());
    let argv: Vec<String> = std::fs::read_to_string(base.join("cmdline"))
        .ok()
        .map(|s| {
            s.split('\0')
                .filter(|p| !p.is_empty())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default();
    let cwd = std::fs::read_link(base.join("cwd"))
        .ok()
        .map(|p| p.to_string_lossy().into_owned());
    if exe.is_none() && argv.is_empty() {
        return None;
    }
    Some(ProcInfo { exe, argv, cwd })
}

/// Find a running process whose executable basename matches `name_l`;
/// used when niri reports a window without a usable PID. Prefers the lowest
/// PID, which is usually the main/parent process.
fn find_proc_by_exe(name_l: &str) -> Option<ProcInfo> {
    let rd = std::fs::read_dir(Path::new("/proc")).ok()?;
    let mut best: Option<(i32, ProcInfo)> = None;
    for child in rd.flatten() {
        let Ok(pid) = child.file_name().to_string_lossy().parse::<i32>() else {
            continue;
        };
        if let Some(info) = read_proc(pid) {
            if let Some(exe) = &info.exe {
                if util::basename(exe).to_lowercase() == name_l
                    && best.as_ref().map_or(true, |(b, _)| pid < *b)
                {
                    best = Some((pid, info));
                }
            }
        }
    }
    best.map(|(_, info)| info)
}

/// Build the app list from the compositor's window list.
pub fn collect(windows: &[Window], db: &Database, excluded: &HashSet<String>) -> Vec<AppEntry> {
    struct Group {
        name: String,
        desktop_id: Option<String>,
        app_id: Option<String>,
        windows: u32,
        instances: Vec<Launch>,
        seen: HashSet<(String, Option<String>)>,
    }
    let mut groups: BTreeMap<String, Group> = BTreeMap::new();

    for window in windows {
        let app_id = window.app_id.clone().unwrap_or_default();
        let app_id_l = app_id.to_lowercase();

        // 1. Resolve the owning process and any matching desktop entry.
        let mut proc = window.pid.and_then(read_proc);
        let mut entry: Option<&Entry> = None;
        if !app_id_l.is_empty() {
            entry = db.by_app_id(&app_id_l);
        }
        if entry.is_none() {
            if let Some(exe) = proc.as_ref().and_then(|p| p.exe.as_deref()) {
                entry = db.by_exec_name(util::basename(exe));
            }
        }
        // Windows without a PID (rare): try to find the process by app id.
        if proc.is_none() && entry.is_none() && !app_id_l.is_empty() {
            proc = find_proc_by_exe(&app_id_l);
            if entry.is_none() {
                if let Some(exe) = proc.as_ref().and_then(|p| p.exe.as_deref()) {
                    entry = db.by_exec_name(util::basename(exe));
                }
            }
        }

        // 2. Skip windows we could never restore: X11 windows whose Wayland
        //    connection belongs to xwayland-satellite and that have no
        //    desktop entry (the recorded command would be satellite's).
        if entry.is_none() {
            let is_satellite = proc
                .as_ref()
                .and_then(|p| p.exe.as_deref())
                .map(|exe| util::basename(exe).eq_ignore_ascii_case("xwayland-satellite"))
                .unwrap_or(false);
            if is_satellite {
                continue;
            }
        }

        // 3. Exclusions: niri config startups, the compositor, and this tool.
        if is_excluded(&app_id_l, proc.as_ref(), excluded) {
            continue;
        }

        // 4. Grouping key and display name.
        let exe_lower = proc
            .as_ref()
            .and_then(|p| p.exe.as_deref())
            .map(|e| e.to_lowercase());
        let argv0_lower = proc
            .as_ref()
            .and_then(|p| p.argv.first().cloned())
            .map(|a| a.to_lowercase());
        let key = entry
            .map(|e| e.id.to_lowercase())
            .filter(|k| !k.is_empty())
            .or_else(|| (!app_id_l.is_empty()).then(|| app_id_l.clone()))
            .or(exe_lower.clone())
            .or(argv0_lower.clone());
        let Some(key) = key else { continue };

        let name = entry
            .and_then(|e| e.name.clone())
            .or_else(|| (!app_id.is_empty()).then_some(app_id.clone()))
            .or_else(|| exe_lower.as_ref().map(|e| util::basename(e).to_string()))
            .or_else(|| argv0_lower.as_ref().map(|a| util::basename(a).to_string()))
            .unwrap_or_else(|| key.clone());

        let launch = instance(proc.as_ref(), entry);

        let group = groups.entry(key).or_insert_with(|| Group {
            name,
            desktop_id: entry.map(|e| e.id.clone()),
            app_id: (!app_id.is_empty()).then_some(app_id.clone()),
            windows: 0,
            instances: Vec::new(),
            seen: HashSet::new(),
        });
        group.windows += 1;
        if let Some(l) = launch {
            let dedup_key = (l.argv.join("\u{0}"), l.cwd.clone());
            if !l.argv.is_empty() && group.seen.insert(dedup_key) {
                group.instances.push(l);
            }
        }
    }

    let mut out: Vec<AppEntry> = groups
        .into_values()
        .map(|g| AppEntry {
            name: g.name,
            desktop_id: g.desktop_id,
            app_id: g.app_id,
            windows: g.windows,
            instances: g.instances,
        })
        .collect();
    out.sort_by_key(|a| a.name.to_lowercase());
    out
}

/// Decide the launch command for a window's owning process.
///
/// Prefers the exact command line (that is what "resume this session" means),
/// unless it disagrees with the matched desktop entry — which happens for X11
/// windows under xwayland-satellite (the connection owner is satellite, not
/// the app) and for wrapper-script launches — in which case the desktop
/// entry's `Exec` is the correct thing to relaunch.
fn instance(proc: Option<&ProcInfo>, entry: Option<&Entry>) -> Option<Launch> {
    if let Some(p) = proc {
        let argv: Vec<String> = if p.argv.is_empty() {
            p.exe.as_ref().map(|e| vec![e.clone()]).unwrap_or_default()
        } else {
            p.argv.clone()
        };
        if !argv.is_empty() {
            let agrees = entry.map_or(true, |e| {
                exec_agrees_with_process(e, &argv, p.exe.as_deref())
            });
            if agrees {
                return Some(Launch {
                    argv,
                    cwd: p.cwd.clone(),
                });
            }
        }
    }
    if let Some(e) = entry {
        if let Some(exec) = &e.exec {
            let argv = desktop::exec_to_argv(exec);
            if !argv.is_empty() {
                return Some(Launch {
                    argv,
                    cwd: e.path.clone(),
                });
            }
        }
    }
    None
}

/// Does the desktop entry's `Exec` name the same binary the window's process
/// is running? (Case-insensitive basename comparison.)
fn exec_agrees_with_process(entry: &Entry, argv: &[String], exe: Option<&str>) -> bool {
    let Some(exec) = &entry.exec else {
        return true;
    };
    let exec_argv = desktop::exec_to_argv(exec);
    let Some(first) = exec_argv.first() else {
        return true;
    };
    let first_l = util::basename(first).to_lowercase();
    if let Some(exe) = exe {
        if util::basename(exe).to_lowercase() == first_l {
            return true;
        }
    }
    if let Some(a0) = argv.first() {
        if util::basename(a0).to_lowercase() == first_l {
            return true;
        }
    }
    false
}

fn is_excluded(app_id_l: &str, proc: Option<&ProcInfo>, excluded: &HashSet<String>) -> bool {
    if !app_id_l.is_empty() && excluded.contains(app_id_l) {
        return true;
    }
    if let Some(p) = proc {
        if let Some(exe) = &p.exe {
            if excluded.contains(&util::basename(exe).to_lowercase()) {
                return true;
            }
        }
        if let Some(a0) = p.argv.first() {
            if excluded.contains(&util::basename(a0).to_lowercase()) {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::Window;
    use std::process::{Command, Stdio};

    fn win(id: u64, app_id: Option<&str>, pid: Option<i32>) -> Window {
        Window {
            id,
            title: Some(format!("window {id}")),
            app_id: app_id.map(String::from),
            pid,
            workspace_id: Some(1),
            is_focused: false,
        }
    }

    fn child_process() -> (std::process::Child, String) {
        // A real child process so /proc has something to read. Give the child
        // a moment to finish execve: reading /proc/<pid>/cmdline while the
        // kernel is replacing the arguments can transiently return empty.
        let child = Command::new("sleep")
            .arg("30")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn sleep");
        std::thread::sleep(std::time::Duration::from_millis(100));
        (child, "sleep".to_string())
    }

    #[test]
    fn groups_windows_by_app_and_dedups_instances() {
        let (mut child, _) = child_process();
        let pid = child.id() as i32;
        let db = Database::default();
        let excluded = exclusion_set(HashSet::new());

        let apps = collect(
            &[
                win(1, Some("sleeptest"), Some(pid)),
                win(2, Some("sleeptest"), Some(pid)),
                win(3, Some("sleeptest"), Some(pid)),
            ],
            &db,
            &excluded,
        );

        child.kill().ok();
        let _ = child.wait();

        assert_eq!(apps.len(), 1);
        assert_eq!(apps[0].name, "sleeptest");
        assert_eq!(apps[0].windows, 3);
        assert_eq!(apps[0].instances.len(), 1); // same argv+cwd deduped
        assert_eq!(apps[0].instances[0].argv, vec!["sleep", "30"]);
        assert!(apps[0].desktop_id.is_none());
        assert_eq!(apps[0].app_id.as_deref(), Some("sleeptest"));
    }

    #[test]
    fn excluded_processes_are_skipped() {
        let (mut child, _) = child_process();
        let pid = child.id() as i32;
        let db = Database::default();
        let excluded = exclusion_set(set(&["sleep"])); // as if niri spawned it

        let apps = collect(&[win(1, Some("sleeptest"), Some(pid))], &db, &excluded);
        child.kill().ok();
        let _ = child.wait();
        assert!(apps.is_empty());
    }

    #[test]
    fn app_id_exclusion() {
        let db = Database::default();
        let excluded = exclusion_set(set(&["waybar"]));
        let apps = collect(&[win(1, Some("Waybar"), None)], &db, &excluded);
        assert!(apps.is_empty());
    }

    #[test]
    fn hard_excluded_names() {
        let db = Database::default();
        let excluded = exclusion_set(HashSet::new());
        let apps = collect(&[win(1, Some("niri-resume"), None)], &db, &excluded);
        assert!(apps.is_empty());
    }

    #[test]
    fn desktop_entry_provides_launch_when_pid_unknown() {
        let dir = std::env::temp_dir().join(format!("niri-resume-apps-{}", std::process::id()));
        let apps_dir = dir.join("applications");
        std::fs::create_dir_all(&apps_dir).unwrap();
        std::fs::write(
            apps_dir.join("testapp.desktop"),
            "[Desktop Entry]\nType=Application\nName=Test App\nExec=testapp --flag\nStartupWMClass=TestAppClass\nPath=/tmp\n",
        )
        .unwrap();
        let db = Database::from_dirs(std::slice::from_ref(&dir));
        let excluded = exclusion_set(HashSet::new());

        let apps = collect(&[win(1, Some("TestAppClass"), None)], &db, &excluded);
        assert_eq!(apps.len(), 1);
        assert_eq!(apps[0].name, "Test App"); // via StartupWMClass fallback? no: id match
        assert_eq!(apps[0].desktop_id.as_deref(), Some("testapp"));
        assert_eq!(apps[0].instances[0].argv, vec!["testapp", "--flag"]);
        assert_eq!(apps[0].instances[0].cwd.as_deref(), Some("/tmp"));
        fs_cleanup(&dir);
    }

    #[test]
    fn window_without_any_resolution_is_dropped_silently() {
        // No app id, no pid: nothing nameable or launchable.
        let db = Database::default();
        let excluded = exclusion_set(HashSet::new());
        let apps = collect(&[win(1, None, None)], &db, &excluded);
        assert!(apps.is_empty());
    }

    fn fs_cleanup(dir: &Path) {
        let _ = std::fs::remove_dir_all(dir);
    }

    fn set(items: &[&str]) -> HashSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }
}
