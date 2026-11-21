//! Post-spawn window placement: matches freshly spawned windows against the
//! recorded window records and rebuilds each window's workspace, size, state
//! (maximized/fullscreen) and position through niri IPC actions.
//!
//! The planning half is a pure function (unit-tested); the executor turns the
//! plan into IPC calls, calibrating the floating coordinate space per output
//! (niri's `move-floating-window` takes working-area-relative coordinates
//! while recorded positions are absolute).

use crate::apps;
use crate::ipc::{self, Window, Workspace};
use crate::state::{WindowRecord, WindowState};
use serde_json::json;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::thread;
use std::time::Duration;

/// A recorded window that restore should try to match and place.
pub struct ExpectedWindow {
    /// Index of the instance in the spawn plan (for pid matching).
    pub instance: usize,
    /// App id recorded for the app (for app-id matching).
    pub app_id: Option<String>,
    pub record: WindowRecord,
}

/// One placement action. Pure data, so planning is unit-testable.
#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    FocusWindow {
        id: u64,
    },
    MoveToMonitor {
        id: u64,
        output: String,
    },
    MoveToWorkspaceByIndex {
        id: u64,
        index: u8,
    },
    MoveToWorkspaceByName {
        id: u64,
        name: String,
    },
    MakeFloating {
        id: u64,
    },
    /// Acts on the focused column; always preceded by `FocusWindow`.
    MoveColumnToIndex {
        index: usize,
    },
    /// Appends the column right of the focused one below it.
    ConsumeWindowIntoColumn,
    SetWidth {
        id: u64,
        width: f64,
    },
    SetHeight {
        id: u64,
        height: f64,
    },
    /// Absolute position (as recorded); the executor converts it to
    /// working-area coordinates after calibration.
    SetFloatingPos {
        id: u64,
        pos: (f64, f64),
        output: Option<String>,
    },
    /// Acts on the focused column; always preceded by `FocusWindow`.
    MaximizeColumn,
    FullscreenWindow {
        id: u64,
    },
}

pub struct Plan {
    pub steps: Vec<Step>,
    pub skipped: Vec<String>,
}

/// Is `child` a descendant process of `ancestor`? (Walks the PPid chain.)
pub fn pid_is_descendant(child: u32, ancestor: u32) -> bool {
    let mut current = child;
    for _ in 0..8 {
        if current == ancestor {
            return true;
        }
        match read_ppid(current as i32) {
            Some(ppid) if ppid > 0 => current = ppid as u32,
            _ => return false,
        }
    }
    false
}

fn read_ppid(pid: i32) -> Option<i32> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("PPid:") {
            return rest.trim().parse().ok();
        }
    }
    None
}

/// Match expected windows against the current window list. Only windows that
/// appeared after the pre-spawn snapshot are eligible. Priority: a process
/// descending from one of the instance's spawned pids, then an app-id match
/// with a title tiebreak.
pub fn match_windows(
    expected: &[ExpectedWindow],
    pre_spawn_ids: &HashSet<u64>,
    current: &[Window],
    spawned_pids: &HashMap<usize, Vec<u32>>,
    is_descendant: &dyn Fn(u32, u32) -> bool,
) -> Vec<Option<u64>> {
    let mut claimed: HashSet<u64> = HashSet::new();
    let mut result = vec![None; expected.len()];

    let app_l = |w: &Window| w.app_id.as_ref().map(|a| a.to_lowercase());
    for (i, exp) in expected.iter().enumerate() {
        let candidates: Vec<&Window> = current
            .iter()
            .filter(|w| !pre_spawn_ids.contains(&w.id) && !claimed.contains(&w.id))
            .collect();

        // Rule 1: descendant of one of the spawned processes.
        if let Some(pids) = spawned_pids.get(&exp.instance) {
            if let Some(hit) = candidates
                .iter()
                .find(|w| {
                    w.pid
                        .is_some_and(|wp| pids.iter().any(|pid| is_descendant(wp as u32, *pid)))
                })
                .copied()
            {
                claimed.insert(hit.id);
                result[i] = Some(hit.id);
                continue;
            }
        }

        // Rule 2: app-id match (title tiebreak).
        let Some(exp_app) = exp.app_id.as_ref().map(|a| a.to_lowercase()) else {
            continue;
        };
        let by_app = |w: &Window| app_l(w) == Some(exp_app.clone());
        let by_title = |w: &Window| match (&exp.record.title, &w.title) {
            (Some(rt), Some(wt)) => rt == wt,
            _ => false,
        };
        let hit = candidates
            .iter()
            .copied()
            .find(|w| by_app(w) && by_title(w))
            .or_else(|| candidates.iter().copied().find(|w| by_app(w)));
        if let Some(hit) = hit {
            claimed.insert(hit.id);
            result[i] = Some(hit.id);
        }
    }
    result
}

