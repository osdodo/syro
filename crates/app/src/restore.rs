//! What the last session's tabs come back as, and which ones cannot.

use std::path::Path;

use rust_i18n::t;
use syro_core::model::{TabLayout, TabSpec, Worktree, WorktreeId};

/// A spec that belonged to the worktree being restored and cannot come back.
#[derive(Debug, Clone, PartialEq)]
pub struct Dropped {
    pub spec: TabSpec,
    pub reason: String,
}

/// What a restart does with the saved tabs.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Plan {
    /// The active worktree's tabs, in their saved order, each with the shape it
    /// was saved in — a tab that was split comes back split.
    pub restore: Vec<TabLayout>,
    /// The active worktree's panes that cannot be rebuilt, with why.
    pub dropped: Vec<Dropped>,
    /// Everything that is not this worktree's, left as it was saved. A pane of a
    /// split tab that belongs elsewhere is carried on its own: the worktree it
    /// belongs to gets it back, one pane to a tab.
    pub carried: Vec<TabLayout>,
}

/// Which worktree a saved spec belongs to.
pub fn worktree_of(spec: &TabSpec, worktrees: &[Worktree]) -> Option<WorktreeId> {
    let path: &Path = match spec {
        TabSpec::Terminal { cwd, .. } => cwd,
        TabSpec::File { path } => path,
        TabSpec::Diff { worktree_id } => return Some(worktree_id.clone()),
    };
    // Both sides resolved before they are compared.
    let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());

    let mut best: Option<(usize, WorktreeId)> = None;
    for worktree in worktrees {
        let root = worktree
            .path
            .canonicalize()
            .unwrap_or_else(|_| worktree.path.clone());
        if !path.starts_with(&root) {
            continue;
        }
        let depth = root.as_os_str().len();
        if best.as_ref().is_none_or(|(best, _)| depth > *best) {
            best = Some((depth, worktree.id.clone()));
        }
    }
    best.map(|(_, id)| id)
}

/// What to call a spec in a sentence shown to the user: the file, the
/// directory, or the thing it is, in the words the tab bar would have used.
pub fn describe(spec: &TabSpec) -> String {
    match spec {
        TabSpec::Terminal { cwd, .. } => {
            t!("restore.terminal_in", path = cwd.display()).to_string()
        }
        TabSpec::File { path } => path.display().to_string(),
        TabSpec::Diff { .. } => t!("restore.diff_tab").to_string(),
    }
}

/// Why this spec cannot be rebuilt, if it cannot.
fn unbuildable(spec: &TabSpec) -> Option<String> {
    match spec {
        TabSpec::Terminal { cwd, .. } if !cwd.is_dir() => {
            Some(t!("workspace.gone", path = cwd.display()).to_string())
        }
        TabSpec::File { path } if !path.is_file() => {
            Some(t!("workspace.gone", path = path.display()).to_string())
        }
        // A diff names a worktree, and the worktree being restored is one that
        // exists — it is the active one.
        TabSpec::Terminal { .. } | TabSpec::File { .. } | TabSpec::Diff { .. } => None,
    }
}

/// What to do with `tabs` when `active` is the worktree on screen.
pub fn plan(tabs: &[TabLayout], worktrees: &[Worktree], active: &WorktreeId) -> Plan {
    let mut plan = Plan::default();
    for tab in tabs {
        let mine = tab
            .specs()
            .iter()
            .any(|spec| worktree_of(spec, worktrees).as_ref() == Some(active));
        if !mine {
            plan.carried.push(tab.clone());
            continue;
        }
        let mut carried = Vec::new();
        let mut dropped = Vec::new();
        let pruned = prune(tab, worktrees, active, &mut carried, &mut dropped);
        plan.carried.extend(carried);
        plan.dropped.extend(dropped);
        if let Some(tab) = pruned {
            plan.restore.push(tab);
        }
    }
    plan
}

