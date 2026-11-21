//! niri-resume: save the session's apps at end-of-session and relaunch them
//! at startup. It has no GUI and is meant to be launched by niri itself:
//!
//! * `spawn-at-startup "niri-resume" "restore"` — relaunch the last session;
//! * binds calling `save-quit` / `save-shutdown` / `save-reboot` — record the
//!   session, then quit / power off / reboot. The save completes and is
//!   fsynced *before* the session-ending action runs, so there is no race.
//!
//! niri has no end-of-session hook, which is why the tool performs the
//! session-ending action itself after recording. No systemd unit is needed.

mod apps;
mod config;
mod desktop;
mod ipc;
mod place;
mod state;
mod util;

use place::ExpectedWindow;
use state::{AppEntry, Session, WorkspaceRef};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::process::{Command, ExitCode};
use std::time::{Duration, Instant};

const BIN: &str = "niri-resume";

const USAGE: &str = "\
niri-resume: session save & restore for the niri Wayland compositor

Usage:
  niri-resume save [--event logout|shutdown|reboot] [--then quit|poweroff|reboot]
  niri-resume save-quit                  Save, then quit niri (logout).
  niri-resume save-shutdown              Save, then power the machine off.
  niri-resume save-reboot                Save, then reboot the machine.
  niri-resume restore [--dry-run] [--no-place]
                                         Launch the apps from the last save
                                         and re-place their windows.
  niri-resume list                       Show the apps that would be saved.
  niri-resume clear                      Delete the saved session.
  niri-resume help                       Show this help.

