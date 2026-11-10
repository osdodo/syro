//! syro-core: pure logic and services, no gpui, so `cargo test -p syro-core` needs no UI.

// The few user-facing sentences this crate writes for itself.
rust_i18n::i18n!("locales", fallback = "en");

pub mod agent;
pub mod commit_message;
pub mod api;
pub mod config;
pub mod event;
pub mod fs_tree;
pub mod git;
pub mod hooks;
pub mod model;
pub mod pricing;
pub mod resource;
pub mod session;
pub mod store;
pub mod term;

use std::path::{Path, PathBuf};

/// `~/.config/syro` — settings and state, rather than macOS's `~/Library`.
pub fn config_dir() -> PathBuf {
    home_dir().join(".config").join("syro")
}

pub fn settings_path() -> PathBuf {
    config_dir().join("settings.toml")
}

pub fn state_path() -> PathBuf {
    config_dir().join("state.json")
}

/// Default worktree root: `~/syro/workspaces/<repo>/<name>`.
pub fn workspaces_root() -> PathBuf {
    home_dir().join("syro").join("workspaces")
}

/// Why a name cannot be turned into a worktree directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorktreeNameError {
    Empty,
    NotOneDirectory,
    NoAlphanumeric,
    OutsideRoot,
}

/// The name rules alone, for a form that has no path to join yet.
pub fn worktree_name(name: &str) -> Result<&str, WorktreeNameError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(WorktreeNameError::Empty);
    }
    if name == "." || name == ".." || name.contains('/') || name.contains('\\') {
        return Err(WorktreeNameError::NotOneDirectory);
    }
    if !name.chars().any(char::is_alphanumeric) {
        return Err(WorktreeNameError::NoAlphanumeric);
    }
    Ok(name)
}

/// `<root>/<repo>/<name>`.
///
/// The one place a worktree name is validated: `git worktree add` would happily write `../../elsewhere`.
pub fn worktree_dir(root: &Path, repo: &str, name: &str) -> Result<PathBuf, WorktreeNameError> {
    let name = worktree_name(name)?;
    let dir = root.join(repo).join(name);
    // Component-wise, not a string prefix: `Path::starts_with` compares
    // components, so `/w-evil` is not inside `/w`.
    if !dir.starts_with(root) {
        return Err(WorktreeNameError::OutsideRoot);
    }
    Ok(dir)
}

/// The user's home directory: `$HOME`, else `%USERPROFILE%` on Windows where
/// `HOME` is usually unset.
pub fn home_dir() -> PathBuf {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .expect("neither HOME nor USERPROFILE is set");
    PathBuf::from(home)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_worktree_lives_under_its_root() {
        assert_eq!(
            worktree_dir(Path::new("/w"), "syro", "my-feature"),
            Ok(PathBuf::from("/w/syro/my-feature")),
        );
    }

    #[test]
    fn a_name_is_trimmed_once() {
        assert_eq!(
            worktree_dir(Path::new("/w"), "syro", "  spaced  "),
            Ok(PathBuf::from("/w/syro/spaced")),
        );
        assert_eq!(worktree_name("  spaced  "), Ok("spaced"));
    }

    #[test]
    fn a_name_that_is_a_path_is_refused() {
        for (bad, expected) in [
            ("", WorktreeNameError::Empty),
            ("   ", WorktreeNameError::Empty),
            (".", WorktreeNameError::NotOneDirectory),
            ("..", WorktreeNameError::NotOneDirectory),
            ("a/b", WorktreeNameError::NotOneDirectory),
            ("a\\b", WorktreeNameError::NotOneDirectory),
            ("///", WorktreeNameError::NotOneDirectory),
            ("---", WorktreeNameError::NoAlphanumeric),
        ] {
            assert_eq!(
                worktree_dir(Path::new("/w"), "syro", bad),
                Err(expected),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn a_layout_that_leaves_the_root_is_refused() {
        assert_eq!(
            worktree_dir(Path::new("/w"), "/tmp", "x"),
            Err(WorktreeNameError::OutsideRoot),
        );
        assert_eq!(
            worktree_dir(Path::new("/w"), "", "x"),
            Ok(PathBuf::from("/w/x")),
        );
    }
}
