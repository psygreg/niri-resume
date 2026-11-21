# Agent Guidelines (AGENTS.md)

This document outlines the mandatory rules and workflow for all AI agents working within this repository. Adherence to these guidelines is required for task completion and code quality.

## 📜 Code Development Standards
Agents must adhere to standard best practices for the languages used:
- **Rust:** Follow Rust best practices (e.g., idiomatic usage, error handling via `Result`). Ensure memory safety and performance, and that any new code added or dependency introduced is done so in accordance to Rust 1.75, which is the oldest version we support.
- Keep the tool what it is: a single dependency-light binary with no GUI, no daemons and no network access. A new dependency must earn itself — `serde` and `serde_json` are the yardstick; anything heavier (async runtimes, GUI/terminal frameworks, config crates) needs an explicit reason why it cannot be done with the standard library or the existing two.
- Code must stay `cargo fmt`-clean and `cargo clippy --all-targets`-clean, and behavior-level changes land together with inline `#[cfg(test)]` tests, following the existing per-file test style.

## 🔗 niri IPC & State Contract
The tool leans on empirically verified behavior of niri's IPC socket and on a versioned state format; both are load-bearing.
- Do not hard-code guessed niri behavior. The working contract (wire protocol, `Window.pid` semantics, layout fields, action quirks across niri versions, geometric maximized/fullscreen classification) is documented in [ARCHITECTURE.md §6](ARCHITECTURE.md); when niri ships a new release or IPC behavior is touched, re-verify against a live niri session before calling the task done.
- The exclusion contract is a feature, not an implementation detail: apps that the niri config starts at startup, and apps XDG autostart would start, must never be saved or restored. Changes to detection logic must preserve it.
- Changes to the state-file shape bump `STATE_VERSION` in [src/state.rs](../src/state.rs) and keep older files loadable (`#[serde(default)]` tolerance both ways; v1 files restore apps without window placement).

## 🧪 Testing Requirement
**All changes must be accompanied by temporary test harnesses.**
1.  Before considering a task complete, the change must pass associated tests.
2.  These temporary test harnesses can be discarded once all tests have successfully passed and verified the task completion and absence of regressions.
3.  Do not submit code without demonstrable proof of correctness via tests.

## 🚫 Workflow Policy
**Agents must never commit or push code themselves.**
- All changes must remain **uncommitted** in the working directory.
- Agents should stage changes (e.g., `git add .`) but *not* commit them.
- The agent's role is to prepare the changes; human review is required for committing and pushing.

## 🚫 Repository Information Upkeep
**The [ARCHITECTURE.md](ARCHITECTURE.md) file must always be kept up-to-date.**
- Any sufficiently relevant changes must be reflected in the file.

## 🗺️ Documentation
For a detailed map of modules, functions, and application structure, please refer to [ARCHITECTURE.md](ARCHITECTURE.md).