fn ws_output_of(window: &Window, workspaces: &[Workspace]) -> Option<String> {
    window
        .workspace_id
        .and_then(|id| workspaces.iter().find(|ws| ws.id == id))
        .and_then(|ws| ws.output.clone())
}

fn named_workspace_exists(name: &str, workspaces: &[Workspace]) -> bool {
    workspaces.iter().any(|ws| {
        ws.name
            .as_ref()
            .is_some_and(|n| n.eq_ignore_ascii_case(name))
    })
}

/// Build the placement steps from the matched windows.
///
/// Phase order: monitor moves, workspace moves (named first, then numbered in
/// ascending index order so niri's "bottommost empty workspace" rule material
/// izes the missing ones), float conversions, column arrangement, sizes,
/// floating positions, maximize, fullscreen, final focus.
pub fn plan(
    expected: &[ExpectedWindow],
    matched: &[Option<u64>],
    current_windows: &[Window],
    workspaces: &[Workspace],
) -> Plan {
    let mut steps = Vec::new();
    let mut skipped = Vec::new();

    let pairs: Vec<(&ExpectedWindow, u64)> = expected
        .iter()
        .zip(matched)
        .filter_map(|(exp, id)| id.map(|id| (exp, id)))
        .collect();

    // Phase 1: monitor moves (only when the recorded output differs and exists).
    for (exp, id) in &pairs {
        let Some(target) = &exp.record.workspace.output else {
            continue;
        };
        let current_output = current_windows
            .iter()
            .find(|w| w.id == *id)
            .and_then(|w| ws_output_of(w, workspaces));
        let output_exists = workspaces
            .iter()
            .any(|ws| ws.output.as_deref() == Some(target));
        if output_exists && current_output.as_deref() != Some(target.as_str()) {
            steps.push(Step::MoveToMonitor {
                id: *id,
                output: target.clone(),
            });
        }
    }

    let current_ws_of = |id: u64| -> Option<&Workspace> {
        current_windows
            .iter()
            .find(|w| w.id == id)
            .and_then(|w| w.workspace_id)
            .and_then(|ws_id| workspaces.iter().find(|ws| ws.id == ws_id))
    };

    // Phase 2a: named workspace moves (only if the workspace exists now; niri
    // does not create named workspaces via IPC).
    for (exp, id) in &pairs {
        let Some(name) = &exp.record.workspace.name else {
            continue;
        };
        if named_workspace_exists(name, workspaces) {
            let already_there = current_ws_of(*id)
                .and_then(|ws| ws.name.clone())
                .is_some_and(|n| n.eq_ignore_ascii_case(name));
            if !already_there {
                steps.push(Step::MoveToWorkspaceByName {
                    id: *id,
                    name: name.clone(),
                });
            }
        } else {
            skipped.push(format!(
                "named workspace `{name}` is not declared in the niri config; window stays put"
            ));
        }
    }

    // Phase 2b: numbered workspace moves, ascending per output (niri creates
    // missing numbered workspaces when windows are moved to them in order).
    let mut numbered: Vec<(String, u8, u64)> = pairs
        .iter()
        .filter_map(|(exp, id)| {
            let ws = &exp.record.workspace;
            if ws.name.is_some() {
                return None;
            }
            let idx = ws.idx?;
            let already_there = current_ws_of(*id)
                .filter(|current| {
                    current.idx == idx && current.output.as_deref() == ws.output.as_deref()
                })
                .is_some();
            if already_there {
                return None;
            }
            let output = ws.output.clone().unwrap_or_default();
            Some((output, idx, *id))
        })
        .collect();
    numbered.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    for (_, idx, id) in numbered {
        steps.push(Step::MoveToWorkspaceByIndex { id, index: idx });
    }

    // Phase 3: float conversions.
    for (exp, id) in &pairs {
        if exp.record.floating {
            steps.push(Step::MakeFloating { id: *id });
        }
    }

    // Phase 4: column arrangement for tiled windows, grouped per workspace.
    let mut columns: BTreeMap<String, Vec<(usize, usize, u64)>> = BTreeMap::new();
    for (exp, id) in &pairs {
        if exp.record.floating {
            continue;
        }
        let Some(col) = exp.record.column else {
            continue;
        };
        let Some(tile) = exp.record.tile else {
            continue;
        };
        let ws = &exp.record.workspace;
        let key = format!(
            "{}|{}|{}",
            ws.output.clone().unwrap_or_default(),
            ws.name.clone().unwrap_or_default(),
            ws.idx.unwrap_or(0)
        );
        columns.entry(key).or_default().push((col, tile, *id));
    }
    for entries in columns.values() {
        let mut arranged = entries.clone();
        arranged.sort_unstable_by_key(|(col, tile, _)| (*col, *tile));
        let mut pointer = 1usize;
        let mut column_start = 0usize;
        while column_start < arranged.len() {
            let col = arranged[column_start].0;
            let column_len = arranged[column_start..]
                .iter()
                .take_while(|(c, _, _)| *c == col)
                .count();
            // First tile: its column becomes column `pointer`.
            steps.push(Step::FocusWindow {
                id: arranged[column_start].2,
            });
            steps.push(Step::MoveColumnToIndex { index: pointer });
            // Additional tiles: park their column right of the previous one,
            // then consume it into the column we are building.
            for offset in 1..column_len {
                let entry = arranged[column_start + offset];
                steps.push(Step::FocusWindow { id: entry.2 });
                steps.push(Step::MoveColumnToIndex { index: pointer + 1 });
                steps.push(Step::FocusWindow {
                    id: arranged[column_start].2,
                });
                steps.push(Step::ConsumeWindowIntoColumn);
            }
            column_start += column_len;
            pointer += 1;
        }
    }

    // Phase 5: sizes. Tiled widths are per column (all tiles share it);
    // maximized/fullscreen columns get their width from the state action.
    let mut width_done: HashSet<u64> = HashSet::new();
    for (exp, id) in &pairs {
        let Some(size) = exp.record.size else {
            continue;
        };
        if exp.record.floating {
            steps.push(Step::SetWidth {
                id: *id,
                width: size.0,
            });
            steps.push(Step::SetHeight {
                id: *id,
                height: size.1,
            });
        } else if exp.record.state == WindowState::Normal {
            if let Some(col) = exp.record.column {
                // Only the topmost recorded tile of a column carries the width.
                let first_of_column = !pairs.iter().any(|(other, other_id)| {
                    other_id != id
                        && other.record.workspace == exp.record.workspace
                        && other.record.column == Some(col)
                        && other
                            .record
                            .tile
                            .is_some_and(|t| t < exp.record.tile.unwrap_or(usize::MAX))
                });
                if first_of_column && width_done.insert(*id) {
                    steps.push(Step::SetWidth {
                        id: *id,
                        width: size.0,
                    });
                }
            }
        }
    }

    // Phase 6: floating positions (absolute; converted by the executor).
    for (exp, id) in &pairs {
        if let Some(pos) = exp.record.pos.filter(|_| exp.record.floating) {
            steps.push(Step::SetFloatingPos {
                id: *id,
                pos,
                output: exp.record.workspace.output.clone(),
            });
        }
    }

    // Phase 7: maximized (acts on the focused column).
    for (exp, id) in &pairs {
        if exp.record.state == WindowState::Maximized {
            steps.push(Step::FocusWindow { id: *id });
            steps.push(Step::MaximizeColumn);
        }
    }

    // Phase 8: fullscreen.
    for (exp, id) in &pairs {
        if exp.record.state == WindowState::Fullscreen {
            steps.push(Step::FullscreenWindow { id: *id });
        }
    }

    // Phase 9: restore the recorded focus last (also brings the view there).
    if let Some((_, id)) = pairs.iter().find(|(exp, _)| exp.record.focused) {
        steps.push(Step::FocusWindow { id: *id });
    }

    Plan { steps, skipped }
}

