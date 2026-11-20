//! Extracts the programs that niri itself starts at startup
//! (`spawn-at-startup` and `spawn-sh-at-startup` nodes, following `include`
//! directives) from the niri config file.
//!
//! niri-resume excludes those apps from the saved session, per its contract:
//! only apps the user launched during the session should be restored, never
//! the panel, notification daemon, wallpaper and friends that niri always
//! starts anyway.
//!
//! This is a deliberately small, line-oriented KDL scanner rather than a full
//! KDL parser: it tracks quotes and comments well enough to find top-level
//! `spawn-*` / `include` nodes, which is all this tool needs.

use crate::util;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Guards against `include` loops.
const MAX_INCLUDE_DEPTH: usize = 8;

/// Lowercased program names that niri starts at startup.
pub fn spawn_names() -> HashSet<String> {
    let mut names = HashSet::new();
    if let Some(path) = config_path() {
        let mut visited = HashSet::new();
        scan_file(&path, &mut names, &mut visited, 0);
    }
    names
}

/// The config file niri loads for this session. Honors `NIRI_CONFIG` the same
/// way niri does, plus a private `NIRI_RESUME_CONFIG` override for tests.
pub fn config_path() -> Option<PathBuf> {
    for var in ["NIRI_RESUME_CONFIG", "NIRI_CONFIG"] {
        if let Ok(p) = std::env::var(var) {
            if !p.is_empty() {
                return Some(PathBuf::from(p));
            }
        }
    }
    let base = match std::env::var("XDG_CONFIG_HOME") {
        Ok(v) if !v.is_empty() => PathBuf::from(v),
        _ => util::home_dir()?.join(".config"),
    };
    let path = base.join("niri").join("config.kdl");
    path.is_file().then_some(path)
}

fn scan_file(
    path: &Path,
    names: &mut HashSet<String>,
    visited: &mut HashSet<PathBuf>,
    depth: usize,
) {
    if depth > MAX_INCLUDE_DEPTH {
        return;
    }
    let key = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    if !visited.insert(key) {
        return; // include cycle or duplicate include
    }
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    let dir = path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    scan_text(&text, &dir, names, visited, depth);
}

/// Scan the text of one config file. Split out from [`scan_file`] for tests.
pub(crate) fn scan_text(
    text: &str,
    dir: &Path,
    names: &mut HashSet<String>,
    visited: &mut HashSet<PathBuf>,
    depth: usize,
) {
    let mut in_block_comment = false;
    for raw_line in text.lines() {
        let line = strip_comments(raw_line, &mut in_block_comment);
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        if let Some(rest) = node_args(line, "spawn-at-startup") {
            collect_spawn_args(&tokenize(rest), names);
        } else if let Some(rest) = node_args(line, "spawn-sh-at-startup") {
            // The whole command is one string handed to `sh -c`; every token
            // in it could be a program that niri starts, so collect them all.
            for arg in tokenize(rest) {
                for token in shell_tokens(&arg) {
                    push_name(&token, names);
                }
            }
        } else if let Some(rest) = node_args(line, "include") {
            for arg in tokenize(rest) {
                if arg.contains('=') {
                    continue; // property such as `optional=true`
                }
                if let Some(target) = resolve_include(&arg, dir) {
                    scan_file(&target, names, visited, depth + 1);
                }
            }
        }
    }
}

/// If `line` starts with the top-level node `name`, return the raw argument text.
fn node_args<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    if !line.starts_with(name) {
        return None;
    }
    let rest = &line[name.len()..];
    match rest.chars().next() {
        None => Some(""),
        Some(c) if c.is_whitespace() || c == ';' => Some(rest.trim_start()),
        Some(_) => None, // a longer node name, e.g. `spawn-at-startupish`
    }
}

/// Extract program names from the arguments of one `spawn-at-startup` node.
fn collect_spawn_args(args: &[String], names: &mut HashSet<String>) {
    // Skip a leading `env` plus its VAR=VAL assignments and flags:
    // `spawn-at-startup env FOO=bar app`.
    let mut idx = 0;
    if args.first().map(String::as_str) == Some("env") {
        idx = 1;
        while idx < args.len() && (args[idx].starts_with('-') || args[idx].contains('=')) {
            idx += 1;
        }
    }

    let mut next_is_command = false;
    for arg in &args[idx..] {
        if next_is_command {
            // Shell command string after -c: include every token in it.
            for token in shell_tokens(arg) {
                push_name(&token, names);
            }
            next_is_command = false;
        } else if is_shell_c_flag(arg) {
            next_is_command = true;
        } else {
            push_name(arg, names);
        }
    }
}

