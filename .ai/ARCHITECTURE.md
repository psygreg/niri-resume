# niri-resume — Architecture

> Companion to `AGENTS.md` (rules) — this document describes *how the project is structured*, not *how to work in it*. User-facing setup (build, niri config snippets, CLI reference) lives in [README.md](../README.md); this file is the internal map.

## 1. What niri-resume is

A single-binary Rust CLI that saves the session at end-of-session and restores it at startup, for the niri Wayland compositor. Two invocations, both launched *by niri itself*:

- **Save** (`save-quit` / `save-shutdown` / `save-reboot`, from binds): snapshots the running apps — each app, each window's workspace, size, maximized/fullscreen state, position and column placement — atomically to a state file, **then** performs the session-ending action itself (`quit` via IPC, or `systemctl poweroff`/`reboot`). niri has no exit hook, so the tool owning the exit is what makes the save race-free.
- **Restore** (`restore`, from `spawn-at-startup`): relaunches the saved apps with their exact original argv + cwd (or the desktop entry `Exec` when the saved process is a wrapper's inner binary), then matches each freshly opened window to its record and re-places it (workspace, size, state, position, column order) via niri IPC actions. Restore is bounded-time: it polls for windows with a timeout (`NIRI_RESUME_PLACE_TIMEOUT_MS`, default 12 s) and places whatever matched.

App detection mirrors the "Apps" tabs of Resources/Mission Center: only processes owning at least one toplevel window count — which excludes services/daemons/layer-shell clients by construction — plus explicit exclusion of everything niri starts from its config and everything XDG autostart would start. Launch commands are wrapper-aware: when `/proc` shows a wrapper's `exec` target instead of the program the desktop entry starts (Nix's `chromium` → `chromium-unwrapped`, `env VAR=… app`, shell wrappers), the desktop `Exec` is recorded and used, because only the wrapper supplies the sandbox/library/Wayland environment; this also repairs state files saved before that rule existed.

Everything niri-specific rides on the IPC socket (`$NIRI_SOCKET`), JSON-per-line wire protocol, with a `niri msg --json` fallback. Dependencies: `serde` + `serde_json` only; the release binary is ~650 KB stripped and a run is ~10 ms (plus the bounded placement wait on restore).

## 2. Repository tree (annotated)

```
niri-resume/
├── Cargo.toml / Cargo.lock   # MSRV 1.75, edition 2021; deps: serde, serde_json
├── README.md                 # user-facing docs (build, config snippet, CLI reference, limitations)
├── LICENSE                   # GPL-3.0-or-later
├── .ai/                      # agent docs: AGENTS.md (rules) + ARCHITECTURE.md (this file)
└── src/
    ├── main.rs               # CLI entrypoint, save/restore orchestration
    ├── ipc.rs                # niri IPC socket client (+ niri msg fallback)
    ├── apps.rs               # window → app grouping, /proc resolution, state classification
    ├── config.rs             # niri config.kdl scanner (spawn-at-startup exclusions)
    ├── desktop.rs            # desktop entry database, Exec parsing, XDG autostart ids
    ├── state.rs              # state-file schema + atomic save/load
    ├── place.rs              # window matching + placement plan + executor
    └── util.rs               # basename, home_dir
```

No build scripts, no CI, no vendoring: the whole contract is `cargo build --release` / `cargo test` / `cargo clippy` / `cargo fmt`.

## 3. Runtime flow

```
niri bind: Mod+Shift+E { spawn "niri-resume" "save-quit"; }
  └─> main::save()
        ├─ ipc::windows() + ipc::workspaces()      # live snapshot
        ├─ config::spawn_names()                   # exclusion set (niri config startups)
        ├─ desktop::Database::load()               # desktop entry resolution
        ├─ apps::collect(...)                      # windows → apps → Session
        ├─ state::save()                           # tmp + fsync + rename (durable before anything else)
        └─ run_then: ipc::quit() | systemctl poweroff/reboot

niri config: spawn-at-startup "niri-resume" "restore"
  └─> main::restore()
        ├─ state::load()                           # older-version tolerance
        ├─ build_spawn_plan()                      # dedup + restore-time exclusions (config + XDG autostart)
        ├─ ipc::windows()                          # pre-spawn id snapshot (matching baseline)
        ├─ launch_command() per instance           # saved argv unless desktop Exec is a wrapper
        ├─ spawn each instance (+ desktop-entry Exec fallback on NotFound)
        ├─ place_windows():
        │     poll match_windows() until all matched | timeout
        │     top-up rounds (×2): relaunch instances with still-missing windows
        │     place::plan() → place::execute()     # IPC actions with sleeps
        │     place::verify_states()               # re-check maximized/fullscreen once
        └─ summary to stderr
```