/// The part of one tab that belongs to `active` and can still be rebuilt.
fn prune(
    tab: &TabLayout,
    worktrees: &[Worktree],
    active: &WorktreeId,
    carried: &mut Vec<TabLayout>,
    dropped: &mut Vec<Dropped>,
) -> Option<TabLayout> {
    match tab {
        TabLayout::Pane(spec) => {
            if worktree_of(spec, worktrees).as_ref() != Some(active) {
                carried.push(TabLayout::Pane(spec.clone()));
                return None;
            }
            match unbuildable(spec) {
                Some(reason) => {
                    dropped.push(Dropped {
                        spec: spec.clone(),
                        reason,
                    });
                    None
                }
                None => Some(TabLayout::Pane(spec.clone())),
            }
        }
        TabLayout::Split {
            axis,
            first,
            second,
        } => {
            let first = prune(first, worktrees, active, carried, dropped);
            let second = prune(second, worktrees, active, carried, dropped);
            match (first, second) {
                (Some(first), Some(second)) => Some(TabLayout::Split {
                    axis: *axis,
                    first: Box::new(first),
                    second: Box::new(second),
                }),
                // One half gone: the half that stayed is the tab.
                (Some(one), None) | (None, Some(one)) => Some(one),
                (None, None) => None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn worktree(id: &str, path: &str) -> Worktree {
        Worktree {
            id: id.to_string(),
            repo_id: "r".to_string(),
            name: id.to_string(),
            path: PathBuf::from(path),
            branch: id.to_string(),
            base_ref: "main".to_string(),
        }
    }

    fn terminal(cwd: &str) -> TabSpec {
        TabSpec::Terminal {
            cwd: PathBuf::from(cwd),
            agent: None,
            session: None,
        }
    }

    fn file(path: &str) -> TabSpec {
        TabSpec::File {
            path: PathBuf::from(path),
        }
    }

    /// Two worktrees, one nested under the workspaces root the way this app
    /// creates them: `/w/demo/main` and `/w/demo/feature`.
    fn worktrees() -> Vec<Worktree> {
        vec![
            worktree("main", "/w/demo/main"),
            worktree("feature", "/w/demo/feature"),
        ]
    }

    /// Saved tabs as the file holds them: one pane each, which is what a
    /// `state.json` written before tabs could be split has.
    fn tabs(specs: &[TabSpec]) -> Vec<TabLayout> {
        specs.iter().cloned().map(TabLayout::Pane).collect()
    }

    /// The specs of a restored tab, for an assertion about *what* came back.
    fn restored(plan: &Plan) -> Vec<TabSpec> {
        plan.restore
            .iter()
            .flat_map(|tab| tab.specs().into_iter().cloned())
            .collect()
    }

    #[test]
    fn a_path_places_a_tab_in_the_worktree_that_holds_it() {
        let worktrees = worktrees();
        assert_eq!(
            worktree_of(&terminal("/w/demo/main"), &worktrees),
            Some("main".to_string())
        );
        assert_eq!(
            worktree_of(&file("/w/demo/feature/src/lib.rs"), &worktrees),
            Some("feature".to_string())
        );
        // The deepest match: a worktree nested inside another's directory
        // belongs to the inner one.
        let nested = vec![
            worktree("outer", "/w/demo"),
            worktree("inner", "/w/demo/inner"),
        ];
        assert_eq!(
            worktree_of(&file("/w/demo/inner/src/lib.rs"), &nested),
            Some("inner".to_string())
        );
        assert_eq!(
            worktree_of(&file("/w/demo/src/lib.rs"), &nested),
            Some("outer".to_string())
        );
    }

    #[test]
    fn a_path_outside_every_worktree_belongs_to_none() {
        let worktrees = worktrees();
        assert_eq!(worktree_of(&file("/elsewhere/note.md"), &worktrees), None);
        // A prefix that is not a path boundary: `/w/demo/main-notes` is not in
        // `/w/demo/main`.
        assert_eq!(
            worktree_of(&file("/w/demo/main-notes/a.md"), &worktrees),
            None
        );
    }

    /// A directory of this test's own, so a full-suite run in parallel cannot
    /// have two tests writing the same file. Every test that touches the disk
    /// gets its own.
    fn scratch_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "syro-restore-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_spec_under_a_symlink_still_places_in_its_worktree() {
        // Two spellings of one directory: a spec holding either one belongs to
        // the same worktree. (`/tmp` for `/private/tmp` is the same story on
        // macOS, and building the link here does not depend on that.)
        let base = scratch_dir("symlink");
        let link = base.join("link");
        std::os::unix::fs::symlink(&base, &link).unwrap();

        let worktrees = vec![worktree("main", &base.to_string_lossy())];
        let spec = file(&format!("{}/a.md", link.display()));
        // A path that is gone cannot be resolved and is dropped for that reason
        // instead, which `a_file_that_is_gone_is_dropped_with_its_reason` covers.
        std::fs::write(base.join("a.md"), "x").unwrap();
        assert_eq!(
            worktree_of(&spec, &worktrees),
            Some("main".to_string()),
            "the symlinked spelling of the worktree's own path"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_diff_names_its_worktree() {
        let worktrees = worktrees();
        assert_eq!(
            worktree_of(
                &TabSpec::Diff {
                    worktree_id: "feature".to_string()
                },
                &worktrees
            ),
            Some("feature".to_string())
        );
    }

    #[test]
    fn the_active_worktrees_tabs_come_back_in_their_saved_order() {
        let dir = scratch_dir("order");
        let here = dir.to_string_lossy().into_owned();
        let specs = vec![
            file(&format!("{here}/a.md")),
            TabSpec::Diff {
                worktree_id: "main".to_string(),
            },
            terminal(&here),
        ];
        std::fs::write(dir.join("a.md"), "x").unwrap();

        let plan = plan(
            &tabs(&specs),
            &[worktree("main", &here)],
            &"main".to_string(),
        );
        assert_eq!(restored(&plan), specs);
        assert!(plan.dropped.is_empty(), "{:?}", plan.dropped);
        assert!(plan.carried.is_empty(), "{:?}", plan.carried);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn another_worktrees_tabs_are_carried_not_restored() {
        let specs = vec![terminal("/w/demo/feature"), file("/w/demo/feature/a.md")];
        let plan = plan(&tabs(&specs), &worktrees(), &"main".to_string());
        assert!(plan.restore.is_empty());
        assert!(plan.dropped.is_empty());
        assert_eq!(
            plan.carried,
            tabs(&specs),
            "they stay saved for that worktree"
        );
    }

    #[test]
    fn a_file_that_is_gone_is_dropped_with_its_reason() {
        let gone = file("/w/demo/main/gone.md");
        let specs = vec![
            gone.clone(),
            TabSpec::Diff {
                worktree_id: "main".to_string(),
            },
        ];
        let plan = plan(&tabs(&specs), &worktrees(), &"main".to_string());
        assert_eq!(
            restored(&plan),
            vec![TabSpec::Diff {
                worktree_id: "main".to_string()
            }],
            "the rest of the list still comes back"
        );
        assert_eq!(plan.dropped.len(), 1);
        assert_eq!(plan.dropped[0].spec, gone);
        assert!(
            plan.dropped[0].reason.contains("gone"),
            "{:?}",
            plan.dropped
        );
    }

    #[test]
    fn a_terminal_whose_directory_is_gone_is_dropped() {
        let specs = vec![terminal("/w/demo/main")];
        let plan = plan(&tabs(&specs), &worktrees(), &"main".to_string());
        assert!(plan.restore.is_empty());
        assert_eq!(plan.dropped.len(), 1);
        assert!(plan.dropped[0].reason.contains("/w/demo/main"));
    }

    #[test]
    fn nothing_saved_is_nothing_to_do() {
        let plan = plan(&tabs(&[]), &worktrees(), &"main".to_string());
        assert_eq!(plan, Plan::default());
    }

    #[test]
    fn a_spec_is_described_in_the_words_the_window_would_use() {
        assert_eq!(describe(&file("/w/a.md")), "/w/a.md");
        assert_eq!(describe(&terminal("/w/demo")), "terminal in /w/demo");
        assert_eq!(
            describe(&TabSpec::Diff {
                worktree_id: "main".to_string()
            }),
            "the diff tab"
        );
    }
}
