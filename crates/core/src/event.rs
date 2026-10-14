//! The one event definition the UI and the hooks share (gpui `EventEmitter` and `[[hook]]`).

use serde::{Deserialize, Serialize};

use crate::model::{AgentId, AgentStatus, Repo, TerminalId, Worktree, WorktreeId};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CoreEvent {
    RepoAdded {
        repo: Repo,
    },
    RepoRemoved {
        repo_id: String,
    },
    WorktreeCreated {
        worktree: Worktree,
    },
    WorktreeRemoved {
        worktree: Worktree,
    },
    ActiveWorktreeChanged {
        worktree_id: Option<WorktreeId>,
    },
    AgentStatusChanged {
        worktree_id: WorktreeId,
        terminal_id: TerminalId,
        agent: Option<AgentId>,
        status: Option<AgentStatus>,
    },
}

impl CoreEvent {
    /// Stable machine-readable name, used by `[[hook]].on` matching.
    pub fn name(&self) -> &'static str {
        match self {
            CoreEvent::RepoAdded { .. } => "repo_added",
            CoreEvent::RepoRemoved { .. } => "repo_removed",
            CoreEvent::WorktreeCreated { .. } => "worktree_created",
            CoreEvent::WorktreeRemoved { .. } => "worktree_removed",
            CoreEvent::ActiveWorktreeChanged { .. } => "active_worktree_changed",
            CoreEvent::AgentStatusChanged { .. } => "agent_status_changed",
        }
    }
}