Logging goes to stderr (visible in the session journal); `list`/`--dry-run` print to stdout.

## 4. Module map (`src/`)

| File | Role |
|---|---|
| `main.rs` | CLI dispatch (`run`, `parse_save_flags`), `save` flow + `run_then`/`systemctl`, `restore` flow: `build_spawn_plan` (dedup + `is_restore_excluded`), `launch_command` (saved argv unless the desktop `Exec` starts a different resolved program — wrapper-aware, and repairs old state files), `spawn_instance` (desktop-`Exec` fallback when the recorded binary is gone), `place_windows` orchestration (`poll_until`, two top-up rounds), `verify_placement`, `list`, `clear`. |
| `apps.rs` | The app-detection core. `collect()`: windows → apps grouped by desktop id (or app id / exe basename fallback), each app carrying `AppEntry.instances` deduped by (argv, cwd), each instance carrying its `WindowRecord`s. `/proc` resolution: `read_proc` (exe/argv/cwd; None for zombies and kernel threads) and `find_proc_by_exe` for pid-less windows. `classify()`: geometric maximized/fullscreen detection (see §6). Launch decision: `instance()` prefers the exact process argv unless the desktop entry starts a different program (`exec_agrees_with_process`, resolved through `PATH` and symlinks) — the xwayland-satellite / Nix-wrapped / `env`-prefixed case, where the desktop `Exec` is the correct relaunch because the recorded inner binary lacks the wrapper's environment. Exclusions: `exclusion_set`/`HARD_EXCLUDED` (`niri`, `niri-resume`) + config names, `is_excluded` matches app id / exe basename / argv[0] basename, case-insensitive. |
| `ipc.rs` | The only niri-facing module. Tolerant serde structs (`Window` with `pid`/`layout: WindowLayout`, `Workspace`); `windows()`/`workspaces()` talk directly to `$NIRI_SOCKET` (JSON request line → `{"Ok":...}`/`{"Err":...}` reply line) and fall back to `niri msg --json <sub>`; `send_action(Value)` wraps `{"Action":...}`; `quit()` sends `Quit { skip_confirmation: true }`. Fields tolerate missing/unknown data across niri versions. |
| `state.rs` | The state-file contract (`STATE_VERSION = 2`): `Session { version, saved_at_epoch_ms, event, apps: Vec<AppEntry> }`; `AppEntry { name, desktop_id, app_id, windows, instances: Vec<Launch> }`; `Launch { argv, cwd, windows: Vec<WindowRecord> }`; `WindowRecord { title, workspace: WorkspaceRef, floating, state: WindowState, pos, size, column, tile, focused }`. Every struct is `#[serde(default)]` so older and future files load; `save()` writes tmp + fsync + rename + dir fsync. Paths overridable via `NIRI_RESUME_STATE`. |
| `place.rs` | The layout engine, split into a pure, unit-tested planner and a thin executor. `match_windows()`: pre-spawn id filter → pid-descendant claim → app-id + title tiebreak; `pid_is_descendant()` walks `/proc/<pid>/status` PPid chains. `plan()` emits `Step`s in nine phases: monitor moves → workspace moves (named ones must already exist; numbered ones ascending so niri's "bottommost empty workspace" rule materializes missing ones; windows already on target are skipped) → float conversions → column arrangement (focus + `move-column-to-index`, stacked tiles via `consume-window-into-column`) → sizes (width per column; maximized/fullscreen skip width) → floating positions → maximize → fullscreen → final focus. `execute()` sends the actions with sleeps and calibrates the floating coordinate space per output. `verify_states()` re-checks maximized/fullscreen once against `apps::classify` of the live layout. |
| `config.rs` | Line-oriented KDL scanner (not a full parser): extracts `spawn-at-startup` / `spawn-sh-at-startup` program names from the niri config, following `include` (cycle-guarded, depth ≤ 8). Tracks `//`, `/* */`, `/-` nodes, quoted strings, raw strings (`r"…"`, `r#"…"#`); unwraps `env` prefixes and `-c` shell command tokens. Config path: `NIRI_RESUME_CONFIG` → `NIRI_CONFIG` (same precedence niri uses) → `~/.config/niri/config.kdl`. |
| `desktop.rs` | Freedesktop desktop entry database over XDG data dirs + Flatpak exports (`search_roots`, first match wins); indexes by desktop id, `StartupWMClass`, and `Exec` argv[0] basename. `exec_to_argv()` implements the Exec spec (quoting, `%%`, field codes dropped even inside quotes). `resolve_program()`/`same_program()` resolve program tokens through `PATH` and symlinks, which is what distinguishes a wrapper from the binary it execs. `autostart_desktop_ids()` returns autostart entries that would apply to niri (Hidden / OnlyShowIn / NotShowIn aware) so restore never double-launches them. `try_exec_exists()` gates fallback launches. |
| `util.rs` | `basename()`, `home_dir()`. |

## 5. The state file

`$XDG_STATE_HOME/niri-resume/session.json` — one snapshot per save, fully overwritten each time (nothing accumulates). The schema lives in [state.rs](../src/state.rs); the wire example is in the README. Rules that keep restore robust:

- `STATE_VERSION` bumps only on a shape change that old code could not honor. Loading is tolerant both ways: unknown fields ignored, missing fields defaulted (`#[serde(default)]` everywhere), and a *newer* version still loads with a warning.
- v1 files (apps only, no window records) restore apps without placement — `Session::has_window_records()` gates the placement phase.
- The save is durable (fsync of file + directory) *before* any session-ending action runs.

## 6. External contract: niri IPC

`ipc.rs` and the placement design lean on facts about niri's IPC that are **not documented in one place upstream** — several were verified empirically against a live niri (25.x–26.04). When touching anything IPC-shaped, treat these as the working contract and re-verify against a live session:

- Wire protocol: connect to `$NIRI_SOCKET`, write one JSON request line, read one JSON reply line: `{"Ok": <Response>}` or `{"Err": "…"}`. Unknown fields/variants must be tolerated (they appear across niri versions). `niri msg --json` is the authoritative fallback if the handshake ever shifts.
- `Window.pid` is the PID that created the window's Wayland connection — for X11 windows under xwayland-satellite that is *satellite's* pid, which is why desktop-`Exec` launch wins whenever the process disagrees with the desktop entry.
- Window layout (`niri msg windows --json`, since 25.08): `pos_in_scrolling_layout` is 1-based (column, tile-in-column); `tile_size` is logical f64; `tile_pos_in_workspace_view` is populated **only for floating windows** (their stored, view-anchored position) and is what the snapshot records.
- Maximized/fullscreen are **column** states with no IPC flags. Geometric classification in `apps::classify`: baseline height = mode height among windows that are alone in their column (stacked tiles would poison a naive mode); fullscreen ⇔ taller than baseline + 8 px; maximized ⇔ widest non-fullscreen tile, exceeding the second-widest distinct width + 8 px. All comparisons are per-output and relative, so no scale knowledge is needed. Blind spots (documented in the README): an output whose every tiled window is fullscreen, and a lone maximized window with no other tiled window — both degrade to `normal`, which the width restore still reproduces visually.
- Action quirks (v26.04): `SetWindowWidth/Height` take the size **positionally** as integer logical px (`SizeChange::SetFixed(i32)`) while `MoveFloatingWindow` takes **f64** positions; `MoveColumnToIndex` has no `focus` field in the IPC enum; `MaximizeColumn` has no window id (it acts on the focused column — focus first); `focus-window` crosses workspaces (used for the final focus, which also restores the view).
- Workspaces: ids are arbitrary per session; numbered workspaces are 1-based `idx`; `move-window-to-workspace` resolves names case-insensitively but **does not create named workspaces** (declare them in the niri config); numbered indexes beyond the current count resolve to the bottommost empty workspace, which is why numbered moves are emitted in ascending order (each move materializes the next).
- `move-floating-window` coordinates are **working-area-relative** while recorded positions are absolute — `place::execute` calibrates per output by parking a window at (0,0) and reading back where it landed.
- `Action::Quit` takes `skip_confirmation` — the save path always quits without the confirmation dialog.

## 7. Testing

- `cargo test` — 50 inline tests: config scanner (comments, raw strings, includes, cycles), Exec parser, program resolution through `PATH`/symlinks, desktop indexing/precedence/autostart rules, wrapper-vs-inner-binary launch selection (save and restore), state round-trip + v1 tolerance, IPC reply parsing (tolerant shapes), classification geometry, matching/planning logic. Tests use per-test temp dirs and the env overrides below; two of them spawn a real `sleep` child so `/proc` has something to read.
- Live-verification overrides (see README §Testing): `NIRI_RESUME_WINDOWS_JSON`, `NIRI_RESUME_CONFIG`, `NIRI_RESUME_STATE`, `NIRI_RESUME_PLACE_TIMEOUT_MS`.
- Anything IPC-shaped beyond what the unit tests cover gets verified against a running niri session before it is considered done (see AGENTS.md — that is what the temporary-test-harnesses rule means here in practice).
