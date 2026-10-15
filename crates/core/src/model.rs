//! Domain types shared across core and the UI: plain `serde` data, no I/O.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub type RepoId = String;
pub type WorktreeId = String;
pub type TerminalId = String;
pub type AgentId = String;

/// A local git repository registered in the sidebar.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Repo {
    pub id: RepoId,
    pub name: String,
    pub path: PathBuf,
    /// Paths, relative to the checkout, brought into every new worktree from the
    /// main one (see `git::share_paths`): what git does not track and a worktree
    /// still needs, like `.env`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub shared_paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Worktree {
    pub id: WorktreeId,
    pub repo_id: RepoId,
    pub name: String,
    pub path: PathBuf,
    pub branch: String,
    pub base_ref: String,
}

/// Orders by urgency: `Permission > Working > Idle`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    Idle,
    Working,
    Permission,
}

/// One title→status rule. First match wins, top to bottom; no match = not an agent.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentStatusRule {
    /// Regex tested against the OSC title.
    #[serde(rename = "match")]
    pub pattern: String,
    /// The title must also name this agent, as a whole token — so `~/codex-scratch` does not match.
    /// `None` leaves the rule ungated.
    #[serde(default)]
    pub requires: Option<String>,
    pub state: AgentStatus,
}

/// A runnable agent preset. Builtin presets are merged with user config by `id`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentSpec {
    pub id: AgentId,
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// argv that re-enters a session: `{id}` is the session id, `{path}` its transcript file.
    #[serde(default)]
    pub resume_args: Vec<String>,
    /// argv that starts a new session under an id syro picks (`{id}`), so the terminal can find
    /// its own transcript.
    #[serde(default)]
    pub new_args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub status: Vec<AgentStatusRule>,
}

/// A lifecycle hook: an event name → a shell command.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HookSpec {
    pub on: String,
    pub run: String,
    /// Run in a visible terminal instead of a silent background shell.
    #[serde(default)]
    pub terminal: bool,
}

/// What a tab holds; rebuilt into a live pane on startup (no terminal content).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TabSpec {
    Terminal {
        cwd: PathBuf,
        agent: Option<AgentId>,
        /// The agent's own session, so a restart re-enters the conversation instead of
        /// starting a new one. `None` is a plain shell, an agent that writes no transcript
        /// this app can find, or a session that never wrote one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session: Option<String>,
    },
    File {
        path: PathBuf,
    },
    Diff {
        worktree_id: WorktreeId,
    },
}

/// Which way a split divides. In core because it is persisted with the tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SplitAxis {
    SideBySide,
    Stacked,
}

/// One tab as it persists: a pane spec, or two divided. Untagged, so a flat `state.json` from
/// before splits still parses.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum TabLayout {
    Pane(TabSpec),
    Split {
        axis: SplitAxis,
        first: Box<TabLayout>,
        second: Box<TabLayout>,
    },
}

impl TabLayout {
    /// Every pane spec in the tab, in draw order — what restore builds.
    pub fn specs(&self) -> Vec<&TabSpec> {
        match self {
            TabLayout::Pane(spec) => vec![spec],
            TabLayout::Split { first, second, .. } => {
                let mut specs = first.specs();
                specs.extend(second.specs());
                specs
            }
        }
    }
}

/// Persistent machine state: what to restore on startup.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct State {
    #[serde(default)]
    pub repos: Vec<Repo>,
    #[serde(default)]
    pub worktrees: Vec<Worktree>,
    /// One entry per tab-bar row, in bar order; a split lives inside its tab.
    #[serde(default)]
    pub tabs: Vec<TabLayout>,
    #[serde(default)]
    pub active_worktree: Option<WorktreeId>,
    /// Sidebar width as last dragged; `None` is the default width.
    #[serde(default)]
    pub sidebar_width: Option<f32>,
    /// The right sidebar's width — one strip, so one width for all three panels.
    #[serde(default)]
    pub right_width: Option<f32>,
    /// Repositories taken out of the sidebar, by id: still in the workspace, just
    /// not on screen. Kept, because a project hidden today is still hidden
    /// tomorrow — and the sidebar says how many are hidden so they can come back.
    #[serde(default)]
    pub hidden_repos: Vec<RepoId>,
}
