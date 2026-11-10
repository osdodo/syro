//! `state.json` persistence: read on startup, atomic write on every change.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Context;

use crate::model::State;

/// `state.json` on disk. Reads are lenient, writes are atomic.
pub struct Store {
    path: PathBuf,
}

impl Store {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// A missing file is the default; corrupt JSON is an error.
    pub fn load(&self) -> anyhow::Result<State> {
        match fs::read_to_string(&self.path) {
            Ok(json) => serde_json::from_str(&json)
                .with_context(|| format!("corrupt state file: {}", self.path.display())),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(State::default()),
            Err(err) => Err(err).with_context(|| format!("reading {}", self.path.display())),
        }
    }

    /// Write `state` atomically (temp file + rename).
    pub fn save(&self, state: &State) -> anyhow::Result<()> {
        let json = serde_json::to_string_pretty(state)?;
        if let Some(parent) = self.path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        let tmp = temp_sibling(&self.path);
        fs::write(&tmp, json).with_context(|| format!("writing {}", tmp.display()))?;
        // ponytail: no fsync; a crash loses state.json, never corrupts it.
        fs::rename(&tmp, &self.path)
            .with_context(|| format!("renaming {} -> {}", tmp.display(), self.path.display()))?;
        Ok(())
    }
}

impl Default for Store {
    fn default() -> Self {
        Self::new(crate::state_path())
    }
}

/// `state.json.tmp-<pid>`; a pid cannot repeat on one host.
fn temp_sibling(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "state.json".to_string());
    path.with_file_name(format!("{file_name}.tmp-{}", std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Repo, TabLayout, TabSpec};

    fn test_store() -> (Store, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "syro-store-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        (Store::new(dir.join("state.json")), dir)
    }

    #[test]
    fn load_missing_file_returns_default() {
        let (store, dir) = test_store();
        let state = store.load().unwrap();
        assert_eq!(state, State::default());
        assert!(state.repos.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_flat_tab_list_from_before_splits_still_reads() {
        let (store, dir) = test_store();
        std::fs::write(
            &store.path,
            r#"{"repos":[],"tabs":[{"kind":"file","path":"/w/a.md"}],"active_worktree":null}"#,
        )
        .unwrap();
        let state = store.load().unwrap();
        assert_eq!(
            state.tabs,
            vec![TabLayout::Pane(TabSpec::File {
                path: "/w/a.md".into()
            })]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_then_load_roundtrips() {
        let (store, dir) = test_store();
        let mut state = State::default();
        state.repos.push(Repo {
            id: "r1".into(),
            name: "syro".into(),
            path: "/tmp/syro".into(),
            shared_paths: Vec::new(),
        });
        state.tabs.push(TabLayout::Split {
            axis: crate::model::SplitAxis::Stacked,
            first: Box::new(TabLayout::Pane(TabSpec::File {
                path: "/tmp/syro/src/main.rs".into(),
            })),
            second: Box::new(TabLayout::Pane(TabSpec::File {
                path: "/tmp/syro/README.md".into(),
            })),
        });
        store.save(&state).unwrap();
        let loaded = store.load().unwrap();
        assert_eq!(loaded, state);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_creates_parent_dir() {
        let dir = std::env::temp_dir().join(format!(
            "syro-store-nested-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let store = Store::new(dir.join("a").join("b").join("state.json"));
        store.save(&State::default()).unwrap();
        assert!(dir.join("a/b/state.json").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_file_is_error() {
        let (store, dir) = test_store();
        std::fs::write(&store.path, "not json").unwrap();
        assert!(store.load().is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