fn send_step(step: &Step) -> Result<(), String> {
    let action = match step {
        Step::FocusWindow { id } => json!({ "FocusWindow": { "id": id } }),
        Step::MoveToMonitor { id, output } => {
            json!({ "MoveWindowToMonitor": { "id": id, "output": output } })
        }
        Step::MoveToWorkspaceByIndex { id, index } => {
            json!({ "MoveWindowToWorkspace": { "window_id": id, "reference": { "Index": index }, "focus": false } })
        }
        Step::MoveToWorkspaceByName { id, name } => {
            json!({ "MoveWindowToWorkspace": { "window_id": id, "reference": { "Name": name }, "focus": false } })
        }
        Step::MakeFloating { id } => json!({ "MoveWindowToFloating": { "id": id } }),
        Step::MoveColumnToIndex { index } => {
            json!({ "MoveColumnToIndex": { "index": index } })
        }
        Step::ConsumeWindowIntoColumn => json!({ "ConsumeWindowIntoColumn": {} }),
        Step::SetWidth { id, width } => {
            json!({ "SetWindowWidth": { "id": id, "change": { "SetFixed": width.round() as i32 } } })
        }
        Step::SetHeight { id, height } => {
            json!({ "SetWindowHeight": { "id": id, "change": { "SetFixed": height.round() as i32 } } })
        }
        Step::SetFloatingPos { .. } => unreachable!("handled by apply_floating_pos"),
        Step::MaximizeColumn => json!({ "MaximizeColumn": {} }),
        Step::FullscreenWindow { id } => json!({ "FullscreenWindow": { "id": id } }),
    };
    ipc::send_action(action)
}

