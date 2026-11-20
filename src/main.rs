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
mod state;
mod util;

use std::collections::HashSet;
use std::path::PathBuf;
use std::process::{Command, ExitCode};

const BIN: &str = "niri-resume";

const USAGE: &str = "\
niri-resume: session save & restore for the niri Wayland compositor

Usage:
  niri-resume save [--event logout|shutdown|reboot] [--then quit|poweroff|reboot]
  niri-resume save-quit                  Save, then quit niri (logout).
  niri-resume save-shutdown              Save, then power the machine off.
  niri-resume save-reboot                Save, then reboot the machine.
  niri-resume restore [--dry-run]        Launch the apps from the last save.
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
            for arg in rest {
                match arg.as_str() {
                    "--dry-run" => dry_run = true,
                    other => return Err(format!("unknown flag `{other}` for restore")),
                }
            }
            restore(dry_run)
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
    let excluded = apps::exclusion_set(config::spawn_names());
    let db = desktop::Database::load();
    let detected = apps::collect(&windows, &db, &excluded);

    let session = state::Session {
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

/// Relaunch the apps recorded at the last end-of-session event.
fn restore(dry_run: bool) -> Result<(), String> {
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

    let excluded = apps::exclusion_set(config::spawn_names());
    let autostart = desktop::autostart_desktop_ids();
    let db = desktop::Database::load();

    let mut launched = 0usize;
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
            if dry_run {
                println!(
                    "would launch {}: {} (cwd: {})",
                    app.name,
                    instance.argv.join(" "),
                    instance.cwd.as_deref().unwrap_or("~")
                );
                launched += 1;
            } else {
                match spawn_instance(instance, app.desktop_id.as_deref(), &db) {
                    Ok(()) => {
                        launched += 1;
                        eprintln!("{BIN}: launched {}", app.name);
                    }
                    Err(e) => eprintln!("{BIN}: failed to launch {}: {e}", app.name),
                }
            }
        }
    }

    if dry_run {
        println!("{BIN}: dry run — would launch {launched} instance(s), skipping {skipped}");
    } else {
        eprintln!("{BIN}: restore finished: {launched} launched, {skipped} skipped");
    }
    Ok(())
}

/// Restore-time exclusions: anything niri config starts at startup, anything
/// XDG autostart would start anyway, and (defensively) this tool itself.
fn is_restore_excluded(
    app: &state::AppEntry,
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

fn spawn_instance(
    instance: &state::Launch,
    desktop_id: Option<&str>,
    db: &desktop::Database,
) -> Result<(), String> {
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
        Ok(_child) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // The binary moved or was removed: try the canonical launch from
            // the matched desktop entry instead.
            if let Some(id) = desktop_id {
                if let Some(entry) = db.by_id(id) {
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
                                return cmd.spawn().map(|_| ()).map_err(fail);
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

fn list() -> Result<(), String> {
    let windows = ipc::windows()?;
    let excluded = apps::exclusion_set(config::spawn_names());
    let db = desktop::Database::load();
    let detected = apps::collect(&windows, &db, &excluded);

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
