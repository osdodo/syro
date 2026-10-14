//! `[[hook]]` entries and a repo's `syro.yaml` both become a shell command per event.

use std::collections::BTreeMap;
use std::process::Command;

use crate::event::CoreEvent;
use crate::model::{HookSpec, Worktree};

pub fn matches(hook: &HookSpec, event: &CoreEvent) -> bool {
    hook.on == event.name()
}

/// The variables a hook process gets; one the event does not carry is omitted.
pub fn environment(event: &CoreEvent, worktree: Option<&Worktree>) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    if let Some(wt) = worktree {
        env.insert(
            "SYRO_WORKTREE_PATH".to_string(),
            wt.path.to_string_lossy().into_owned(),
        );
        env.insert("SYRO_WORKSPACE_NAME".to_string(), wt.name.clone());
    }
    if let CoreEvent::AgentStatusChanged {
        agent: Some(agent), ..
    } = event
    {
        env.insert("SYRO_AGENT_ID".to_string(), agent.clone());
    }
    // ponytail: only `RepoAdded` carries the repo path, so other events get no repo env.
    if let CoreEvent::RepoAdded { repo } = event {
        env.insert(
            "SYRO_ROOT_PATH".to_string(),
            repo.path.to_string_lossy().into_owned(),
        );
    }
    env
}

/// Spawn `sh -c <hook.run>` with that env, in the worktree dir when given, then detach.
pub fn run(hook: &HookSpec, event: &CoreEvent, worktree: Option<&Worktree>) -> anyhow::Result<()> {
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg(&hook.run)
        .envs(environment(event, worktree));
    if let Some(wt) = worktree {
        cmd.current_dir(&wt.path);
    }
    // ponytail: a `terminal = true` hook still runs detached here; dropping the Child detaches it.
    cmd.spawn()?;
    Ok(())
}

/// `syro.yaml`'s `scripts.setup` / `scripts.archive`, through a hand-rolled block-scalar parser
/// so core needs no yaml dependency.
pub fn from_syro_yaml(contents: &str) -> Vec<HookSpec> {
    let lines: Vec<&str> = contents.lines().collect();
    let mut hooks = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim_start();
        let indent = line.len() - trimmed.len();
        let (key, on) = if trimmed.starts_with("setup:") {
            ("setup", "worktree_created")
        } else if trimmed.starts_with("archive:") {
            ("archive", "worktree_removed")
        } else {
            i += 1;
            continue;
        };

        let rest = trimmed[key.len() + 1..].trim();
        if rest.starts_with('|') || rest.starts_with('>') {
            // literal/folded block scalar: body is the more-indented lines.
            let mut body = Vec::new();
            let mut j = i + 1;
            let mut strip = None;
            while j < lines.len() {
                let b = lines[j];
                let bt = b.trim_start();
                let bi = b.len() - bt.len();
                if bt.is_empty() {
                    body.push(String::new());
                    j += 1;
                    continue;
                }
                if bi <= indent {
                    break;
                }
                let s = *strip.get_or_insert(bi);
                body.push(b.get(s.min(b.len())..).unwrap_or("").to_string());
                j += 1;
            }
            let run = body.join("\n").trim_end().to_string();
            if !run.is_empty() {
                hooks.push(HookSpec {
                    on: on.to_string(),
                    run,
                    terminal: false,
                });
            }
            i = j;
        } else {
            if !rest.is_empty() {
                hooks.push(HookSpec {
                    on: on.to_string(),
                    run: rest.to_string(),
                    terminal: false,
                });
            }
            i += 1;
        }
    }
    hooks
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AgentStatus, Repo, Worktree};

    fn worktree() -> Worktree {
        Worktree {
            id: "wt-1".into(),
            repo_id: "repo-1".into(),
            name: "my-feature".into(),
            path: "/tmp/wt/my-feature".into(),
            branch: "my-feature".into(),
            base_ref: "origin/main".into(),
        }
    }

    #[test]
    fn matches_by_event_name() {
        let hook = HookSpec {
            on: "worktree_created".into(),
            run: "true".into(),
            terminal: false,
        };
        let wt = worktree();
        assert!(matches(
            &hook,
            &CoreEvent::WorktreeCreated {
                worktree: wt.clone()
            }
        ));
        assert!(!matches(
            &hook,
            &CoreEvent::WorktreeRemoved { worktree: wt }
        ));
    }

    #[test]
    fn env_has_worktree_path_and_name() {
        let wt = worktree();
        let env = environment(
            &CoreEvent::WorktreeCreated {
                worktree: wt.clone(),
            },
            Some(&wt),
        );
        assert_eq!(
            env.get("SYRO_WORKTREE_PATH").map(String::as_str),
            Some("/tmp/wt/my-feature")
        );
        assert_eq!(
            env.get("SYRO_WORKSPACE_NAME").map(String::as_str),
            Some("my-feature")
        );
        assert!(env.get("SYRO_AGENT_ID").is_none());
    }

    #[test]
    fn env_agent_id_and_root_path() {
        let env = environment(
            &CoreEvent::AgentStatusChanged {
                worktree_id: "wt-1".into(),
                terminal_id: "t-1".into(),
                agent: Some("claude".into()),
                status: Some(AgentStatus::Working),
            },
            None,
        );
        assert_eq!(env.get("SYRO_AGENT_ID").map(String::as_str), Some("claude"));

        let repo = Repo {
            id: "repo-1".into(),
            name: "syro".into(),
            path: "/repos/syro".into(),
            shared_paths: Vec::new(),
        };
        let env = environment(&CoreEvent::RepoAdded { repo }, None);
        assert_eq!(
            env.get("SYRO_ROOT_PATH").map(String::as_str),
            Some("/repos/syro")
        );
    }

    #[test]
    fn syro_yaml_maps_setup_and_archive() {
        let yaml = "scripts:\n  setup: |\n    node config/scripts/run-internal-dev-setup.mjs\n    pnpm install\n  archive: |\n    rm -rf node_modules\n";
        let hooks = from_syro_yaml(yaml);
        assert_eq!(hooks.len(), 2);
        assert_eq!(hooks[0].on, "worktree_created");
        assert!(hooks[0].run.contains("pnpm install"));
        assert!(!hooks[0].terminal);
        assert_eq!(hooks[1].on, "worktree_removed");
        assert!(hooks[1].run.contains("rm -rf node_modules"));
    }

    #[test]
    fn syro_yaml_inline_setup() {
        let hooks = from_syro_yaml("scripts:\n  setup: pnpm install\n");
        assert_eq!(hooks.len(), 1);
        assert_eq!(hooks[0].on, "worktree_created");
        assert_eq!(hooks[0].run, "pnpm install");
    }
}