fn step_delay(step: &Step, base: Duration) -> Duration {
    match step {
        Step::FullscreenWindow { .. } | Step::MaximizeColumn => base * 3,
        Step::MoveToWorkspaceByIndex { .. } | Step::MoveToWorkspaceByName { .. } => base * 2,
        _ => base,
    }
}

/// Execute the plan against the live compositor. Floating positions are
/// converted from absolute to working-area coordinates, calibrating the
/// origin per output on first use by parking a window at (0,0) and reading
/// back where that landed.
pub fn execute(steps: &[Step], base_delay: Duration) -> usize {
    let mut origins: HashMap<String, (f64, f64)> = HashMap::new();
    let mut applied = 0usize;
    for step in steps {
        let result = match step {
            Step::SetFloatingPos { id, pos, output } => {
                apply_floating_pos(*id, *pos, output.as_deref(), &mut origins, base_delay)
            }
            other => send_step(other),
        };
        match result {
            Ok(()) => {
                applied += 1;
                thread::sleep(step_delay(step, base_delay));
            }
            Err(e) => eprintln!("niri-resume: placement step skipped: {e}"),
        }
    }
    applied
}

fn apply_floating_pos(
    id: u64,
    abs: (f64, f64),
    output: Option<&str>,
    origins: &mut HashMap<String, (f64, f64)>,
    base_delay: Duration,
) -> Result<(), String> {
    let origin = match output.and_then(|o| origins.get(o).copied()) {
        Some(origin) => origin,
        None => {
            // Calibrate: park the window at working-area (0,0) and read back
            // where that landed in absolute coordinates.
            ipc::send_action(json!({
                "MoveFloatingWindow": {
                    "id": id,
                    "x": { "SetFixed": 0.0 },
                    "y": { "SetFixed": 0.0 }
                }
            }))?;
            thread::sleep(base_delay * 3);
            let origin = ipc::windows()?
                .iter()
                .find(|w| w.id == id)
                .and_then(|w| w.layout.as_ref())
                .and_then(|l| l.tile_pos_in_workspace_view)
                .unwrap_or((0.0, 0.0));
            if let Some(out) = output {
                origins.insert(out.to_string(), origin);
            }
            origin
        }
    };
    ipc::send_action(json!({
        "MoveFloatingWindow": {
            "id": id,
            "x": { "SetFixed": abs.0 - origin.0 },
            "y": { "SetFixed": abs.1 - origin.1 }
        }
    }))
}

