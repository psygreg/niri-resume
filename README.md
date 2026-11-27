# niri-resume

Resume your previous session after rebooting or shutting down on niri.

`niri-resume` is a small Rust CLI that records which apps were running when a
niri session ended — including each window's workspace, size, maximized/
fullscreen state and position — and relaunches and re-places them at the next
startup. It has no GUI and no interactivity: niri launches it, it does its job
in a few milliseconds (plus a bounded wait while windows reappear), and it
exits.

## How it works

**End of session** (`save-quit` / `save-shutdown` / `save-reboot`): the tool
asks niri over its IPC socket for the list of open toplevel windows
(`niri msg windows --json` data), maps each window to the process that owns it,
groups windows into apps the same way the Apps tabs of
[Resources](https://gitlab.gnome.org/GNOME/Incubator/resources) and
[Mission Center](https://gitlab.com/mission-center-devs/mission-center) do, and
writes one snapshot file. Only after the snapshot is written and fsynced does
it trigger the session-ending action (quit niri, `systemctl poweroff`, or
`systemctl reboot`), so there is no race between saving and exiting.

**Startup** (`restore`, from `spawn-at-startup`): the tool reads the snapshot
and launches each recorded app with its exact original command line and working
directory. When a recorded process is the inner binary of a wrapper — Nix's
`chromium` wrapper execs `…-chromium-unwrapped/libexec/chromium/chromium`,
distro Firefox scripts exec `/usr/lib*/firefox/firefox`, and `env VAR=… app`
has the same shape — the app's desktop entry `Exec` is used instead, because
only the wrapper sets up the sandbox, library paths and Wayland flags the app
needs. If a recorded binary has since moved or disappeared, it also falls back
to the app's desktop entry `Exec` command. It then watches the compositor for
the freshly opened windows, matches them against the recorded ones, and
re-places each one (see the next section).

### What counts as an "app"

Like the monitors' Apps tabs, only processes that own at least one toplevel
window are considered. This excludes background processes and services by
construction: daemons, D-Bus services and compositing infrastructure own no
toplevels, and layer-shell clients (bars, notification daemons, wallpaper
tools) are not toplevels, so niri never reports them.

Window → process mapping uses the PID that niri reports for each window
(available since the `Window.pid` IPC field). From there the tool reads
`/proc/<pid>` for the executable, argv and working directory, and matches
desktop entries (by desktop id, `StartupWMClass`, or `Exec` binary name) for
names and for wrapper-aware (or fallback) launch commands.

### Window layout restore

For every window the snapshot records:

- the **workspace** (by name for named workspaces, otherwise by 1-based index
  and output),
- the **tile size** in logical pixels,
- the **state**: normal, maximized or fullscreen,
- the **position** for floating windows (niri reports floating windows'
  absolute positions; `niri-ipc` does not expose them for tiled ones),
- the **column arrangement** for tiled windows (1-based column index and
  tile-within-column index),
- which window was **focused**.

At restore, each spawned app's new windows are matched to the recorded ones
(process-descendant PIDs first, then app-id with a title tiebreak) and placed
via niri IPC actions: `move-window-to-workspace` / `move-window-to-monitor`,
`set-window-width`/`set-window-height`, `move-window-to-floating` +
`move-floating-window`, `maximize-column`, `fullscreen-window`, and column
rearrangement with `move-column-to-index` / `consume-window-into-column`.
Maximized and fullscreen are re-verified after applying, because some apps
restore their own state before the tool acts and would make the toggle
backfire. If an app opens fewer windows than were recorded (terminals and
single-instance apps open one window per launch), the instance is relaunched
up to two more times until the recorded count is matched.

niri's IPC does not expose maximized/fullscreen flags, so they are detected
geometrically against sibling tiles on the same output (a fullscreen column is
taller than the baseline tile height because it covers bars and struts; a
maximized column is the widest tile by a clear margin). This has two blind
spots, both falling back to `normal` — which the recorded size still restores
visually: an output where *every* tiled window is fullscreen, and a lone
maximized window with no other tiled window on the output.

### What is excluded

- Anything niri starts at startup from its config: `spawn-at-startup` and
  `spawn-sh-at-startup` entries are parsed out of `config.kdl` (following
  `include` directives) and their programs are excluded from the snapshot.
  At restore time the same filter applies again, so an app you have since
  added to the config is not started twice.
- Anything XDG autostart would start anyway at login (niri as a session runs
  `xdg-desktop-autostart`); restore skips apps whose desktop entry appears in
  `~/.config/autostart` or `/etc/xdg/autostart` and applies to niri.
- The compositor itself and `niri-resume`.
- Windows that cannot be attributed to anything launchable (no app-id, no PID,
  no desktop entry), including X11 windows owned by `xwayland-satellite` that
  have no matching desktop entry.
- Duplicate launch commands: several windows of the same app with the same
  argv and working directory produce a single launch. Different invocations
  (e.g. two editors opened on different directories) are recorded and restored
  separately.

## Why binds instead of a systemd unit

niri has no end-of-session hook: there is no config key that runs something at
logout, shutdown or reboot, and a systemd `ExecStopPost` only runs *after*
niri has exited — at which point the window list is gone and apps have lost
their compositor connection. A user-level systemd service that works reliably
would have to run permanently, watching the IPC event stream, which is exactly
the daemon this tool avoids being.

So the tool inverts the problem: the binds that end the session call
`niri-resume` first, and the tool performs the exit/poweroff/reboot itself
after the snapshot is durably written. No systemd unit is needed, and there is
no race.

## Building and installing

```bash
cargo build --release
install -Dm755 target/release/niri-resume ~/.local/bin/niri-resume
```

The only dependencies are `serde` and `serde_json`; the release build is a
single stripped binary (~650 KB, ~10 ms per run).

## niri configuration

Add this to `~/.config/niri/config.kdl` (validated against niri 26.04):

```kdl
// Relaunch the apps recorded at the last session end.
spawn-at-startup "niri-resume" "restore"

binds {
    // Logout: record the session, then quit niri.
    Mod+Shift+E { spawn "niri-resume" "save-quit"; }

    // Shutdown and reboot: record, then let systemd do the rest.
    // Pick free key combos; these shadow two default-config binds
    // (Mod+Shift+P is DPMS power-off, Mod+Shift+R is preset width back).
    Mod+Shift+P { spawn "niri-resume" "save-shutdown"; }
    Mod+Shift+R { spawn "niri-resume" "save-reboot"; }
}
```

Make sure every way your session can end goes through the tool. The default
config also binds `Ctrl+Alt+Delete { quit; }` — replace it with
`save-quit` too, or that logout will not be recorded. If you shut down from a
launcher menu (wlogout, fuzzel script, etc.), point those entries at
`niri-resume save-shutdown` / `save-reboot` instead of plain
`systemctl poweroff` / `reboot`.

If you prefer to keep your existing binds, the general form is
`niri-resume save --then quit|poweroff|reboot`; `save-quit` and friends are
shorthands for it. `save` with no `--then` only records the session.

## Command reference

| Command | Effect |
| --- | --- |
| `save [--event e] [--then a]` | Record the current session; optionally then run `a` = `quit`, `poweroff` or `reboot`. |
| `save-quit` | `save --event logout --then quit` |
| `save-shutdown` / `save-poweroff` | `save --event shutdown --then poweroff` |
| `save-reboot` | `save --event reboot --then reboot` |
| `restore [--dry-run]` | Launch the apps from the last snapshot and re-place their windows; `--dry-run` only prints the plan, `--no-place` skips placement. |
| `list` | Show the apps that would be saved right now, with their launch commands — use this to check what the exclusions catch. |
| `clear` | Delete the saved snapshot (e.g. to skip one restore). |

## State file

One snapshot at `$XDG_STATE_HOME/niri-resume/session.json` (usually
`~/.local/state/niri-resume/session.json`), written atomically (temp file +
fsync + rename) so a power cut can never leave it half-written. Each save
overwrites it completely; nothing accumulates.

```json
{
  "version": 2,
  "saved_at_epoch_ms": 1795152392416,
  "event": "shutdown",
  "apps": [
    {
      "name": "Fluxer",
      "desktop_id": "app.fluxer.Fluxer",
      "app_id": "app.fluxer.Fluxer",
      "windows": 1,
      "instances": [
        {
          "argv": ["/usr/bin/flatpak", "run", "..."],
          "cwd": null,
          "windows": [
            {
              "title": "Fluxer",
              "workspace": { "output": "HDMI-A-1", "name": null, "idx": 2 },
              "floating": false,
              "state": "maximized",
              "pos": null,
              "size": [2528.0, 1374.0],
              "column": 1,
              "tile": 1,
              "focused": true
            }
          ]
        }
      ]
    }
  ]
}
```

## Limitations and notes

- **Session ends must go through niri-resume.** A crash, a plain `quit` from a
  leftover bind, or a power button handled by logind ends the session without
  a snapshot; the next restore then replays the last successful one.
- **Named workspaces must be declared.** niri's IPC cannot create named
  workspaces at runtime, so windows recorded on a named workspace are only
  re-placed if you declare it in your config (`workspace "name" { }`);
  otherwise they are left where they spawn and a note is logged. Numbered
  workspaces are created on demand and need no declaration.
- **Tiled positions are column order, not pixels.** niri's scrolling layout
  has no absolute-position action for tiled windows; restore rebuilds the
  column order, widths and the maximized/fullscreen states, and finally
  focuses the window that was focused (which also scrolls the view back to
  it). Floating windows are restored at their exact size and position.
- **Counts are best effort.** One process is one app; if it opens fewer
  windows than recorded, the instance is relaunched (up to twice) to top up.
  A browser with five windows may still come back with fewer if the app
  refuses to cooperate.
- **Monitors.** Windows recorded on an output that is connected again are
  moved back to it; outputs that vanished are skipped.
- **Desktop entries are used for names, wrappers and fallbacks.** A desktop
  entry whose `Exec` starts a different program than the saved process (Nix
  packages, `env`-prefixed entries, shell wrappers) is authoritative on restore,
  because the wrapper carries the environment the inner binary needs. Apps
  without a desktop entry are restored from their exact process command line,
  which is the more faithful path anyway.
- **`Terminal=true` apps** are launched bare; wrap them in your terminal
  manually if you use such entries.
- Portal dialogs (e.g. an open file chooser) belong to portal processes that
  do own toplevels, so they can appear in a snapshot if one was open when you
  ended the session. They are harmless to relaunch.

## Testing without a session

For scripting and CI, these environment variables override the live sources:

- `NIRI_RESUME_WINDOWS_JSON` — read the window list from this file (a raw
  array or a full IPC reply) instead of talking to niri.
- `NIRI_RESUME_CONFIG` — use this file as the niri config instead of
  `$XDG_CONFIG_HOME/niri/config.kdl` (the `NIRI_CONFIG` variable that niri
  itself honors is also respected).
- `NIRI_RESUME_STATE` — use this path as the state file.
- `NIRI_RESUME_PLACE_TIMEOUT_MS` — how long restore waits for app windows to
  appear before placing whatever matched (default 12000).

```bash
NIRI_RESUME_WINDOWS_JSON=windows.json NIRI_RESUME_STATE=/tmp/s.json \
  niri-resume save --event shutdown
NIRI_RESUME_STATE=/tmp/s.json niri-resume restore --dry-run
```

## License

GPL-3.0-or-later; see [LICENSE](LICENSE).
