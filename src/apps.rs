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
//!
//! Each window is additionally classified (normal / maximized / fullscreen)
//! and its workspace, size and position recorded, so restore can rebuild the
//! layout. Since niri's IPC does not expose maximized/fullscreen flags, the
//! classification is geometric: niri implements both as *column* states, and
//! a fullscreen column is taller than the workspace's baseline tile height
//! (it covers struts and bars), while a maximized column is the widest tile
//! on its output. Comparisons stay relative to sibling tiles on the same
//! output, so no output/scale knowledge is needed.

use crate::desktop::{self, Database, Entry};
use crate::ipc::{Window, Workspace};
use crate::state::{AppEntry, Launch, WindowRecord, WindowState, WorkspaceRef};
use crate::util;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

/// Names always excluded from the app list, even if not present in the niri
/// config: the compositor and this tool itself.
pub const HARD_EXCLUDED: &[&str] = &["niri", "niri-resume"];

/// A window counts as fullscreen when its tile is this much taller than the
/// output's baseline (most common) tiled tile height.
const FULLSCREEN_HEIGHT_MARGIN: f64 = 8.0;

/// A window counts as maximized when its tile width is the output's largest
/// and exceeds the second-largest distinct width by this much.
const MAXIMIZED_WIDTH_MARGIN: f64 = 8.0;

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

/// Classify every window as normal/maximized/fullscreen by comparing tile
/// geometry against sibling tiled windows on the same output.
///
/// niri implements both states on the window's *column*: a maximized column
/// spans the full usable width (still inside the gaps), a fullscreen column
/// covers the entire output including struts and bars. So:
///
/// * baseline height = most common height among windows that are alone in
///   their column (those are full-height by default; stacked tiles are
///   fractions of it and would poison a naive mode);
/// * fullscreen ⇔ tile is taller than that baseline by a clear margin;
/// * maximized ⇔ not fullscreen, and the tile is the widest non-fullscreen
///   tile on the output, exceeding the second-widest distinct width by a
///   clear margin.
///
/// Everything is relative to sibling tiles, so no output/scale knowledge is
/// needed. Known blind spots (documented in the README): an output whose
/// every tiled window is fullscreen, and a lone maximized window with no
/// other tiled window on the output — both fall back to `normal`, which the
/// width-restore still reproduces visually.
pub fn classify(windows: &[Window], workspaces: &[Workspace]) -> HashMap<u64, WindowState> {
    let output_of: HashMap<u64, Option<String>> = workspaces
        .iter()
        .map(|ws| (ws.id, ws.output.clone()))
        .collect();

    /// (window id, tile width, tile height, (column, tile) position)
    type TiledEntry = (u64, f64, f64, (usize, usize));

    // Per output: tiled windows with tile geometry, plus the set of columns
    // that hold more than one window (stacked columns).
    let mut tiled: BTreeMap<String, Vec<TiledEntry>> = BTreeMap::new();
    for window in windows {
        if window.is_floating {
            continue;
        }
        let Some(layout) = &window.layout else {
            continue;
        };
        let Some((w, h)) = layout.tile_size else {
            continue;
        };
        let Some((col, tile)) = layout.pos_in_scrolling_layout else {
            continue;
        };
        let output = window
            .workspace_id
            .and_then(|id| output_of.get(&id).cloned())
            .flatten()
            .unwrap_or_default();
        tiled
            .entry(output)
            .or_default()
            .push((window.id, w, h, (col, tile)));
    }

    let mut baseline_h: HashMap<String, f64> = HashMap::new();
    for (output, entries) in &tiled {
        // Heights of windows that are alone in their column.
        let mut column_counts: HashMap<usize, usize> = HashMap::new();
        for (_, _, _, (col, _)) in entries {
            *column_counts.entry(*col).or_default() += 1;
        }
        let single_tile_heights: Vec<f64> = entries
            .iter()
            .filter(|(_, _, _, (col, _))| column_counts.get(col) == Some(&1))
            .map(|(_, _, h, _)| *h)
            .collect();
        let all_heights: Vec<f64> = entries.iter().map(|(_, _, h, _)| *h).collect();
        baseline_h.insert(
            output.clone(),
            mode(&single_tile_heights)
                .or_else(|| mode(&all_heights))
                .unwrap_or(0.0),
        );
    }

    let mut states = HashMap::new();
    for (output, entries) in &tiled {
        let base = baseline_h.get(output).copied().unwrap_or(0.0);

        // Pass 1: fullscreen is a pure height comparison against the baseline.
        let mut fullscreen: HashSet<u64> = HashSet::new();
        for (id, _, h, _) in entries {
            if *h > base + FULLSCREEN_HEIGHT_MARGIN {
                fullscreen.insert(*id);
                states.insert(*id, WindowState::Fullscreen);
            }
        }

        // Pass 2: maximized = widest non-fullscreen tile by a clear margin.
        let mut widths: Vec<f64> = entries
            .iter()
            .filter(|(id, _, _, _)| !fullscreen.contains(id))
            .map(|(_, w, _, _)| *w)
            .collect();
        widths.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));
        widths.dedup_by(|a, b| (*a - *b).abs() < 0.5);
        let max_w = widths.first().copied().unwrap_or(0.0);
        let second_w = widths.get(1).copied().unwrap_or(f64::NAN);
        for (id, w, _, _) in entries {
            if fullscreen.contains(id) {
                continue;
            }
            let is_maximized =
                (*w - max_w).abs() < 0.5 && max_w - second_w > MAXIMIZED_WIDTH_MARGIN;
            states.insert(
                *id,
                if is_maximized {
                    WindowState::Maximized
                } else {
                    WindowState::Normal
                },
            );
        }
    }
    // Floating windows and windows without layout info stay Normal.
    for window in windows {
        states.entry(window.id).or_insert(WindowState::Normal);
    }
    states
}