/// After placement, re-check maximized/fullscreen records: some apps restore
/// their own state before we act, which would make our toggle switch it back
/// off. Re-apply once where the live state disagrees.
pub fn verify_states(expected: &[ExpectedWindow], matched: &[Option<u64>], base_delay: Duration) {
    let Ok(windows) = ipc::windows() else { return };
    let Ok(workspaces) = ipc::workspaces() else {
        return;
    };
    let states = apps::classify(&windows, &workspaces);

    for (exp, id) in expected.iter().zip(matched) {
        let Some(id) = id else { continue };
        let live = states.get(id).copied().unwrap_or(WindowState::Normal);
        match (exp.record.state, live) {
            (WindowState::Fullscreen, WindowState::Fullscreen)
            | (WindowState::Maximized, WindowState::Maximized) => {}
            (WindowState::Fullscreen, _) => {
                if ipc::send_action(json!({ "FullscreenWindow": { "id": id } })).is_ok() {
                    thread::sleep(base_delay * 3);
                }
            }
            (WindowState::Maximized, _) => {
                let _ = ipc::send_action(json!({ "FocusWindow": { "id": id } }));
                thread::sleep(base_delay);
                if ipc::send_action(json!({ "MaximizeColumn": {} })).is_ok() {
                    thread::sleep(base_delay * 3);
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::WorkspaceRef;

    fn window(id: u64, app_id: Option<&str>, ws: u64, pid: Option<i32>) -> Window {
        Window {
            id,
            title: Some(format!("window {id}")),
            app_id: app_id.map(String::from),
            pid,
            workspace_id: Some(ws),
            is_focused: false,
            is_floating: false,
            layout: None,
        }
    }

    fn record(ws: WorkspaceRef) -> WindowRecord {
        WindowRecord {
            title: Some("window 1".into()),
            workspace: ws,
            ..Default::default()
        }
    }

    fn ws_ref_named(output: &str, name: &str) -> WorkspaceRef {
        WorkspaceRef {
            output: Some(output.into()),
            name: Some(name.into()),
            idx: None,
        }
    }

    fn ws_ref_idx(output: &str, idx: u8) -> WorkspaceRef {
        WorkspaceRef {
            output: Some(output.into()),
            name: None,
            idx: Some(idx),
        }
    }

    #[test]
    fn matches_by_pid_descendant_then_app_and_title() {
        let expected = vec![
            ExpectedWindow {
                instance: 0,
                app_id: Some("app".into()),
                record: record(ws_ref_idx("DP-1", 1)),
            },
            ExpectedWindow {
                instance: 0,
                app_id: Some("app".into()),
                record: record(ws_ref_idx("DP-1", 1)),
            },
        ];
        let spawned = HashMap::from([(0usize, vec![100u32])]);
        // Simulated tree: windows 10 (pid 101) and 11 (pid 102) are children
        // of pid 100; window 12 (pid 999) is unrelated.
        let descendant =
            |child: u32, ancestor: u32| ancestor == 100 && (101..=102).contains(&child);
        let current = vec![
            window(10, Some("app"), 1, Some(101)),
            window(11, Some("app"), 1, Some(102)),
            window(12, Some("app"), 1, Some(999)),
        ];
        let matched = match_windows(&expected, &HashSet::new(), &current, &spawned, &descendant);
        assert_eq!(matched, vec![Some(10), Some(11)]);
    }

    #[test]
    fn title_tiebreak_when_no_pid_match() {
        let expected = vec![ExpectedWindow {
            instance: 0,
            app_id: Some("app".into()),
            record: WindowRecord {
                title: Some(" wanted".into()),
                workspace: ws_ref_idx("DP-1", 1),
                ..Default::default()
            },
        }];
        let spawned = HashMap::from([(0usize, Vec::new())]);
        let current = vec![
            window(1, Some("app"), 1, None),
            Window {
                title: Some(" wanted".into()),
                ..window(2, Some("app"), 1, None)
            },
        ];
        let matched = match_windows(&expected, &HashSet::new(), &current, &spawned, &|_, _| {
            false
        });
        assert_eq!(matched, vec![Some(2)]);
    }

    #[test]
    fn never_claims_pre_spawn_windows() {
        let expected = vec![ExpectedWindow {
            instance: 0,
            app_id: Some("app".into()),
            record: record(ws_ref_idx("DP-1", 1)),
        }];
        let spawned = HashMap::new();
        let current = vec![window(1, Some("app"), 1, None)];
        let pre = HashSet::from([1u64]);
        let matched = match_windows(&expected, &pre, &current, &spawned, &|_, _| false);
        assert_eq!(matched, vec![None]);
    }

    #[test]
    fn plan_orders_workspaces_and_skips_missing_names() {
        let expected = vec![
            ExpectedWindow {
                instance: 0,
                app_id: Some("app".into()),
                record: record(ws_ref_idx("DP-1", 3)),
            },
            ExpectedWindow {
                instance: 0,
                app_id: Some("app".into()),
                record: record(ws_ref_idx("DP-1", 2)),
            },
            ExpectedWindow {
                instance: 0,
                app_id: Some("app".into()),
                record: record(ws_ref_named("DP-1", "ghost")),
            },
        ];
        let matched = vec![Some(1), Some(2), Some(3)];
        let workspaces = vec![Workspace {
            id: 1,
            idx: 1,
            name: None,
            output: Some("DP-1".into()),
            is_focused: true,
            is_active: true,
        }];
        let current = vec![
            window(1, Some("app"), 1, None),
            window(2, Some("app"), 1, None),
            window(3, Some("app"), 1, None),
        ];
        let plan = plan(&expected, &matched, &current, &workspaces);

        let ws_moves: Vec<&Step> = plan
            .steps
            .iter()
            .filter(|s| matches!(s, Step::MoveToWorkspaceByIndex { .. }))
            .collect();
        // Ascending index order regardless of record order.
        assert_eq!(
            ws_moves,
            vec![
                &Step::MoveToWorkspaceByIndex { id: 2, index: 2 },
                &Step::MoveToWorkspaceByIndex { id: 1, index: 3 },
            ]
        );
        assert_eq!(plan.skipped.len(), 1);
        assert!(plan.skipped[0].contains("ghost"));
    }

    #[test]
    fn plan_arranges_columns_and_stacks_tiles() {
        // Two recorded columns: column 1 with two tiles, column 2 with one.
        let rec = |col: usize, tile: usize| WindowRecord {
            workspace: ws_ref_idx("DP-1", 1),
            column: Some(col),
            tile: Some(tile),
            size: Some((800.0, 600.0)),
            ..Default::default()
        };
        let expected = vec![
            ExpectedWindow {
                instance: 0,
                app_id: Some("app".into()),
                record: rec(1, 2),
            },
            ExpectedWindow {
                instance: 0,
                app_id: Some("app".into()),
                record: rec(1, 1),
            },
            ExpectedWindow {
                instance: 0,
                app_id: Some("app".into()),
                record: rec(2, 1),
            },
        ];
        let matched = vec![Some(1), Some(2), Some(3)];
        let workspaces = vec![Workspace {
            id: 1,
            idx: 1,
            name: None,
            output: Some("DP-1".into()),
            is_focused: true,
            is_active: true,
        }];
        let current = vec![
            window(1, Some("app"), 1, None),
            window(2, Some("app"), 1, None),
            window(3, Some("app"), 1, None),
        ];
        let plan = plan(&expected, &matched, &current, &workspaces);

        // Expect: (all windows already on workspace idx 1, so no workspace
        // moves) focus tile(1,1) -> index 1; focus tile(1,2) -> index 2;
        // focus tile(1,1) -> consume; focus tile(2,1) -> index 2; widths.
        assert_eq!(
            plan.steps,
            vec![
                Step::FocusWindow { id: 2 },
                Step::MoveColumnToIndex { index: 1 },
                Step::FocusWindow { id: 1 },
                Step::MoveColumnToIndex { index: 2 },
                Step::FocusWindow { id: 2 },
                Step::ConsumeWindowIntoColumn,
                Step::FocusWindow { id: 3 },
                Step::MoveColumnToIndex { index: 2 },
                Step::SetWidth {
                    id: 2,
                    width: 800.0
                },
                Step::SetWidth {
                    id: 3,
                    width: 800.0
                },
            ]
        );
    }

    #[test]
    fn plan_applies_states_and_final_focus() {
        let rec = |state: WindowState, focused: bool| WindowRecord {
            workspace: ws_ref_idx("DP-1", 1),
            state,
            focused,
            size: Some((800.0, 600.0)),
            column: Some(1),
            tile: Some(1),
            ..Default::default()
        };
        let expected = vec![
            ExpectedWindow {
                instance: 0,
                app_id: Some("app".into()),
                record: rec(WindowState::Fullscreen, false),
            },
            ExpectedWindow {
                instance: 0,
                app_id: Some("app".into()),
                record: rec(WindowState::Maximized, true),
            },
        ];
        let matched = vec![Some(1), Some(2)];
        let workspaces = vec![Workspace {
            id: 1,
            idx: 1,
            name: None,
            output: Some("DP-1".into()),
            is_focused: true,
            is_active: true,
        }];
        let current = vec![
            window(1, Some("app"), 1, None),
            window(2, Some("app"), 1, None),
        ];
        let plan = plan(&expected, &matched, &current, &workspaces);

        // Maximized windows must not get a width step; fullscreen comes last
        // before the final focus.
        assert!(!plan
            .steps
            .iter()
            .any(|s| matches!(s, Step::SetWidth { id, .. } if *id == 1 || *id == 2)));
        assert!(matches!(
            plan.steps.last(),
            Some(Step::FocusWindow { id: 2 })
        ));
        assert!(plan.steps.contains(&Step::FullscreenWindow { id: 1 }));
        assert!(plan.steps.contains(&Step::MaximizeColumn));
    }

    #[test]
    fn plan_floats_and_positions() {
        let rec = WindowRecord {
            workspace: ws_ref_idx("DP-1", 2),
            floating: true,
            pos: Some((300.0, 200.0)),
            size: Some((600.0, 400.0)),
            ..Default::default()
        };
        let expected = vec![ExpectedWindow {
            instance: 0,
            app_id: Some("app".into()),
            record: rec,
        }];
        let matched = vec![Some(1)];
        let workspaces = vec![Workspace {
            id: 1,
            idx: 1,
            name: None,
            output: Some("DP-1".into()),
            is_focused: true,
            is_active: true,
        }];
        let current = vec![window(1, Some("app"), 1, None)];
        let plan = plan(&expected, &matched, &current, &workspaces);
        assert!(plan.steps.contains(&Step::MakeFloating { id: 1 }));
        assert!(plan.steps.contains(&Step::SetWidth {
            id: 1,
            width: 600.0
        }));
        assert!(plan.steps.contains(&Step::SetHeight {
            id: 1,
            height: 400.0
        }));
        assert!(plan.steps.iter().any(|s| matches!(
            s,
            Step::SetFloatingPos {
                id: 1,
                pos: (300.0, 200.0),
                ..
            }
        )));
    }
}
