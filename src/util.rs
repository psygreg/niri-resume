//! Tiny shared helpers.

use std::path::PathBuf;

/// Final path component of a program path or name (`/usr/bin/foo` → `foo`).
pub fn basename(s: &str) -> &str {
    match s.rfind('/') {
        Some(i) => &s[i + 1..],
        None => s,
    }
}

/// The user's home directory (`$HOME`).
pub fn home_dir() -> Option<PathBuf> {
    std::env::var("HOME")
        .ok()
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
}