/// Most frequent value, rounded to whole pixels to absorb float noise.
fn mode(values: &[f64]) -> Option<f64> {
    let mut counts: HashMap<i64, usize> = HashMap::new();
    for v in values {
        *counts.entry(v.round() as i64).or_default() += 1;
    }
    counts
        .into_iter()
        .max_by_key(|(value, count)| (*count, *value))
        .map(|(value, _)| value as f64)
}

/// Build the app list from the compositor's window list.
pub fn collect(
    windows: &[Window],
    workspaces: &[Workspace],
    db: &Database,
    excluded: &HashSet<String>,
) -> Vec<AppEntry> {
    let states = classify(windows, workspaces);
    let ws_by_id: HashMap<u64, &Workspace> = workspaces.iter().map(|ws| (ws.id, ws)).collect();

    struct InstanceAcc {
        key: (String, Option<String>),
        launch: Launch,
    }
    struct Group {
        name: String,
        desktop_id: Option<String>,
        app_id: Option<String>,
        windows: u32,
        instances: Vec<InstanceAcc>,
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

        // 4. Grouping key, display name and launch command.
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
            .or_else(|| exe_lower.clone().or(argv0_lower.clone()));
        let Some(key) = key else { continue };

        let name = entry
            .and_then(|e| e.name.clone())
            .or_else(|| (!app_id.is_empty()).then_some(app_id.clone()))
            .or_else(|| exe_lower.as_ref().map(|e| util::basename(e).to_string()))
            .or_else(|| argv0_lower.as_ref().map(|a| util::basename(a).to_string()))
            .unwrap_or_else(|| key.clone());

        let launch = instance(proc.as_ref(), entry);
        let record = window_record(window, &ws_by_id, states.get(&window.id).copied());

        let group = groups.entry(key).or_insert_with(|| Group {
            name,
            desktop_id: entry.map(|e| e.id.clone()),
            app_id: (!app_id.is_empty()).then_some(app_id.clone()),
            windows: 0,
            instances: Vec::new(),
        });
        group.windows += 1;

        if let Some(mut launch) = launch {
            let instance_key = (launch.argv.join("\u{0}"), launch.cwd.clone());
            let acc = if let Some(existing) = group
                .instances
                .iter_mut()
                .find(|acc| acc.key == instance_key)
            {
                existing
            } else {
                group.instances.push(InstanceAcc {
                    key: instance_key,
                    launch: Launch {
                        argv: Vec::new(),
                        cwd: None,
                        windows: Vec::new(),
                    },
                });
                group.instances.last_mut().unwrap()
            };
            if acc.launch.argv.is_empty() {
                acc.launch = Launch {
                    argv: std::mem::take(&mut launch.argv),
                    cwd: launch.cwd.take(),
                    windows: Vec::new(),
                };
            }
            acc.launch.windows.push(record);
        }
    }

    let mut out: Vec<AppEntry> = groups
        .into_values()
        .map(|g| AppEntry {
            name: g.name,
            desktop_id: g.desktop_id,
            app_id: g.app_id,
            windows: g.windows,
            instances: g.instances.into_iter().map(|acc| acc.launch).collect(),
        })
        .collect();
    out.sort_by_key(|a| a.name.to_lowercase());
    out
}