/// `-c`, `-lc`, `-cu`... but not `--long-option`.
fn is_shell_c_flag(arg: &str) -> bool {
    arg.starts_with('-') && !arg.starts_with("--") && arg.contains('c')
}

/// Register a token as an excluded program name. Flags and assignments are
/// skipped; the rest is basename'd and lowercased.
fn push_name(token: &str, names: &mut HashSet<String>) {
    if token.starts_with('-') || token.contains('=') {
        return;
    }
    let name = util::basename(token);
    if name.chars().any(char::is_alphanumeric) {
        names.insert(name.to_lowercase());
    }
}

/// Split a shell command string into rough tokens (quotes removed).
fn shell_tokens(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for c in s.chars() {
        if escaped {
            cur.push(c);
            escaped = false;
            continue;
        }
        match quote {
            Some(q) => {
                if c == '\\' && q == '"' {
                    escaped = true;
                } else if c == q {
                    quote = None;
                } else {
                    cur.push(c);
                }
            }
            None => match c {
                ' ' | '\t' => {
                    if !cur.is_empty() {
                        out.push(std::mem::take(&mut cur));
                    }
                }
                '\'' | '"' => quote = Some(c),
                '\\' => escaped = true,
                _ => cur.push(c),
            },
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Tokenize KDL node arguments: double-quoted strings (with escapes) and barewords.
fn tokenize(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_str = false;
    let mut escaped = false;
    for c in s.chars() {
        if in_str {
            if escaped {
                cur.push(c);
                escaped = false;
                continue;
            }
            match c {
                '\\' => escaped = true,
                '"' => in_str = false,
                _ => cur.push(c),
            }
            continue;
        }
        match c {
            '"' => in_str = true,
            ' ' | '\t' | ';' => {
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

/// Remove `//` and `/* */` comments, respecting quoted strings and KDL raw
/// strings (`r"..."`, `r#"..."#`). Block comment state carries across lines.
fn strip_comments(line: &str, in_block: &mut bool) -> String {
    let mut out = String::with_capacity(line.len());
    let chars: Vec<char> = line.chars().collect();
    let mut i = 0;
    let mut quote = false;
    let mut escaped = false;
    let mut raw_end: Option<&str> = None; // terminator of a raw string, e.g. "\"#"

    while i < chars.len() {
        if *in_block {
            if chars[i] == '*' && chars.get(i + 1) == Some(&'/') {
                *in_block = false;
                i += 2;
                continue;
            }
            i += 1;
            continue;
        }
        if let Some(end) = raw_end {
            let endc: Vec<char> = end.chars().collect();
            if chars[i..].starts_with(&endc[..]) {
                i += endc.len();
                raw_end = None;
                continue;
            }
            out.push(chars[i]);
            i += 1;
            continue;
        }
        if quote {
            if escaped {
                escaped = false;
                out.push(chars[i]);
                i += 1;
                continue;
            }
            match chars[i] {
                '\\' => escaped = true,
                '"' => quote = false,
                _ => {}
            }
            out.push(chars[i]);
            i += 1;
            continue;
        }
        match chars[i] {
            '/' if chars.get(i + 1) == Some(&'/') => break, // line comment
            '/' if chars.get(i + 1) == Some(&'*') => {
                *in_block = true;
                i += 2;
            }
            '"' => {
                quote = true;
                out.push('"');
                i += 1;
            }
            'r' if chars.get(i + 1) == Some(&'#') && chars.get(i + 2) == Some(&'"') => {
                raw_end = Some("\"#");
                out.push('r');
                out.push('#');
                out.push('"');
                i += 3;
            }
            'r' if chars.get(i + 1) == Some(&'"') => {
                raw_end = Some("\"");
                out.push('r');
                out.push('"');
                i += 2;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

/// Resolve an `include` argument relative to the including file's directory
/// (absolute paths and `~/...` as-is).
fn resolve_include(arg: &str, dir: &Path) -> Option<PathBuf> {
    let path = if let Some(rest) = arg.strip_prefix("~/") {
        util::home_dir()?.join(rest)
    } else if arg.starts_with('/') {
        PathBuf::from(arg)
    } else {
        dir.join(arg)
    };
    path.is_file().then_some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(text: &str) -> HashSet<String> {
        let mut names = HashSet::new();
        let mut visited = HashSet::new();
        scan_text(text, Path::new("/tmp"), &mut names, &mut visited, 0);
        names
    }

    #[test]
    fn simple_spawn() {
        assert_eq!(names(r#"spawn-at-startup "waybar""#), set(&["waybar"]));
    }

    #[test]
    fn multi_arg_spawn_collects_args_but_not_flags() {
        let got = names(r#"spawn-at-startup "alacritty" "-e" "fish""#);
        assert!(got.contains("alacritty"));
        assert!(got.contains("fish"));
        assert!(!got.contains("-e"));
    }

    #[test]
    fn env_prefix_is_unwrapped() {
        let got = names(r#"spawn-at-startup env FOO=1 foot --server"#);
        assert!(got.contains("foot"));
        assert!(!got.iter().any(|n| n.contains('=')));
        assert!(!got.contains("--server"));
    }

    #[test]
    fn shell_c_command_tokens_are_collected() {
        let got = names(r#"spawn-at-startup "sh" "-c" "waybar & dunst -config x""#);
        assert!(got.contains("waybar"));
        assert!(got.contains("dunst"));
    }

    #[test]
    fn spawn_sh_at_startup_is_collected() {
        let got = names("spawn-sh-at-startup \"qs -c ~/source/qs/MyAwesomeShell\"");
        assert!(got.contains("qs"));
        assert!(got.contains("myawesomeshell"));
    }

    #[test]
    fn comments_and_kdl_comment_out_are_ignored() {
        let got = names(concat!(
            "/* spawn-at-startup \"blockcommed\" */\n",
            "// spawn-at-startup \"linecommed\"\n",
            "/-spawn-at-startup \"slashcommed\"\n",
            "spawn-at-startup \"real\" // trailing\n",
            "spawn-at-startup \"after\" /* mid */ \"cmd\"\n",
        ));
        assert_eq!(got, set(&["real", "after", "cmd"]));
    }

    #[test]
    fn block_comment_spans_lines() {
        let got = names(concat!(
            "/*\n",
            "spawn-at-startup \"commed\"\n",
            "*/\n",
            "spawn-at-startup \"kept\"\n",
        ));
        assert_eq!(got, set(&["kept"]));
    }

    #[test]
    fn urls_inside_strings_survive_comment_stripping() {
        // The `//` inside the string must not end the line scan before the arg.
        let got = names(r#"spawn-at-startup "firefox" "https://example.com""#);
        assert!(got.contains("firefox"));
    }

    #[test]
    fn raw_strings_do_not_confuse_the_scanner() {
        let got = names(concat!(
            "window-rule {\n",
            "    match app-id=r#\"^org\\.wezfurlong\\.wezterm$\"#\n",
            "}\n",
            "spawn-at-startup \"wezterm\"\n",
        ));
        assert_eq!(got, set(&["wezterm"]));
    }

    #[test]
    fn includes_are_followed() {
        let dir = std::env::temp_dir().join(format!("niri-resume-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("sub.kdl"), "spawn-at-startup \"from_include\"\n").unwrap();
        let text = "include \"sub.kdl\"\nspawn-at-startup \"from_main\"\n";
        let mut set = HashSet::new();
        let mut visited = HashSet::new();
        scan_text(text, &dir, &mut set, &mut visited, 0);
        assert!(set.contains("from_include"));
        assert!(set.contains("from_main"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn include_cycles_terminate() {
        let dir = std::env::temp_dir().join(format!("niri-resume-cycle-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("a.kdl"),
            "include \"b.kdl\"\nspawn-at-startup \"a\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("b.kdl"),
            "include \"a.kdl\"\nspawn-at-startup \"b\"\n",
        )
        .unwrap();
        let mut set = HashSet::new();
        let mut visited = HashSet::new();
        scan_text("include \"a.kdl\"", &dir, &mut set, &mut visited, 0);
        assert!(set.contains("a") && set.contains("b"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn set(items: &[&str]) -> HashSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }
}