Add to your niri config:
  spawn-at-startup \"niri-resume\" \"restore\"
  binds { Mod+Shift+E { spawn \"niri-resume\" \"save-quit\"; } }

The state file lives in $XDG_STATE_HOME/niri-resume/session.json.
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("{BIN}: {msg}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &[String]) -> Result<(), String> {
    let Some((cmd, rest)) = args.split_first() else {
        print!("{USAGE}");
        return Ok(());
    };
    match cmd.as_str() {
        "save" => save(&parse_save_flags(rest)?, "logout", None),
        "save-quit" => save(&parse_save_flags(rest)?, "logout", Some("quit")),
        "save-shutdown" | "save-poweroff" => {
            save(&parse_save_flags(rest)?, "shutdown", Some("poweroff"))
        }
        "save-reboot" => save(&parse_save_flags(rest)?, "reboot", Some("reboot")),
        "restore" => {
            let mut dry_run = false;
            let mut no_place = false;
            for arg in rest {
                match arg.as_str() {
                    "--dry-run" => dry_run = true,
                    "--no-place" => no_place = true,
                    other => return Err(format!("unknown flag `{other}` for restore")),
                }
            }
            restore(dry_run, no_place)
        }
        "list" => list(),
        "clear" => clear(),
        "help" | "--help" | "-h" => {
            print!("{USAGE}");
            Ok(())
        }
        "--version" | "-V" => {
            println!("{BIN} {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        other => Err(format!("unknown command `{other}` (try `{BIN} help`)")),
    }
}

struct SaveFlags {
    event: Option<String>,
    then: Option<String>,
}

fn parse_save_flags(args: &[String]) -> Result<SaveFlags, String> {
    let mut flags = SaveFlags {
        event: None,
        then: None,
    };
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--event" => {
                i += 1;
                let value = args.get(i).ok_or("--event needs a value")?;
                flags.event = Some(value.clone());
            }
            "--then" => {
                i += 1;
                let value = args.get(i).ok_or("--then needs a value")?;
                flags.then = Some(value.clone());
            }
            other => return Err(format!("unknown flag `{other}`")),
        }
        i += 1;
    }
    Ok(flags)
}

/// Record the current session, then optionally trigger the session-ending
/// action. The state file is written and fsynced before that action runs.
fn save(flags: &SaveFlags, default_event: &str, forced_then: Option<&str>) -> Result<(), String> {
    let event = flags
        .event
        .clone()
        .unwrap_or_else(|| default_event.to_string());
    let then = forced_then.or(flags.then.as_deref());

    let windows = ipc::windows()?;
    let workspaces = ipc::workspaces().unwrap_or_default();
    let excluded = apps::exclusion_set(config::spawn_names());
    let db = desktop::Database::load();
    let detected = apps::collect(&windows, &workspaces, &db, &excluded);

    let session = Session {
        version: state::STATE_VERSION,
        saved_at_epoch_ms: now_epoch_ms(),
        event,
        apps: detected,
    };
    let path = state::save(&session)?;
    eprintln!(
        "{BIN}: saved {} app(s) to {}",
        session.apps.len(),
        path.display()
    );

    if let Some(action) = then {
        run_then(action)?;
    }
    Ok(())
}

fn run_then(action: &str) -> Result<(), String> {
    match action {
        "quit" => ipc::quit(),
        "poweroff" => systemctl("poweroff"),
        "reboot" => systemctl("reboot"),
        other => Err(format!(
            "unknown --then action `{other}` (expected quit|poweroff|reboot)"
        )),
    }
}

fn systemctl(action: &str) -> Result<(), String> {
    let status = Command::new("systemctl")
        .arg(action)
        .status()
        .map_err(|e| format!("cannot run `systemctl {action}`: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("`systemctl {action}` exited with {status}"))
    }
}

/// One entry of the restore spawn plan: an app instance plus the app-level
/// info needed for window matching.
struct SpawnItem {
    name: String,
    app_id: Option<String>,
    desktop_id: Option<String>,
    instance: state::Launch,
    windows: Vec<state::WindowRecord>,
}

/// Build the spawn plan from the saved session, applying the restore-time
/// exclusions (niri config startups, XDG autostart) and deduplication.
fn build_spawn_plan(session: &Session) -> (Vec<SpawnItem>, usize) {
    let excluded = apps::exclusion_set(config::spawn_names());
    let autostart = desktop::autostart_desktop_ids();

    let mut items: Vec<SpawnItem> = Vec::new();
    let mut skipped = 0usize;
    for app in &session.apps {
        let mut seen: HashSet<(String, Option<String>)> = HashSet::new();
        for instance in &app.instances {
            if instance.argv.is_empty() {
                skipped += 1;
                continue;
            }
            let dedup_key = (instance.argv.join("\u{0}"), instance.cwd.clone());
            if !seen.insert(dedup_key) {
                continue;
            }
            if is_restore_excluded(app, instance, &excluded, &autostart) {
                eprintln!(
                    "{BIN}: skipping {} (started by niri config or XDG autostart)",
                    app.name
                );
                skipped += 1;
                continue;
            }
            items.push(SpawnItem {
                name: app.name.clone(),
                app_id: app.app_id.clone(),
                desktop_id: app.desktop_id.clone(),
                instance: instance.clone(),
                windows: instance.windows.clone(),
            });
        }
    }
    (items, skipped)
}

fn restore(dry_run: bool, no_place: bool) -> Result<(), String> {
    let Some(session) = state::load()? else {
        eprintln!("{BIN}: no saved session found; nothing to restore");
        return Ok(());
    };
    if session.version > state::STATE_VERSION {
        eprintln!(
            "{BIN}: state file version {} is newer than this build; trying anyway",
            session.version
        );
    }
    let placeable = !no_place && session.has_window_records();

    let (items, skipped) = build_spawn_plan(&session);

    if dry_run {
        for item in &items {
            println!(
                "would launch {}: {} (cwd: {})",
                item.name,
                item.instance.argv.join(" "),
                item.instance.cwd.as_deref().unwrap_or("~")
            );
            for record in &item.windows {
                let ws = describe_workspace(&record.workspace);
                let pos = record
                    .pos
                    .map(|(x, y)| format!(" at ({x:.0},{y:.0})"))
                    .unwrap_or_default();
                let size = record
                    .size
                    .map(|(w, h)| format!(" {}x{}", w as i32, h as i32))
                    .unwrap_or_default();
                let column = record
                    .column
                    .map(|c| format!(", column {c}"))
                    .unwrap_or_default();
                println!(
                    "    window: {} — {}{pos}{size}{column}{}",
                    record.title.as_deref().unwrap_or("(no title)"),
                    ws,
                    match record.state {
                        state::WindowState::Fullscreen => ", fullscreen",
                        state::WindowState::Maximized => ", maximized",
                        state::WindowState::Normal => "",
                    }
                );
            }
        }
        println!(
            "{BIN}: dry run — would launch {} instance(s), skipping {skipped}",
            items.len()
        );
        return Ok(());
    }

    // Snapshot the windows that already exist, so freshly spawned ones can be
    // told apart during matching.
    let pre_windows = ipc::windows();
    let ipc_available = pre_windows.is_ok();
    let pre_spawn_ids: HashSet<u64> = match pre_windows {
        Ok(windows) => windows.iter().map(|w| w.id).collect(),
        Err(e) => {
            eprintln!("{BIN}: cannot reach niri IPC ({e}); windows will not be re-placed");
            HashSet::new()
        }
    };

    let mut spawned_pids: HashMap<usize, Vec<u32>> = HashMap::new();
    let mut launched = 0usize;
    for (index, item) in items.iter().enumerate() {
        match spawn_instance(&item.instance, item.desktop_id.as_deref()) {
            Ok(pid) => {
                launched += 1;
                if let Some(pid) = pid {
                    spawned_pids.entry(index).or_default().push(pid);
                } else {
                    spawned_pids.entry(index).or_default();
                }
                eprintln!("{BIN}: launched {}", item.name);
            }
            Err(e) => eprintln!("{BIN}: failed to launch {}: {e}", item.name),
        }
    }
    eprintln!("{BIN}: restore finished: {launched} launched, {skipped} skipped");

    if placeable && ipc_available {
        place_windows(&items, &mut spawned_pids, &pre_spawn_ids);
    }

    Ok(())
}

fn describe_workspace(ws: &WorkspaceRef) -> String {
    if let Some(name) = &ws.name {
        format!("workspace '{name}'")
    } else if let Some(idx) = ws.idx {
        format!("workspace {idx}")
    } else {
        "current workspace".to_string()
    }
}

/// Match freshly opened windows to the recorded ones and re-place them.
/// Single-launch apps that open one window per launch get "top-up" spawns
/// until the recorded window count is matched (bounded to two extra rounds).
fn place_windows(
    items: &[SpawnItem],
    spawned_pids: &mut HashMap<usize, Vec<u32>>,
    pre_spawn_ids: &HashSet<u64>,
) {
    let mut expected: Vec<ExpectedWindow> = Vec::new();
    for (index, item) in items.iter().enumerate() {
        for record in &item.windows {
            expected.push(ExpectedWindow {
                instance: index,
                app_id: item.app_id.clone(),
                record: record.clone(),
            });
        }
    }
    if expected.is_empty() {
        return;
    }

    let timeout = std::env::var("NIRI_RESUME_PLACE_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(12_000);
    let base_delay = Duration::from_millis(120);

    let poll_until = |matched: &mut Vec<Option<u64>>,
                      deadline: Instant,
                      pids: &HashMap<usize, Vec<u32>>|
     -> bool {
        loop {
            if let Ok(current) = ipc::windows() {
                *matched = place::match_windows(
                    &expected,
                    pre_spawn_ids,
                    &current,
                    pids,
                    &place::pid_is_descendant,
                );
                if matched.iter().all(|m| m.is_some()) {
                    return true;
                }
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    };

    eprintln!(
        "{BIN}: waiting up to {} ms for app windows to appear…",
        timeout
    );
    let mut matched: Vec<Option<u64>> = vec![None; expected.len()];
    let mut all_matched = poll_until(
        &mut matched,
        Instant::now() + Duration::from_millis(timeout),
        spawned_pids,
    );

    // Top-up spawning: the snapshot may hold several windows of an app that
    // opens one window per launch (terminals, single-instance handoffs).
    for _ in 0..2 {
        if all_matched {
            break;
        }
        let missing: HashMap<usize, usize> = expected
            .iter()
            .zip(&matched)
            .filter(|(_, m)| m.is_none())
            .fold(HashMap::new(), |mut acc, (exp, _)| {
                *acc.entry(exp.instance).or_default() += 1;
                acc
            });
        if missing.is_empty() {
            break;
        }
        for (index, _) in missing {
            let item = &items[index];
            match spawn_instance(&item.instance, item.desktop_id.as_deref()) {
                Ok(Some(pid)) => spawned_pids.entry(index).or_default().push(pid),
                Ok(None) => {}
                Err(e) => eprintln!("{BIN}: top-up spawn failed for {}: {e}", item.name),
            }
            eprintln!(
                "{BIN}: relaunched {} for its remaining window(s)",
                item.name
            );
            std::thread::sleep(Duration::from_millis(600));
        }
        all_matched = poll_until(
            &mut matched,
            Instant::now() + Duration::from_secs(6),
            spawned_pids,
        );
    }

    let matched_count = matched.iter().flatten().count();
    if matched_count == 0 {
        eprintln!("{BIN}: no app windows could be matched; skipping window placement");
        return;
    }

    let Ok(current) = ipc::windows() else { return };
    let workspaces = ipc::workspaces().unwrap_or_default();
    let plan = place::plan(&expected, &matched, &current, &workspaces);
    for note in &plan.skipped {
        eprintln!("{BIN}: {note}");
    }
    let applied = place::execute(&plan.steps, base_delay);
    verify_placement(&expected, &matched, base_delay);

    eprintln!(
        "{BIN}: placed {matched_count}/{} recorded window(s) with {applied} action(s)",
        expected.len()
    );
}

fn verify_placement(expected: &[ExpectedWindow], matched: &[Option<u64>], base_delay: Duration) {
    // Give the last state action time to settle, then re-check maximized and
    // fullscreen records once (apps may have restored their own state first,
    // making our toggle a no-op in the wrong direction).
    std::thread::sleep(Duration::from_millis(500));
    place::verify_states(expected, matched, base_delay);
}

/// Restore-time exclusions: anything niri config starts at startup, anything
/// XDG autostart would start anyway, and (defensively) this tool itself.
fn is_restore_excluded(
    app: &AppEntry,
    instance: &state::Launch,
    excluded: &HashSet<String>,
    autostart: &HashSet<String>,
) -> bool {
    let matches = |name: &str| -> bool {
        let l = name.to_lowercase();
        excluded.contains(&l) || autostart.contains(&l)
    };
    if let Some(id) = &app.desktop_id {
        if matches(id) {
            return true;
        }
    }
    if let Some(id) = &app.app_id {
        if matches(id) {
            return true;
        }
    }
    if let Some(argv0) = instance.argv.first() {
        if matches(util::basename(argv0)) {
            return true;
        }
    }
    false
}

/// Spawn one instance; returns the child's PID when available (used for
/// window matching).
fn spawn_instance(
    instance: &state::Launch,
    desktop_id: Option<&str>,
) -> Result<Option<u32>, String> {
    let cwd: PathBuf = instance
        .cwd
        .as_ref()
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
        .or_else(util::home_dir)
        .unwrap_or_else(|| PathBuf::from("/"));

    let fail = |e: std::io::Error| format!("cannot launch `{}`: {e}", instance.argv[0]);
    let mut cmd = Command::new(&instance.argv[0]);
    cmd.args(&instance.argv[1..]);
    cmd.current_dir(&cwd);

    match cmd.spawn() {
        Ok(child) => Ok(Some(child.id())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // The binary moved or was removed: try the canonical launch from
            // the matched desktop entry instead.
            if let Some(id) = desktop_id {
                if let Some(entry) = desktop_db().by_id(id) {
                    if desktop::try_exec_exists(entry) {
                        if let Some(exec) = &entry.exec {
                            let argv = desktop::exec_to_argv(exec);
                            if !argv.is_empty() {
                                let mut cmd = Command::new(&argv[0]);
                                cmd.args(&argv[1..]);
                                let dir = entry
                                    .path
                                    .as_ref()
                                    .map(PathBuf::from)
                                    .filter(|p| p.is_dir())
                                    .unwrap_or_else(|| cwd.clone());
                                cmd.current_dir(dir);
                                return cmd.spawn().map(|child| Some(child.id())).map_err(fail);
                            }
                        }
                    }
                }
            }
            Err(fail(e))
        }
        Err(e) => Err(fail(e)),
    }
}

fn desktop_db() -> desktop::Database {
    desktop::Database::load()
}

fn list() -> Result<(), String> {
    let windows = ipc::windows()?;
    let workspaces = ipc::workspaces().unwrap_or_default();
    let excluded = apps::exclusion_set(config::spawn_names());
    let db = desktop::Database::load();
    let detected = apps::collect(&windows, &workspaces, &db, &excluded);

    println!(
        "{} open toplevel window(s) grouped into {} app(s):",
        windows.len(),
        detected.len()
    );
    for app in &detected {
        println!(
            "  {} — {} window(s), app-id: {}, desktop: {}",
            app.name,
            app.windows,
            app.app_id.as_deref().unwrap_or("—"),
            app.desktop_id.as_deref().unwrap_or("—")
        );
        for instance in &app.instances {
            println!(
                "    launch: {}  (cwd: {})",
                instance.argv.join(" "),
                instance.cwd.as_deref().unwrap_or("~")
            );
            for record in &instance.windows {
                let state = match record.state {
                    state::WindowState::Fullscreen => "fullscreen",
                    state::WindowState::Maximized => "maximized",
                    state::WindowState::Normal => "normal",
                };
                let extra = if record.floating {
                    record
                        .pos
                        .map(|(x, y)| format!("floating at ({x:.0},{y:.0})"))
                        .unwrap_or_else(|| "floating".to_string())
                } else {
                    record
                        .column
                        .map(|c| format!("column {c}"))
                        .unwrap_or_else(|| "tiled".to_string())
                };
                println!(
                    "    window: {} — {}, {}, {}",
                    record.title.as_deref().unwrap_or("(no title)"),
                    describe_workspace(&record.workspace),
                    state,
                    extra
                );
            }
        }
    }
    Ok(())
}

fn clear() -> Result<(), String> {
    if state::clear()? {
        eprintln!("{BIN}: saved session removed");
    } else {
        eprintln!("{BIN}: no saved session present");
    }
    Ok(())
}

fn now_epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