fn window_record(
    window: &Window,
    ws_by_id: &HashMap<u64, &Workspace>,
    state: Option<WindowState>,
) -> WindowRecord {
    let workspace = window
        .workspace_id
        .and_then(|id| ws_by_id.get(&id))
        .map(|ws| WorkspaceRef {
            output: ws.output.clone(),
            name: ws.name.clone(),
            idx: Some(ws.idx),
        })
        .unwrap_or_default();
    let layout = window.layout.clone().unwrap_or_default();
    WindowRecord {
        title: window.title.clone(),
        workspace,
        floating: window.is_floating,
        state: state.unwrap_or(WindowState::Normal),
        pos: layout.tile_pos_in_workspace_view,
        size: layout.tile_size,
        column: layout.pos_in_scrolling_layout.map(|(c, _)| c),
        tile: layout.pos_in_scrolling_layout.map(|(_, t)| t),
        focused: window.is_focused,
    }
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
                    windows: Vec::new(),
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
                    windows: Vec::new(),
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
    use crate::ipc::WindowLayout;
    use std::process::{Command, Stdio};

    fn win(id: u64, app_id: Option<&str>, pid: Option<i32>) -> Window {
        Window {
            id,
            title: Some(format!("window {id}")),
            app_id: app_id.map(String::from),
            pid,
            workspace_id: Some(1),
            is_focused: false,
            is_floating: false,
            layout: None,
        }
    }

    fn tiled_win(id: u64, app_id: Option<&str>, ws: u64, size: (f64, f64), col: usize) -> Window {
        Window {
            layout: Some(WindowLayout {
                pos_in_scrolling_layout: Some((col, 1)),
                tile_size: Some(size),
                tile_pos_in_workspace_view: None,
            }),
            ..win(id, app_id, None)
        }
        .with_workspace(ws)
    }

    fn floating_win(id: u64, ws: u64, size: (f64, f64), pos: (f64, f64)) -> Window {
        Window {
            layout: Some(WindowLayout {
                pos_in_scrolling_layout: None,
                tile_size: Some(size),
                tile_pos_in_workspace_view: Some(pos),
            }),
            ..win(id, None, None)
        }
        .with_workspace(ws)
    }

    trait WithWs {
        fn with_workspace(self, ws: u64) -> Window;
    }
    impl WithWs for Window {
        fn with_workspace(mut self, ws: u64) -> Window {
            self.workspace_id = Some(ws);
            self
        }
    }

    fn workspace(id: u64, idx: u8, name: Option<&str>, output: &str) -> Workspace {
        Workspace {
            id,
            idx,
            name: name.map(String::from),
            output: Some(output.to_string()),
            is_focused: false,
            is_active: false,
        }
    }

    fn child_process() -> std::process::Child {
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
        child
    }

    #[test]
    fn groups_windows_by_app_and_dedups_instances() {
        let mut child = child_process();
        let pid = child.id() as i32;
        let db = Database::default();
        let excluded = exclusion_set(HashSet::new());

        let apps = collect(
            &[
                win(1, Some("sleeptest"), Some(pid)),
                win(2, Some("sleeptest"), Some(pid)),
                win(3, Some("sleeptest"), Some(pid)),
            ],
            &[],
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
        // All three windows attributed to the single instance.
        assert_eq!(apps[0].instances[0].windows.len(), 3);
    }

    #[test]
    fn excluded_processes_are_skipped() {
        let mut child = child_process();
        let pid = child.id() as i32;
        let db = Database::default();
        let excluded = exclusion_set(set(&["sleep"])); // as if niri spawned it

        let apps = collect(&[win(1, Some("sleeptest"), Some(pid))], &[], &db, &excluded);
        child.kill().ok();
        let _ = child.wait();
        assert!(apps.is_empty());
    }

    #[test]
    fn app_id_exclusion() {
        let db = Database::default();
        let excluded = exclusion_set(set(&["waybar"]));
        let apps = collect(&[win(1, Some("Waybar"), None)], &[], &db, &excluded);
        assert!(apps.is_empty());
    }

    #[test]
    fn hard_excluded_names() {
        let db = Database::default();
        let excluded = exclusion_set(HashSet::new());
        let apps = collect(&[win(1, Some("niri-resume"), None)], &[], &db, &excluded);
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

        let apps = collect(&[win(1, Some("TestAppClass"), None)], &[], &db, &excluded);
        assert_eq!(apps.len(), 1);
        assert_eq!(apps[0].name, "Test App");
        assert_eq!(apps[0].desktop_id.as_deref(), Some("testapp"));
        assert_eq!(apps[0].instances[0].argv, vec!["testapp", "--flag"]);
        assert_eq!(apps[0].instances[0].cwd.as_deref(), Some("/tmp"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn window_without_any_resolution_is_dropped_silently() {
        // No app id, no pid: nothing nameable or launchable.
        let db = Database::default();
        let excluded = exclusion_set(HashSet::new());
        let apps = collect(&[win(1, None, None)], &[], &db, &excluded);
        assert!(apps.is_empty());
    }

    #[test]
    fn classifies_maximized_and_fullscreen_geometrically() {
        let workspaces = vec![workspace(1, 1, None, "DP-1")];
        let windows = vec![
            tiled_win(1, None, 1, (1256.0, 1374.0), 1),
            tiled_win(2, None, 1, (1256.0, 1374.0), 2),
            tiled_win(3, None, 1, (2528.0, 1374.0), 3), // maximized
            tiled_win(4, None, 1, (2560.0, 1440.0), 4), // fullscreen
            floating_win(5, 1, (500.0, 300.0), (10.0, 20.0)),
        ];
        let states = classify(&windows, &workspaces);
        assert_eq!(states[&1], WindowState::Normal);
        assert_eq!(states[&2], WindowState::Normal);
        assert_eq!(states[&3], WindowState::Maximized);
        assert_eq!(states[&4], WindowState::Fullscreen);
        assert_eq!(states[&5], WindowState::Normal);
    }

    #[test]
    fn stacked_columns_do_not_confuse_classification() {
        let workspaces = vec![workspace(1, 1, None, "DP-1")];
        let windows = vec![
            tiled_win(1, None, 1, (1256.0, 687.0), 1),
            tiled_win(2, None, 1, (1256.0, 687.0), 1), // stacked below
            tiled_win(3, None, 1, (2528.0, 1374.0), 2), // maximized
        ];
        let states = classify(&windows, &workspaces);
        // Mode height is 687, but 1374 is not > 687+8... actually it is!
        // The maximized tile is taller than stacked tiles; ensure it is still
        // classified by width, not misread as fullscreen.
        assert_eq!(states[&3], WindowState::Maximized);
    }

    #[test]
    fn records_workspace_and_layout() {
        let dir = std::env::temp_dir().join(format!("niri-resume-records-{}", std::process::id()));
        let apps_dir = dir.join("applications");
        std::fs::create_dir_all(&apps_dir).unwrap();
        std::fs::write(
            apps_dir.join("testapp.desktop"),
            "[Desktop Entry]\nType=Application\nName=Test App\nExec=testapp --flag\nStartupWMClass=TestAppClass\nPath=/tmp\n",
        )
        .unwrap();
        let db = Database::from_dirs(std::slice::from_ref(&dir));
        let excluded = exclusion_set(HashSet::new());

        let workspaces = vec![workspace(7, 3, Some("web"), "DP-1")];
        let apps = collect(
            &[tiled_win(1, Some("TestAppClass"), 7, (800.0, 1374.0), 2)],
            &workspaces,
            &db,
            &excluded,
        );
        assert_eq!(apps.len(), 1);
        let record = &apps[0].instances[0].windows[0];
        assert_eq!(apps[0].instances[0].argv, vec!["testapp", "--flag"]);
        assert_eq!(apps[0].instances[0].cwd.as_deref(), Some("/tmp"));
        assert_eq!(record.workspace.name.as_deref(), Some("web"));
        assert_eq!(record.workspace.idx, Some(3));
        assert_eq!(record.workspace.output.as_deref(), Some("DP-1"));
        assert_eq!(record.state, WindowState::Normal);
        assert_eq!(record.size, Some((800.0, 1374.0)));
        assert_eq!(record.column, Some(2));
        assert_eq!(record.tile, Some(1));
        assert!(!record.focused);
        let _ = std::fs::remove_dir_all(dir);
    }

    fn set(items: &[&str]) -> HashSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }
}
