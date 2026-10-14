//! The single source of truth: repos, their worktrees, the active one, and the
//! tab list carried across a restart.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use gpui_kit::*;
use rust_i18n::t;
use syro_core::agent;
use syro_core::config::{ComputerAwakeMode, Settings};
use syro_core::event::CoreEvent;
use syro_core::git::{self, FileChange, GitWorktree};
use syro_core::hooks;
use syro_core::model::{
    AgentId, AgentSpec, AgentStatus, HookSpec, Repo, RepoId, State, TabLayout, TerminalId,
    Worktree, WorktreeId,
};
use syro_core::store::Store;
use syro_core::{WorktreeNameError, worktree_dir, worktree_name};

use crate::awake::AwakeAssertion;

/// `repo::Add` — register a repository. `path` is the directory to add; `None`
/// asks the platform's picker for one first.
pub mod repo {
    use super::*;

    #[derive(Clone, PartialEq, Debug, Action)]
    #[action(namespace = repo, no_json)]
    pub struct Add {
        pub path: Option<PathBuf>,
    }

    /// Ask for a new project: the sidebar's `+` and the palette open the form — browse, clone from a URL, or start an empty repository.
    #[derive(Clone, PartialEq, Debug, Default, Action)]
    #[action(namespace = repo, no_json)]
    pub struct AskAdd;

    /// Edit the paths every new worktree gets from the main checkout: the
    /// project menu's entry opens the form.
    #[derive(Clone, PartialEq, Debug, Action)]
    #[action(namespace = repo, no_json)]
    pub struct EditSharedPaths {
        pub repo_id: RepoId,
    }

    /// Take the repository out of the workspace.
    #[derive(Clone, PartialEq, Debug, Action)]
    #[action(namespace = repo, no_json)]
    pub struct Remove {
        pub repo_id: RepoId,
    }
}

/// `worktree::*` — everything the sidebar does to a worktree.
pub mod worktree {
    use super::*;

    /// Create a worktree of `repo_id` named `name`. `base` defaults to the
    /// repo's `origin/HEAD`, resolved when the call runs.
    #[derive(Clone, PartialEq, Debug, Action)]
    #[action(namespace = worktree, no_json)]
    pub struct Create {
        pub repo_id: RepoId,
        pub name: String,
        pub base: Option<String>,
    }

    /// Ask for a new worktree: the sidebar's `+` opens the form.
    #[derive(Clone, PartialEq, Debug, Action)]
    #[action(namespace = worktree, no_json)]
    pub struct New {
        pub repo_id: RepoId,
    }

    /// Ask to remove a worktree: the row's `×` opens the confirmation, which is
    /// what offers the forced retry. [`Remove`] is what actually removes.
    #[derive(Clone, PartialEq, Debug, Action)]
    #[action(namespace = worktree, no_json)]
    pub struct AskRemove {
        pub worktree_id: WorktreeId,
    }

    /// Remove a worktree and its branch. `force` is the retry the UI offers
    /// after an unforced removal was refused.
    #[derive(Clone, PartialEq, Debug, Action)]
    #[action(namespace = worktree, no_json)]
    pub struct Remove {
        pub worktree_id: WorktreeId,
        pub force: bool,
    }

    /// Make a worktree the one the workspace shows.
    #[derive(Clone, PartialEq, Debug, Action)]
    #[action(namespace = worktree, no_json)]
    pub struct Activate {
        pub worktree_id: WorktreeId,
    }
}

/// `terminal::*` — what a worktree's terminal is running.
pub mod terminal {
    use super::*;

    /// Start (or restart) the worktree's terminal, running one of the configured agents in it when
    /// `agent` names one.
    #[derive(Clone, PartialEq, Debug, Action)]
    #[action(namespace = terminal, no_json)]
    pub struct New {
        pub worktree_id: WorktreeId,
        pub agent: Option<AgentId>,
        /// The past session to re-enter, when this terminal is a resume rather
        /// than a fresh start. `None` is every other way a terminal is opened.
        pub resume: Option<crate::pane::terminal::SessionRef>,
    }

    /// A new shell tab in the worktree on screen. The tab bar's `+` and `cmd-t`
    /// both mean this, and neither knows which worktree that is — which is what
    /// makes this a field-less action rather than a [`New`] one.
    #[derive(Clone, PartialEq, Debug, Default, Action)]
    #[action(namespace = terminal, no_json)]
    pub struct Add;

    /// Restart the terminal at `index` in place: the tab is the one the restart
    /// button belongs to, and it keeps its place in its tab and its half of the
    /// split.
    #[derive(Clone, PartialEq, Debug, Action)]
    #[action(namespace = terminal, no_json)]
    pub struct Restart {
        pub worktree_id: WorktreeId,
        /// The pane whose child went away, by content id — not its position:
        /// that is where the new child goes back.
        pub pane: crate::split::Pane,
    }
}

/// `editor::*` — what an open file can be asked to do.
pub mod editor {
    use super::*;

    #[derive(Clone, PartialEq, Debug, Action)]
    #[action(namespace = editor, no_json)]
    pub struct Open {
        pub path: PathBuf,
    }

    #[derive(Clone, PartialEq, Debug, Default, Action)]
    #[action(namespace = editor, no_json)]
    pub struct Save;
}

/// `tab::*` — the tab bar's two clicks. `index` is a position in the active
/// worktree's tab list, which is what the bar drew.
pub mod tab {
    use super::*;

    #[derive(Clone, PartialEq, Debug, Action)]
    #[action(namespace = tab, no_json)]
    pub struct Activate {
        pub worktree_id: WorktreeId,
        pub index: usize,
    }

    #[derive(Clone, PartialEq, Debug, Action)]
    #[action(namespace = tab, no_json)]
    pub struct Close {
        pub worktree_id: WorktreeId,
        pub index: usize,
    }
}

/// `diff::*` — the active worktree's changes, as a tab.
pub mod diff {
    use super::*;

    #[derive(Clone, PartialEq, Debug, Default, Action)]
    #[action(namespace = diff, no_json)]
    pub struct Open;

    /// Show one changed file's diff. Dispatched by the source control panel's
    /// rows — the file list lives there, and this is what a row's click means —
    /// and handled by the workspace, which owns the diff tab.
    #[derive(Clone, PartialEq, Debug, Action)]
    #[action(namespace = diff, no_json)]
    pub struct OpenFile {
        /// The path git reported, relative to the worktree.
        pub path: String,
    }

    /// Show one file as the commit named changed it. Dispatched by the rows
    /// under a commit — in the source control panel, and in the graph — and
    /// handled by the workspace, which owns the diff tab. A separate action from
    /// [`OpenFile`] because it is the other comparison: that commit's change to
    /// that file, whatever the worktree has done since.
    #[derive(Clone, PartialEq, Debug, Action)]
    #[action(namespace = diff, no_json)]
    pub struct OpenCommitFile {
        /// The commit, by id.
        pub commit: String,
        /// The path git reported, relative to the worktree.
        pub path: String,
    }
}

/// `gitgraph::*` — the full-window graph of every branch.
pub mod gitgraph {
    use super::*;

    /// Show the graph. Dispatched by the source control panel's own header, and
    /// handled by the workspace, which owns the window the graph covers.
    #[derive(Clone, PartialEq, Debug, Default, Action)]
    #[action(namespace = gitgraph, no_json)]
    pub struct Open;

    /// Take the graph down. Dispatched by the `×` in its own toolbar, and by
    /// Escape, which is the same road every other overlay takes.
    #[derive(Clone, PartialEq, Debug, Default, Action)]
    #[action(namespace = gitgraph, no_json)]
    pub struct Close;
}

/// Re-read every repo's worktrees from git. : a refresh is
/// driven by an event, never by a timer.
#[derive(Clone, PartialEq, Debug, Default, Action)]
#[action(namespace = workspace, no_json)]
pub struct Refresh;

/// One terminal's agent, as last reported by the terminal itself.
#[derive(Debug, Clone, PartialEq)]
struct TerminalAgent {
    worktree: WorktreeId,
    status: AgentStatus,
    /// The agent said it was working and has since gone quiet
    /// (`syro_core::agent::stalled`). Only the terminal sees the output, so it is
    /// the terminal that decides this and reports it along with the status.
    stalled: bool,
}

pub struct AppState {
    store: Store,
    /// Where new worktrees go: `<root>/<repo>/<name>`.
    workspaces_root: PathBuf,
    repos: Vec<Repo>,
    worktrees: Vec<Worktree>,
    /// Owned by the tab bar. Carried through a restart, not interpreted here —
    /// and each entry is a whole tab, split tabs included.
    tabs: Vec<TabLayout>,
    active: Option<WorktreeId>,
    /// The user's settings: agents and hooks are configuration, not code. `with_settings` supplies
    /// them, and the default is an app with no settings file yet.
    settings: Settings,
    /// Where those settings are written back to. Held rather than read from
    /// `syro_core::settings_path()` at the write, so a test that changes a
    /// setting writes to a scratch file instead of the developer's own.
    settings_path: PathBuf,
    /// Live agent status: terminal id → what its agent is doing.
    statuses: BTreeMap<TerminalId, TerminalAgent>,
    /// Each worktree's changed files, relative to its base ref.
    changes: BTreeMap<WorktreeId, Vec<FileChange>>,
    /// Why a worktree's change list could not be read, for the diff tab to say.
    changes_error: BTreeMap<WorktreeId, String>,
    /// Git's complaint about a repo, for the sidebar to show.
    errors: BTreeMap<RepoId, String>,
    /// The repositories taken out of the sidebar (see [`Self::set_repo_hidden`]).
    hidden_repos: Vec<RepoId>,
    /// A git operation running for a repo, labelled for the sidebar.
    pending: BTreeMap<RepoId, String>,
    /// A `state.json` that could not be read or written. Not per repo, because
    /// the file is not.
    state_error: Option<String>,
    /// The last `[[hook]]` that failed and what it said.
    hook_error: Option<String>,
    /// How wide the side panels were as last dragged (the layout restore — the widths are
    /// layout, and `state.json` is where layout lives).
    sidebar_width: Option<f32>,
    right_width: Option<f32>,
    /// Keeps the machine awake while it should (see [`Self::refresh_awake`]).
    awake: AwakeAssertion,
}

impl EventEmitter<CoreEvent> for AppState {}

impl AppState {
    /// Load `store` and re-read every repo's worktrees. Worktrees are not taken
    /// from the file as truth: git is, which is what makes an external
    /// `git worktree remove` disappear on the next refresh.
    pub fn open(store: Store, workspaces_root: PathBuf, cx: &mut Context<Self>) -> Self {
        let (loaded, state_error) = match store.load() {
            Ok(state) => (state, None),
            // A corrupt file is not silently replaced: the sidebar says so and
            // the file stays where it is for the user to look at.
            Err(error) => (State::default(), Some(format!("{error:#}"))),
        };
        let mut state = Self {
            store,
            workspaces_root,
            repos: loaded.repos,
            worktrees: Vec::new(),
            tabs: loaded.tabs,
            active: loaded.active_worktree,
            settings: Settings::default(),
            settings_path: syro_core::settings_path(),
            statuses: BTreeMap::new(),
            changes: BTreeMap::new(),
            changes_error: BTreeMap::new(),
            errors: BTreeMap::new(),
            pending: BTreeMap::new(),
            state_error,
            hook_error: None,
            sidebar_width: loaded.sidebar_width,
            right_width: loaded.right_width,
            hidden_repos: loaded.hidden_repos,
            awake: AwakeAssertion::new(),
        };
        state.reload_all(cx);
        state.refresh_changes(cx);
        state.persist();
        cx.notify();
        state
    }

    /// Attach the user's settings. A method rather than an argument to `open`,
    /// because only the app has a settings file to read: every test, and any
    /// future headless mode, keeps the defaults without saying so.
    pub fn with_settings(mut self, settings: Settings) -> Self {
        self.settings = settings;
        // The setting can ask for the machine to stay awake from the first frame.
        self.refresh_awake();
        self
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    /// Where a change to the settings is written. Defaults to the user's
    /// `settings.toml`; a test that changes a setting points it at a scratch
    /// directory, for the same reason it points the store at one.
    pub fn with_settings_path(mut self, path: PathBuf) -> Self {
        self.settings_path = path;
        self
    }

    /// Replace the settings, because a user changed one in the settings form.
    pub fn set_settings(
        &mut self,
        settings: Settings,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<()> {
        let written = settings.save_to(&self.settings_path);
        self.settings = settings;
        self.refresh_awake();
        cx.notify();
        written
    }

    /// Ask the system to stay awake as `settings.computer_awake` and the running
    /// agents say: always in `on`, while an agent is working in `auto`, never in
    /// `off`. Called whenever either of those two changes.
    fn refresh_awake(&mut self) {
        let working = self
            .statuses
            .values()
            .any(|agent| agent.status == AgentStatus::Working);
        self.awake
            .set(wants_awake(self.settings.computer_awake, working));
    }

    /// Whether the app is holding the machine awake right now, for the settings
    /// form to say so.
    pub fn computer_awake_active(&self) -> bool {
        self.awake.is_active()
    }

    /// The open tabs, as they will come back after a restart: the layout is restored, terminal
    /// content is not. The bar knows which worktree each one belongs to, because that is live UI
    /// state and this file is not.
    pub fn tabs(&self) -> &[TabLayout] {
        &self.tabs
    }

    /// Replace the tab list and write it out. Called by the workspace whenever
    /// a tab opens, closes or is saved: the workspace owns the views, so it is
    /// the only thing that knows the list is stale.
    pub fn set_tabs(&mut self, tabs: Vec<TabLayout>, cx: &mut Context<Self>) {
        // Equality first: a repaint must not rewrite `state.json`.
        if self.tabs == tabs {
            return;
        }
        self.tabs = tabs;
        self.persist();
        cx.notify();
    }

    /// The preset for `id`: builtin, merged with the user's by id.
    pub fn agent(&self, id: &str) -> Option<AgentSpec> {
        // ponytail: re-resolves on every call, which is a `builtin.toml` parse
        // per terminal started. Rare enough to leave alone; cache it here when a
        // settings hot reload makes it hot.
        agent::resolve_agents(&self.settings.agents)
            .into_iter()
            .find(|spec| spec.id == id)
    }

    pub fn repos(&self) -> &[Repo] {
        &self.repos
    }

    /// Every preset the pickers offer, builtin merged with the user's by id.
    /// The order is `builtin.toml`'s, which is the order the docs list them in.
    pub fn agents(&self) -> Vec<AgentSpec> {
        agent::resolve_agents(&self.settings.agents)
    }
    /// The worktrees of one repo, in the order git reports them (the repo's own checkout first).
    pub fn worktrees(&self) -> &[Worktree] {
        &self.worktrees
    }

    pub fn worktrees_of(&self, repo_id: &str) -> impl Iterator<Item = &Worktree> {
        self.worktrees.iter().filter(move |w| w.repo_id == repo_id)
    }

    pub fn active_id(&self) -> Option<&WorktreeId> {
        self.active.as_ref()
    }

    pub fn active(&self) -> Option<&Worktree> {
        let id = self.active.as_ref()?;
        self.worktree(id)
    }

    /// One worktree by id.
    pub fn worktree(&self, id: &str) -> Option<&Worktree> {
        self.worktrees.iter().find(|worktree| worktree.id == id)
    }

    /// Whether a worktree is the repository's own checkout, which git refuses to
    /// remove. The removal dialog asks, and answers with `repo::Remove`.
    pub fn is_own_checkout(&self, worktree_id: &str) -> bool {
        let Some(worktree) = self.worktree(worktree_id) else {
            return false;
        };
        self.repos
            .iter()
            .any(|repo| repo.id == worktree.repo_id && repo.path == worktree.path)
    }

    pub fn error_for(&self, repo_id: &str) -> Option<&str> {
        self.errors.get(repo_id).map(String::as_str)
    }

    pub fn pending_for(&self, repo_id: &str) -> Option<&str> {
        self.pending.get(repo_id).map(String::as_str)
    }

    pub fn state_error(&self) -> Option<&str> {
        self.state_error.as_deref()
    }

    /// The last `[[hook]]` failure, as one line for whoever shows it.
    pub fn hook_error(&self) -> Option<&str> {
        self.hook_error.as_deref()
    }

    /// How wide the sidebar was left, or `None` for the default width.
    pub fn sidebar_width(&self) -> Option<f32> {
        self.sidebar_width
    }

    /// The repositories taken out of the sidebar.
    pub fn hidden_repos(&self) -> &[RepoId] {
        &self.hidden_repos
    }

    /// Hide one repository, or put it back. Either way it stays in the workspace:
    /// this is the list on screen, not the set of projects the app knows.
    pub fn set_repo_hidden(&mut self, repo_id: &str, hidden: bool, cx: &mut Context<Self>) {
        let listed = !self.hidden_repos.iter().any(|id| id == repo_id);
        if listed == !hidden {
            return;
        }
        if hidden {
            self.hidden_repos.push(repo_id.to_string());
        } else {
            self.hidden_repos.retain(|id| id != repo_id);
        }
        self.persist();
        cx.notify();
    }

    /// The paths every new worktree of `repo_id` gets from the main checkout (see
    /// `git::share_paths`), cleaned and without repeats.
    pub fn set_shared_paths(&mut self, repo_id: &str, paths: Vec<String>, cx: &mut Context<Self>) {
        let Some(repo) = self.repos.iter_mut().find(|repo| repo.id == repo_id) else {
            return;
        };
        let mut cleaned: Vec<String> = Vec::new();
        for path in paths.iter().filter_map(|path| git::shared_path(path)) {
            if !cleaned.contains(&path) {
                cleaned.push(path);
            }
        }
        repo.shared_paths = cleaned;
        self.persist();
        cx.notify();
    }

    /// Put every hidden repository back on screen.
    pub fn show_all_repos(&mut self, cx: &mut Context<Self>) {
        if self.hidden_repos.is_empty() {
            return;
        }
        self.hidden_repos.clear();
        self.persist();
        cx.notify();
    }

    /// The same for the right sidebar, which is the panel on the other side.
    pub fn right_width(&self) -> Option<f32> {
        self.right_width
    }

    /// Remember a panel's width. Called when a drag *ends*, not on every pixel
    /// of it: a `state.json` write per mouse move is a file write per frame.
    pub fn set_sidebar_width(&mut self, width: f32, cx: &mut Context<Self>) {
        // Equality first, the way `set_tabs` does it: a drag that ends where it
        // started must not rewrite the file.
        if self.sidebar_width == Some(width) {
            return;
        }
        self.sidebar_width = Some(width);
        self.persist();
        cx.notify();
    }

    pub fn set_right_width(&mut self, width: f32, cx: &mut Context<Self>) {
        if self.right_width == Some(width) {
            return;
        }
        self.right_width = Some(width);
        self.persist();
        cx.notify();
    }

    /// What the worktree's agents are doing, as the one status worth showing:
    /// the most urgent of its terminals. `AgentStatus` is `Ord` with
    /// `Permission` the greatest, so `max` is the answer.
    pub fn status_of(&self, worktree_id: &str) -> Option<AgentStatus> {
        self.statuses
            .values()
            .filter(|agent| agent.worktree == worktree_id)
            .map(|agent| agent.status)
            .max()
    }

    /// Whether any of the worktree's agents has gone quiet mid-task.
    pub fn stalled_of(&self, worktree_id: &str) -> bool {
        self.statuses
            .values()
            .any(|agent| agent.worktree == worktree_id && agent.stalled)
    }

    /// What one *terminal's* agent is doing, by terminal id.
    pub fn terminal_status(&self, terminal_id: &str) -> Option<AgentStatus> {
        self.statuses.get(terminal_id).map(|agent| agent.status)
    }

    /// The per-terminal answer to [`Self::stalled_of`], for the resource
    /// readout, which lists terminals rather than worktrees.
    pub fn terminal_stalled(&self, terminal_id: &str) -> bool {
        self.statuses
            .get(terminal_id)
            .is_some_and(|agent| agent.stalled)
    }

    /// The directory a worktree named `name` in `repo_id` would occupy — the same computation
    /// [`Self::create_worktree`] makes, exposed because `syro/worktree.create` has to answer
    /// with an id before git has run.
    pub fn worktree_target(&self, repo_id: &str, name: &str) -> Option<PathBuf> {
        let repo = self.repos.iter().find(|repo| repo.id == repo_id)?;
        let root = self
            .workspaces_root
            .canonicalize()
            .unwrap_or_else(|_| self.workspaces_root.clone());
        worktree_dir(&root, &repo.name, name).ok()
    }

    // --- agent status ------------------------------------------------

    /// Record what a terminal's agent is doing.
    pub fn note_agent_status(
        &mut self,
        worktree_id: &WorktreeId,
        terminal_id: &TerminalId,
        agent: Option<AgentId>,
        status: Option<AgentStatus>,
        stalled: bool,
        cx: &mut Context<Self>,
    ) {
        let previous = self.statuses.get(terminal_id).map(|agent| agent.status);
        match status {
            Some(status) => {
                self.statuses.insert(
                    terminal_id.clone(),
                    TerminalAgent {
                        worktree: worktree_id.clone(),
                        status,
                        stalled,
                    },
                );
            }
            None => {
                self.statuses.remove(terminal_id);
            }
        }

        let event = CoreEvent::AgentStatusChanged {
            worktree_id: worktree_id.clone(),
            terminal_id: terminal_id.clone(),
            agent,
            status,
        };
        // Hooks run whether or not the window is on this worktree: a hook is the
        // user's own reaction to "this agent needs me", not the UI's.
        self.run_status_hooks(&event, status, cx);
        // A transition *into* a status, not a repeat of one: the title of a
        // waiting agent is re-set by its TUI on every redraw.
        if let Some(status) = status
            && previous != Some(status)
        {
            self.announce_status(worktree_id, terminal_id, status, cx);
        }
        // An agent starting or stopping is what `auto` mode is about.
        self.refresh_awake();
        cx.emit(event);
        cx.notify();
    }

    /// Drop a worktree's statuses. They describe terminals that are about to be
    /// dropped with it, and a status for a worktree that is gone is a dot on a
    /// row that is not there.
    fn forget_statuses(&mut self, worktree_id: &str) {
        self.statuses
            .retain(|_, agent| agent.worktree != worktree_id);
        // Dropped agents are not work any more.
        self.refresh_awake();
    }

    /// Run the user's `[[hook]]`s that this status asks for, off the UI thread.
    fn run_status_hooks(
        &self,
        event: &CoreEvent,
        status: Option<AgentStatus>,
        cx: &mut Context<Self>,
    ) {
        let Some(status) = status else { return };
        let wanted: Vec<HookSpec> = self
            .settings
            .hooks
            .iter()
            .filter(|hook| hooks::matches(hook, event) || wants_status(&hook.on, status))
            .cloned()
            .collect();
        if wanted.is_empty() {
            return;
        }

        let CoreEvent::AgentStatusChanged { worktree_id, .. } = event else {
            return;
        };
        let worktree = self
            .worktrees
            .iter()
            .find(|worktree| &worktree.id == worktree_id)
            .cloned();
        let event = event.clone();
        // Off the UI thread because a hook is a command: `pnpm install` is minutes.
        Self::off_thread(
            cx,
            move || {
                let mut failure = None;
                for hook in wanted {
                    if let Err(reason) = run_hook(&hook, &event, worktree.as_ref()) {
                        tracing::warn!(command = %hook.run, %reason, "agent status hook failed");
                        // The first one is the one to show: hooks run in the
                        // order the user wrote them, and the first failure is
                        // the one that explains the rest.
                        failure.get_or_insert(reason);
                    }
                }
                failure
            },
            |state, failure, cx| {
                // A hook that works clears the message the last failure left:
                // a banner nobody can get rid of is a banner people stop
                // reading.
                if state.hook_error != failure {
                    state.hook_error = failure;
                    cx.notify();
                }
            },
        );
    }

    /// Tell the operating system that an agent is waiting for the user.
    fn announce_status(
        &self,
        worktree_id: &WorktreeId,
        terminal_id: &TerminalId,
        status: AgentStatus,
        cx: &mut Context<Self>,
    ) {
        if status != AgentStatus::Permission {
            return;
        }
        // Already on screen: the app is focused and this terminal's worktree is
        // the one being shown. Announcing what the user is looking at is noise.
        let watching =
            cx.active_window().is_some() && self.active.as_deref() == Some(worktree_id.as_str());
        if watching {
            return;
        }

        let worktree = self
            .worktrees
            .iter()
            .find(|worktree| &worktree.id == worktree_id);
        let who = worktree
            .map(|worktree| worktree.name.clone())
            .unwrap_or_else(|| t!("state.a_worktree").to_string());

        cx.show_system_notification(SystemNotification {
            // Per terminal, so a second notification for the same terminal
            // replaces the first rather than stacking up.
            tag: notification_tag(terminal_id).into(),
            title: t!("state.agent_waiting_title").to_string().into(),
            body: t!("state.agent_waiting_body", who = who).to_string().into(),
            actions: Vec::new(),
        });
    }

    // --- actions -----------------------------------------------------------

    /// `repo::Add`. `path` is validated and reduced to the repo root, so adding
    /// a subdirectory of a checkout adds the checkout.
    pub fn add_repo(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let root = match git::repo_root(&path) {
            Ok(root) => root,
            Err(error) => {
                // Not a repo at all: there is no repo to hang the error on, so
                // it goes on the window's.
                self.state_error = Some(format!("{}: {error:#}", path.display()));
                cx.notify();
                return;
            }
        };
        let existing = self
            .repos
            .iter()
            .find(|r| r.path == root)
            .map(|r| r.id.clone());
        if let Some(id) = existing {
            // Already there: activating it is what the user meant.
            let first = self.worktrees_of(&id).map(|w| w.id.clone()).next();
            if let Some(first) = first {
                self.activate(&first, cx);
            }
            return;
        }

        let name = root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| root.display().to_string());
        let repo = Repo {
            id: format!("repo:{}", root.display()),
            name,
            path: root,
            shared_paths: Vec::new(),
        };
        self.repos.push(repo.clone());
        self.errors.remove(&repo.id);
        self.refresh_repo(&repo.id, cx);
        self.activate_first_of(&repo.id, cx);
        // One of the two call sites that set `active` without going through
        // `activate`: the first repo has to arrive with its changes read.
        self.refresh_changes(cx);
        cx.emit(CoreEvent::RepoAdded { repo });
        self.persist();
        cx.notify();
    }

    /// `worktree::Activate`.
    pub fn activate(&mut self, id: &str, cx: &mut Context<Self>) {
        if self.active.as_deref() == Some(id) {
            return;
        }
        if !self.worktrees.iter().any(|w| w.id == id) {
            return;
        }
        self.active = Some(id.to_string());
        self.refresh_changes(cx);
        cx.emit(CoreEvent::ActiveWorktreeChanged {
            worktree_id: Some(id.to_string()),
        });
        self.persist();
        cx.notify();
    }

    /// `worktree::Create`. The git call runs off the UI thread; the worktree is
    /// in the sidebar when it lands, and `pending_for` labels it while it runs.
    pub fn create_worktree(
        &mut self,
        repo_id: &str,
        name: &str,
        base: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let Some(repo) = self.repos.iter().find(|r| &r.id == repo_id).cloned() else {
            return;
        };
        // Trimmed once, here: the branch, the directory and the name the sidebar
        // shows are all this one answer (core trims too, and for the same
        // reason — what the dialog previews is what git is asked for).
        let name = name.trim();
        let branch = git::branch_from_name(name);
        // Through `worktree_target`, so the directory `git worktree add` makes is the one
        // `syro/worktree.create` answers with (see that method).
        let Some(dest) = self.worktree_target(repo_id, name) else {
            if let Some(problem) = worktree_name_problem(name) {
                self.errors.insert(repo_id.to_string(), problem);
                cx.notify();
            }
            return;
        };
        if let Some(parent) = dest.parent() {
            if let Err(error) = std::fs::create_dir_all(parent) {
                self.errors
                    .insert(repo_id.to_string(), format!("{error:#}"));
                cx.notify();
                return;
            }
        }

        self.pending.insert(
            repo_id.to_string(),
            t!("state.creating", name = name).to_string(),
        );
        self.errors.remove(repo_id);
        cx.notify();

        let repo_path = repo.path.clone();
        let shared_paths = repo.shared_paths.clone();
        let create_dest = dest.clone();
        let create_branch = branch.clone();
        let repo_id = repo_id.to_string();
        let worktree_name = name.to_string();
        Self::off_thread(
            cx,
            move || -> anyhow::Result<(PathBuf, String, String)> {
                let base = match base {
                    Some(base) => base,
                    None => git::default_base_ref(&repo_path)?,
                };
                git::worktree_add(&repo_path, &create_dest, &create_branch, &base)?;
                // Before `worktree_created`, so a setup hook finds the `.env` it needs.
                // shortcut: a path that could not be brought over is logged, not shown —
                // the symlink fallback all but never fails. Surface it if it ever does.
                for failed in git::share_paths(&repo_path, &create_dest, &shared_paths) {
                    tracing::warn!("shared path not brought over: {failed}");
                }
                Ok((create_dest, worktree_name, base))
            },
            move |state, result, cx| {
                state.pending.remove(&repo_id);
                match result {
                    Ok((path, name, base)) => {
                        state.refresh_repo(&repo_id, cx);
                        let path = path.canonicalize().unwrap_or(path);
                        // Git knows the branch it ended up on; it does not know what the worktree was forked from, and
                        // that base is what the diff will be taken against — so the call that
                        // ran `worktree add` is what records it.
                        let created = match state.worktrees.iter_mut().find(|w| w.path == path) {
                            Some(known) => {
                                known.base_ref = base;
                                known.clone()
                            }
                            // Not listed by git, which should not happen: keep
                            // it anyway rather than losing a worktree that is
                            // on disk.
                            None => {
                                let created = Worktree {
                                    id: path.display().to_string(),
                                    repo_id: repo_id.clone(),
                                    branch: git::branch_from_name(&name),
                                    name,
                                    path,
                                    base_ref: base,
                                };
                                state.worktrees.push(created.clone());
                                created
                            }
                        };
                        state.activate(&created.id.clone(), cx);
                        cx.emit(CoreEvent::WorktreeCreated { worktree: created });
                    }
                    Err(error) => {
                        state.errors.insert(repo_id.clone(), format!("{error:#}"));
                    }
                }
                state.persist();
                cx.notify();
            },
        );
    }

    /// `worktree::Remove`: the directory first, then the branch. A refusal
    /// without `force` lands in `error_for`, which is what the sidebar offers
    /// the forced retry from.
    pub fn remove_worktree(&mut self, id: &str, force: bool, cx: &mut Context<Self>) {
        // Owned, because the background work outlives this call.
        let id = id.to_string();
        let Some(target) = self.worktrees.iter().find(|w| w.id == id).cloned() else {
            return;
        };
        let Some(repo) = self.repos.iter().find(|r| r.id == target.repo_id).cloned() else {
            return;
        };
        // The repo's own checkout is not ours to remove.
        if target.path == repo.path {
            self.errors
                .insert(target.repo_id.clone(), t!("state.own_checkout").to_string());
            cx.notify();
            return;
        }

        let repo_id = target.repo_id.clone();
        self.pending.insert(
            repo_id.clone(),
            t!("state.removing", name = target.name).to_string(),
        );
        self.errors.remove(&repo_id);
        cx.notify();

        let repo_path = repo.path.clone();
        let remove_path = target.path.clone();
        let remove_branch = target.branch.clone();
        Self::off_thread(
            cx,
            move || -> anyhow::Result<()> {
                git::worktree_remove(&repo_path, &remove_path, force)?;
                if !remove_branch.is_empty() && remove_branch != "HEAD" {
                    git::branch_delete(&repo_path, &remove_branch, force)?;
                }
                Ok(())
            },
            move |state, result, cx| {
                state.pending.remove(&repo_id);
                match result {
                    Ok(()) => {
                        state.refresh_repo(&repo_id, cx);
                        if state.active.as_deref() == Some(id.as_str()) {
                            // Whatever is left of that repo becomes the active
                            // one; nothing at all means nothing is shown.
                            state.activate_first_of(&repo_id, cx);
                        }
                        state.forget_statuses(&id);
                        cx.emit(CoreEvent::WorktreeRemoved { worktree: target });
                    }
                    Err(error) => {
                        // Kept for the sidebar, which offers the forced retry
                        // from exactly here.
                        state.errors.insert(repo_id.clone(), format!("{error:#}"));
                    }
                }
                state.persist();
                cx.notify();
            },
        );
    }

    /// `repo::Remove`: take a repository out of the sidebar.
    pub fn remove_repo(&mut self, repo_id: &str, cx: &mut Context<Self>) {
        if !self.repos.iter().any(|repo| repo.id == repo_id) {
            return;
        }
        let gone: Vec<Worktree> = self.worktrees_of(repo_id).cloned().collect();
        self.repos.retain(|repo| repo.id != repo_id);
        self.worktrees
            .retain(|worktree| worktree.repo_id != repo_id);
        self.hidden_repos.retain(|id| id != repo_id);
        self.errors.remove(repo_id);
        self.pending.remove(repo_id);
        for worktree in gone {
            self.forget_statuses(&worktree.id);
            cx.emit(CoreEvent::WorktreeRemoved { worktree });
        }
        // The worktree on screen can have been one of that repo's.
        self.ensure_active_exists(cx);
        cx.emit(CoreEvent::RepoRemoved {
            repo_id: repo_id.to_string(),
        });
        self.persist();
        cx.notify();
    }

    /// `workspace::Refresh`: re-read every repo, which prunes what git no longer
    /// knows about.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        let before = self.worktrees.clone();
        self.reload_all(cx);
        self.refresh_changes(cx);
        let gone: Vec<Worktree> = before
            .into_iter()
            .filter(|old| !self.worktrees.iter().any(|new| new.id == old.id))
            .collect();
        for gone in gone {
            self.forget_statuses(&gone.id);
            cx.emit(CoreEvent::WorktreeRemoved { worktree: gone });
        }
        self.persist();
        cx.notify();
    }

    /// Re-read the active worktree's changed files against its base ref.
    pub fn refresh_changes(&mut self, cx: &mut Context<Self>) {
        let Some(worktree) = self
            .active
            .as_deref()
            .and_then(|id| self.worktrees.iter().find(|w| w.id == id))
            .cloned()
        else {
            return;
        };
        match git::changed_files(&worktree.path, &worktree.base_ref) {
            Ok(changes) => {
                self.changes_error.remove(&worktree.id);
                self.changes.insert(worktree.id, changes);
            }
            Err(error) => {
                self.changes.remove(&worktree.id);
                self.changes_error.insert(worktree.id, format!("{error:#}"));
            }
        }
        cx.notify();
    }

    /// One worktree's changed files, as `git::changed_files` reported them: paths
    /// are relative to that worktree's directory.
    pub fn changes(&self, worktree_id: &WorktreeId) -> &[FileChange] {
        self.changes.get(worktree_id).map_or(&[], Vec::as_slice)
    }

    pub fn changes_error(&self, worktree_id: &WorktreeId) -> Option<&str> {
        self.changes_error.get(worktree_id).map(String::as_str)
    }

    /// The git status letter for one file of a worktree, given the path relative
    /// to that worktree's directory. `None` for a file git has nothing to say
    /// about — which is most of them.
    pub fn change_status(&self, worktree_id: &WorktreeId, relative: &Path) -> Option<char> {
        self.changes(worktree_id)
            .iter()
            .find(|change| Path::new(&change.path) == relative)
            .map(|change| change.status)
    }

    // --- internals ---------------------------------------------------------

    fn reload_all(&mut self, cx: &mut Context<Self>) {
        let ids: Vec<RepoId> = self.repos.iter().map(|r| r.id.clone()).collect();
        // Only what belongs to a repo that is no longer registered goes; each
        // remaining repo's entries are replaced by `refresh_repo`, which has to
        // see them to keep the name and the base ref they carry.
        self.worktrees.retain(|w| ids.contains(&w.repo_id));
        for id in ids {
            self.refresh_repo(&id, cx);
        }
    }

    /// Replace one repo's worktrees with what git reports, keeping the name and
    /// base ref a previous entry had for the same path so a rename in git does
    /// not lose them.
    fn refresh_repo(&mut self, repo_id: &str, cx: &mut Context<Self>) {
        let Some(repo) = self.repos.iter().find(|r| r.id == repo_id).cloned() else {
            return;
        };
        match git::list_worktrees(&repo.path) {
            Ok(listed) => {
                self.errors.remove(repo_id);
                // What we knew before, so a refresh does not forget the name a
                // worktree was made with, or the base it was forked from — git
                // has neither of those to tell us.
                let known: Vec<Worktree> = self
                    .worktrees
                    .iter()
                    .filter(|w| w.repo_id == repo_id)
                    .cloned()
                    .collect();
                self.worktrees.retain(|w| w.repo_id != repo_id);
                for entry in listed {
                    // Resolved once, here: git already reports a resolved path,
                    // and the ones state.json kept are resolved too.
                    let entry = GitWorktree {
                        path: entry.path.canonicalize().unwrap_or(entry.path),
                        ..entry
                    };
                    // A worktree whose directory is gone is not ours to show,
                    // even before `git worktree prune` forgets it.
                    if !entry.path.is_dir() {
                        continue;
                    }
                    let previous = known.iter().find(|w| w.path == entry.path);
                    self.worktrees
                        .push(worktree_from_git(&repo, entry, previous));
                }
                self.ensure_active_exists(cx);
            }
            Err(error) => {
                self.errors
                    .insert(repo_id.to_string(), format!("{error:#}"));
            }
        }
    }

    /// Point the active worktree at something that still exists.
    fn ensure_active_exists(&mut self, cx: &mut Context<Self>) {
        let alive = self
            .active
            .as_deref()
            .is_some_and(|id| self.worktrees.iter().any(|w| w.id == id));
        if alive {
            return;
        }
        match self.worktrees.first().map(|w| w.id.clone()) {
            Some(first) => self.activate(&first, cx),
            // Nothing left at all: said out loud too, so a view that was showing
            // a worktree's files stops showing them.
            None => {
                self.active = None;
                cx.emit(CoreEvent::ActiveWorktreeChanged { worktree_id: None });
            }
        }
    }

    /// Make the repo's first worktree the active one, and *say so*.
    fn activate_first_of(&mut self, repo_id: &str, cx: &mut Context<Self>) {
        let first = self.worktrees_of(repo_id).map(|w| w.id.clone()).next();
        if let Some(first) = first {
            self.activate(&first, cx);
        }
    }

    fn snapshot(&self) -> State {
        State {
            repos: self.repos.clone(),
            worktrees: self.worktrees.clone(),
            tabs: self.tabs.clone(),
            active_worktree: self.active.clone(),
            sidebar_width: self.sidebar_width,
            right_width: self.right_width,
            hidden_repos: self.hidden_repos.clone(),
        }
    }

    /// Write state.json. A failure is kept for the sidebar: losing the layout
    /// on restart is worth saying out loud, and it is not worth a panic.
    fn persist(&mut self) {
        match self.store.save(&self.snapshot()) {
            Ok(()) => self.state_error = None,
            Err(error) => self.state_error = Some(format!("{error:#}")),
        }
    }

    /// Run `work` on the background executor and hand its result back to
    /// `apply` on the UI thread. Every git call long enough to drop a frame
    /// goes through here.
    fn off_thread<T, F, A>(cx: &mut Context<AppState>, work: F, apply: A)
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
        A: FnOnce(&mut AppState, T, &mut Context<AppState>) + 'static,
    {
        let task = cx.background_spawn(async move { work() });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |state, cx| apply(state, result, cx)).ok();
        })
        .detach();
    }
}

/// Why a name cannot be a worktree directory, as the sentence to say about it.
pub fn worktree_name_problem(name: &str) -> Option<String> {
    let error = worktree_name(name).err()?;
    Some(match error {
        WorktreeNameError::Empty => t!("new_worktree.needs_name").to_string(),
        WorktreeNameError::NotOneDirectory => t!("new_worktree.name_is_one_dir").to_string(),
        WorktreeNameError::NoAlphanumeric => t!("new_worktree.needs_alnum").to_string(),
        // One plain directory name cannot leave the root; only a repository name
        // that is itself a path can, and that is the state's own doing.
        WorktreeNameError::OutsideRoot => t!("new_worktree.name_is_one_dir").to_string(),
    })
}

/// Run one `[[hook]]` and say how it went.
fn run_hook(hook: &HookSpec, event: &CoreEvent, worktree: Option<&Worktree>) -> Result<(), String> {
    let mut cmd = std::process::Command::new("sh");
    cmd.arg("-c")
        .arg(&hook.run)
        .envs(hooks::environment(event, worktree));
    if let Some(wt) = worktree {
        cmd.current_dir(&wt.path);
    }
    let output = cmd.output().map_err(|error| {
        t!("state.hook_start_failed", run = hook.run, error = error).to_string()
    })?;
    if output.status.success() {
        return Ok(());
    }

    // One line, because this is drawn in a 240-pixel column: the first non-empty
    // line of stderr is where a failing tool puts its reason, and a tool that
    // printed two hundred lines is not going to have them all read anyway.
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let said = stderr
        .lines()
        .chain(stdout.lines())
        .map(str::trim)
        .find(|line| !line.is_empty());
    Err(match said {
        Some(line) => t!(
            "state.hook_failed",
            run = hook.run,
            status = ended(&output.status),
            line = clip(line, 160)
        )
        .to_string(),
        None => t!(
            "state.hook_failed_nothing",
            run = hook.run,
            status = ended(&output.status)
        )
        .to_string(),
    })
}

/// How a process ended, in a word: the exit code, or the signal that stopped it.
fn ended(status: &std::process::ExitStatus) -> String {
    match status.code() {
        Some(code) => t!("state.ended_exit", code = code).to_string(),
        None => t!("state.ended_signal").to_string(),
    }
}

/// `text` in at most `max` bytes' worth of characters, with an ellipsis when
/// something was cut. Bytes are not the measure because a terminal title, a
/// path and a git error are all full of characters that are more than one.
fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut cut: String = text.chars().take(max.saturating_sub(1)).collect();
    cut.push('\u{2026}');
    cut
}

/// Wire every action to the state: one command channel, so a click, a keybinding and a menu
/// item all arrive here.
pub fn register_actions(state: &Entity<AppState>, cx: &mut App) {
    register::<repo::Add>(state, cx, |state, action, cx| match action.path.clone() {
        Some(path) => state.update(cx, |state, cx| state.add_repo(path, cx)),
        None => {
            let picked = cx.prompt_for_paths(PathPromptOptions {
                files: false,
                directories: true,
                multiple: false,
                prompt: Some(t!("state.add_git_repository").to_string().into()),
            });
            let state = state.clone();
            cx.spawn(async move |cx| match picked.await {
                Ok(Ok(Some(paths))) => {
                    if let Some(path) = paths.into_iter().next() {
                        state.update(cx, |state, cx| state.add_repo(path, cx));
                    }
                }
                _ => {}
            })
            .detach();
        }
    });
    register::<repo::Remove>(state, cx, |state, action, cx| {
        state.update(cx, |state, cx| state.remove_repo(&action.repo_id, cx))
    });
    register::<worktree::Create>(state, cx, |state, action, cx| {
        state.update(cx, |state, cx| {
            state.create_worktree(&action.repo_id, &action.name, action.base.clone(), cx)
        })
    });
    register::<worktree::Remove>(state, cx, |state, action, cx| {
        state.update(cx, |state, cx| {
            state.remove_worktree(&action.worktree_id, action.force, cx)
        })
    });
    register::<worktree::Activate>(state, cx, |state, action, cx| {
        state.update(cx, |state, cx| state.activate(&action.worktree_id, cx))
    });
    register::<Refresh>(state, cx, |state, _action, cx| {
        state.update(cx, |state, cx| state.refresh(cx))
    });
}

/// One action → one handler that owns the state handle.
fn register<A: Action>(
    state: &Entity<AppState>,
    cx: &mut App,
    handler: impl Fn(&Entity<AppState>, &A, &mut App) + 'static,
) {
    let state = state.clone();
    cx.on_action(move |action: &A, cx: &mut App| handler(&state, action, cx));
}

/// Whether a mode wants the machine held awake, given whether any agent is
/// working. Pure, so the mapping is testable without spawning an assertion.
fn wants_awake(mode: ComputerAwakeMode, working: bool) -> bool {
    match mode {
        ComputerAwakeMode::On => true,
        ComputerAwakeMode::Auto => working,
        ComputerAwakeMode::Off => false,
    }
}

/// Whether a `[[hook]].on` name asks for `status`. The two names no `CoreEvent` carries: the
/// event is `agent_status_changed` for all of them.
fn wants_status(on: &str, status: AgentStatus) -> bool {
    match on {
        "agent_idle" => status == AgentStatus::Idle,
        "agent_needs_input" => status == AgentStatus::Permission,
        _ => false,
    }
}

/// The tag a terminal's notifications carry, so they replace rather than stack.
fn notification_tag(terminal_id: &TerminalId) -> String {
    format!("syro/{terminal_id}")
}

/// Map one `git worktree list` entry onto the model, keeping what we knew.
fn worktree_from_git(repo: &Repo, entry: GitWorktree, previous: Option<&Worktree>) -> Worktree {
    let branch = entry
        .branch
        .as_deref()
        .and_then(|b| b.strip_prefix("refs/heads/"))
        .unwrap_or("HEAD")
        .to_string();
    let name = previous.map(|p| p.name.clone()).unwrap_or_else(|| {
        if entry.path == repo.path {
            // The repo's own checkout is a worktree like any other; it is
            // named after the repo so the sidebar reads as one tree.
            repo.name.clone()
        } else {
            entry
                .path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| entry.path.display().to_string())
        }
    });
    Worktree {
        // The resolved path is the identity git gives a worktree, and it is
        // what a removal or a refresh has to match on.
        id: entry.path.display().to_string(),
        repo_id: repo.id.clone(),
        name,
        path: entry.path,
        base_ref: previous
            .map(|p| p.base_ref.clone())
            .unwrap_or(branch.clone()),
        branch,
    }
}

/// What to call a worktree on one line: its branch, or its own name when git has
/// no branch on it — a checkout on no branch reports `HEAD` (`worktree_from_git`
/// above), and `HEAD` is not a name.
pub fn branch_or_name(worktree: &Worktree) -> &str {
    if worktree.branch == "HEAD" {
        &worktree.name
    } else {
        &worktree.branch
    }
}

#[cfg(test)]
pub(crate) mod fixture {
    //! A scratch repo with two worktrees, for this crate's tests. Both test
    //! modules build the same thing, so it is built once here.

    use std::path::{Path, PathBuf};
    use std::process::Command;

    use syro_core::model::Repo;

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap_or_else(|e| panic!("git {args:?} in {}: {e}", dir.display()));
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// A `state.json` path in a scratch directory, so no test can touch `~/.config`.
    pub(crate) fn temp_store(tag: &str) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "syro-store-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        (dir.join("state.json"), dir)
    }

    /// `(scratch dir, the repo, the two extra worktree paths)`.
    pub(crate) fn scratch_repo(tag: &str) -> (PathBuf, Repo, Vec<PathBuf>) {
        let dir = std::env::temp_dir().join(format!(
            "syro-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let main = dir.join("main");
        std::fs::create_dir_all(&main).unwrap();
        git(&dir, &["init", "-q", "-b", "main", main.to_str().unwrap()]);
        git(&main, &["config", "user.email", "test@example.com"]);
        git(&main, &["config", "user.name", "syro test"]);
        std::fs::write(main.join("README.md"), "scratch\n").unwrap();
        git(&main, &["add", "-A"]);
        git(&main, &["commit", "-q", "-m", "initial"]);

        let mut created = Vec::new();
        for name in ["wt-one", "wt-two"] {
            let path = dir.join(name);
            git(
                &main,
                &[
                    "worktree",
                    "add",
                    "-q",
                    "-b",
                    name,
                    path.to_str().unwrap(),
                    "main",
                ],
            );
            created.push(path.canonicalize().unwrap_or(path));
        }

        // Canonical, because that is what git and `repo_root` report and what
        // the app therefore keeps: on macOS `temp_dir()` is `/var/...`, which
        // is a symlink for `/private/var/...`.
        let main = main.canonicalize().unwrap_or(main);
        let repo = Repo {
            id: format!("repo:{}", main.display()),
            name: "main".to_string(),
            path: main,
            shared_paths: Vec::new(),
        };
        (dir, repo, created)
    }
}

#[cfg(test)]
mod tests {
    // Not `use super::*`: the module above globs gpui in, and gpui's own `test`
    // attribute comes with it — every `#[gpui_kit::test]` below then expands
    // into itself. Explicit imports keep Rust's `#[test]` out of the way.
    use std::path::PathBuf;

    use gpui_kit::{AppContext as _, TestAppContext};
    use syro_core::config::{ComputerAwakeMode, Settings};
    use syro_core::model::{AgentStatus, HookSpec, State, TabLayout, TabSpec, Worktree};
    use syro_core::store::Store;

    use super::{AppState, branch_or_name, wants_awake};
    use crate::state::fixture::{scratch_repo, temp_store};

    /// The one line a worktree is called by, which is the line both the sidebar
    /// row and the resource panel draw: its branch, or its own name when git has
    /// no branch on it.
    #[test]
    fn a_worktree_is_called_by_its_branch_or_by_its_name() {
        let worktree = |name: &str, branch: &str| Worktree {
            id: format!("repo:/w/{name}"),
            repo_id: "repo:/w".to_string(),
            name: name.to_string(),
            path: PathBuf::from("/w").join(name),
            branch: branch.to_string(),
            base_ref: "main".to_string(),
        };

        // The branch, even when the name says something else: a worktree this app
        // made is named after its branch, and one made elsewhere need not be.
        assert_eq!(
            branch_or_name(&worktree("main-2", "feat/solana")),
            "feat/solana"
        );
        assert_eq!(branch_or_name(&worktree("blog", "main")), "main");
        // A checkout on no branch reports `HEAD`, and `HEAD` is not a name.
        assert_eq!(branch_or_name(&worktree("detached", "HEAD")), "detached");
    }

    /// The three answers against the two facts that decide them.
    #[test]
    fn the_awake_mode_says_when_to_stay_awake() {
        // `on` is unconditional and `off` is never, working agent or not.
        assert!(wants_awake(ComputerAwakeMode::On, false));
        assert!(wants_awake(ComputerAwakeMode::On, true));
        assert!(!wants_awake(ComputerAwakeMode::Off, false));
        assert!(!wants_awake(ComputerAwakeMode::Off, true));
        // `auto` is exactly "an agent is working".
        assert!(!wants_awake(ComputerAwakeMode::Auto, false));
        assert!(wants_awake(ComputerAwakeMode::Auto, true));
    }

    /// `[[hook]]`s named after a *status* — the `agent_idle` / `agent_needs_input` — run when
    /// that status arrives, and only then.
    #[gpui_kit::test]
    fn a_status_hook_runs_for_its_status_and_no_other(cx: &mut TestAppContext) {
        let (dir, repo, _) = scratch_repo("state-hooks");
        let (state_path, store_dir) = temp_store("state-hooks");
        Store::new(state_path.clone())
            .save(&State {
                repos: vec![repo.clone()],
                ..Default::default()
            })
            .unwrap();

        // A marker file per hook: the hook's whole job is to prove it ran, and
        // the process is detached, so the file is the only way to see it.
        let waited = dir.join("needs-input-ran");
        let idle = dir.join("idle-ran");
        let hook = |on: &str, marker: &std::path::Path| HookSpec {
            on: on.to_string(),
            run: format!("touch {}", marker.display()),
            terminal: false,
        };
        let settings = Settings {
            hooks: vec![
                hook("agent_needs_input", &waited),
                hook("agent_idle", &idle),
            ],
            ..Default::default()
        };

        let state = cx.update(|cx| {
            cx.new(|cx| {
                AppState::open(Store::new(state_path), dir.join("workspaces"), cx)
                    .with_settings(settings)
            })
        });
        let worktree = cx.update(|cx| {
            state
                .read(cx)
                .worktrees_of(&repo.id)
                .next()
                .expect("the repo's own checkout")
                .id
                .clone()
        });
        let note = |cx: &mut TestAppContext, status: Option<AgentStatus>| {
            cx.update(|cx| {
                state.update(cx, |state, cx| {
                    state.note_agent_status(
                        &worktree,
                        &"terminal-hook".to_string(),
                        Some("droid".to_string()),
                        status,
                        false,
                        cx,
                    )
                })
            });
        };

        note(cx, Some(AgentStatus::Permission));
        assert!(ran(cx, &waited), "the agent_needs_input hook never ran");
        assert!(
            !idle.exists(),
            "the agent_idle hook ran for a permission status"
        );

        note(cx, Some(AgentStatus::Idle));
        assert!(ran(cx, &idle), "the agent_idle hook never ran");

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&store_dir);
    }

    /// A hook that fails says so, in one line, naming itself and what it printed.
    #[gpui_kit::test]
    fn a_failing_hook_is_reported_with_what_it_said(cx: &mut TestAppContext) {
        let (dir, repo, _) = scratch_repo("state-hook-failed");
        let (state_path, store_dir) = temp_store("state-hook-failed");
        Store::new(state_path.clone())
            .save(&State {
                repos: vec![repo.clone()],
                ..Default::default()
            })
            .unwrap();
        let settings = Settings {
            hooks: vec![HookSpec {
                on: "agent_idle".to_string(),
                // Both streams, because a hook that fails prints to either, and
                // an exit code with nothing said is its own case.
                run: "echo the setup script broke >&2; echo and to stdout; exit 3".to_string(),
                terminal: false,
            }],
            ..Default::default()
        };

        let state = cx.update(|cx| {
            cx.new(|cx| {
                AppState::open(Store::new(state_path), dir.join("workspaces"), cx)
                    .with_settings(settings)
            })
        });
        let worktree = cx.update(|cx| {
            state
                .read(cx)
                .worktrees_of(&repo.id)
                .next()
                .expect("the repo's own checkout")
                .id
                .clone()
        });
        let before = cx.update(|cx| state.read(cx).worktrees_of(&repo.id).count());

        cx.update(|cx| {
            state.update(cx, |state, cx| {
                state.note_agent_status(
                    &worktree,
                    &"terminal-hook".to_string(),
                    Some("droid".to_string()),
                    Some(AgentStatus::Idle),
                    false,
                    cx,
                );
            })
        });

        let message = wait_for_message(cx, &state);
        let message = message.expect("a failed hook was never reported");
        assert!(
            message.contains("echo the setup script broke"),
            "the message does not name the hook that failed: {message}"
        );
        assert!(
            message.contains("exit 3"),
            "the message does not say how it ended: {message}"
        );
        assert!(
            message.contains("the setup script broke"),
            "the message does not say what the process said: {message}"
        );
        // stderr before stdout: that is where a failing tool puts its reason, so
        // the sentence ends with the stderr line and not the one from stdout.
        assert!(
            message.ends_with("the setup script broke"),
            "the message did not end with what the process said on stderr: {message}"
        );

        // Nothing was undone for it.
        assert_eq!(
            cx.update(|cx| state.read(cx).worktrees_of(&repo.id).count()),
            before,
            "a failed hook removed a worktree"
        );

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&store_dir);
    }

    /// A hook that works leaves no message, and clears the one the last failure
    /// left: a banner nobody can get rid of is a banner people stop reading.
    #[gpui_kit::test]
    fn a_hook_that_works_reports_nothing(cx: &mut TestAppContext) {
        let (dir, repo, _) = scratch_repo("state-hook-ok");
        let (state_path, store_dir) = temp_store("state-hook-ok");
        Store::new(state_path.clone())
            .save(&State {
                repos: vec![repo.clone()],
                ..Default::default()
            })
            .unwrap();
        // The marker is how the test knows the hook *ran*: a message that is
        // absent because nothing happened proves nothing.
        let marker = dir.join("ok-ran");
        let settings = Settings {
            hooks: vec![HookSpec {
                on: "agent_idle".to_string(),
                run: format!("touch {}", marker.display()),
                terminal: false,
            }],
            ..Default::default()
        };

        let state = cx.update(|cx| {
            cx.new(|cx| {
                AppState::open(Store::new(state_path), dir.join("workspaces"), cx)
                    .with_settings(settings)
            })
        });
        let worktree = cx.update(|cx| {
            state
                .read(cx)
                .worktrees_of(&repo.id)
                .next()
                .expect("the repo's own checkout")
                .id
                .clone()
        });

        // A failure first, so the clearing half is testable.
        cx.update(|cx| {
            state.update(cx, |state, _cx| {
                state.hook_error = Some("an earlier hook failed".to_string());
            })
        });

        cx.update(|cx| {
            state.update(cx, |state, cx| {
                state.note_agent_status(
                    &worktree,
                    &"terminal-hook".to_string(),
                    Some("droid".to_string()),
                    Some(AgentStatus::Idle),
                    false,
                    cx,
                );
            })
        });

        assert!(ran(cx, &marker), "the hook never ran");
        // Wait for the write-back, which happens after the process is reaped.
        let cleared = {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            loop {
                cx.run_until_parked();
                if cx.update(|cx| state.read(cx).hook_error().is_none()) {
                    break true;
                }
                if std::time::Instant::now() > deadline {
                    break false;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        };
        assert!(cleared, "the message from the last failure stayed up");

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&store_dir);
    }

    /// The message a failed hook put in the state, once it lands: the failure
    /// arrives on the UI thread after the process is reaped, which is a wait and
    /// not a frame.
    fn wait_for_message(
        cx: &mut TestAppContext,
        state: &gpui_kit::Entity<AppState>,
    ) -> Option<String> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            cx.run_until_parked();
            if let Some(message) = cx.update(|cx| state.read(cx).hook_error().map(str::to_string)) {
                return Some(message);
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        None
    }

    /// A panel width a previous run dragged is what this one opens with, and a
    /// width from a wider window is clamped rather than used as it is.
    #[gpui_kit::test]
    fn a_remembered_panel_width_is_read_back_and_clamped(cx: &mut TestAppContext) {
        let (dir, repo, _) = scratch_repo("state-widths");
        let (state_path, store_dir) = temp_store("state-widths");
        Store::new(state_path.clone())
            .save(&State {
                repos: vec![repo],
                sidebar_width: Some(410.),
                // From a window much wider than this one, and past the range the
                // drag itself allows: the panel clamps it on the way in, so it
                // cannot open off the edge.
                right_width: Some(9_000.),
                ..Default::default()
            })
            .unwrap();

        let state = cx.update(|cx| {
            cx.new(|cx| AppState::open(Store::new(state_path.clone()), dir.join("workspaces"), cx))
        });
        assert_eq!(cx.update(|cx| state.read(cx).sidebar_width()), Some(410.));
        assert_eq!(cx.update(|cx| state.read(cx).right_width()), Some(9_000.));

        // Writing one back is a write to `state.json`, because that is what a
        // restart reads.
        cx.update(|cx| {
            state.update(cx, |state, cx| state.set_sidebar_width(280., cx));
            state.update(cx, |state, cx| state.set_right_width(300., cx));
        });
        let saved = Store::new(state_path).load().unwrap();
        assert_eq!(saved.sidebar_width, Some(280.));
        assert_eq!(saved.right_width, Some(300.));

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&store_dir);
    }

    /// Wait for the detached hook process to have made its mark.
    fn ran(cx: &mut TestAppContext, marker: &std::path::Path) -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            cx.run_until_parked();
            if marker.exists() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        false
    }

    #[gpui_kit::test]
    fn a_saved_repo_is_reloaded_from_git_and_prunes_missing_worktrees(cx: &mut TestAppContext) {
        let (dir, repo, worktrees) = scratch_repo("state-load");
        let (state_path, store_dir) = temp_store("state-load");
        let store = Store::new(state_path.clone());
        // A tab from a previous run: this module does not interpret the tab
        // list (the tab bar does), and it must survive the round trip anyway.
        let tab = TabSpec::Terminal {
            cwd: repo.path.clone(),
            agent: Some("claude".to_string()),
        };
        store
            .save(&State {
                repos: vec![repo.clone()],
                tabs: vec![TabLayout::Pane(tab.clone())],
                ..Default::default()
            })
            .unwrap();

        let state = cx.update(|cx| {
            cx.new(|cx| AppState::open(Store::new(state_path), dir.join("workspaces"), cx))
        });
        cx.update(|cx| {
            let state = state.read(cx);
            assert_eq!(state.repos().len(), 1);
            // The repo's own checkout plus the two created: git is the truth,
            // and state.json only said which repo to look at.
            assert_eq!(state.worktrees_of(&repo.id).count(), 3);
            assert!(
                worktrees
                    .iter()
                    .all(|path| state.worktrees_of(&repo.id).any(|w| &w.path == path))
            );
            // Something is active, so the workspace has something to show.
            assert!(state.active().is_some());
        });

        // An external `git worktree remove`, then a refresh: the row is gone.
        let removed = worktrees[0].clone();
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo.path)
            .args(["worktree", "remove", "--force"])
            .arg(&removed)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );

        cx.update(|cx| state.update(cx, |state, cx| state.refresh(cx)));
        cx.update(|cx| {
            let state = state.read(cx);
            assert_eq!(state.worktrees_of(&repo.id).count(), 2);
            assert!(!state.worktrees_of(&repo.id).any(|w| w.path == removed));
        });
        assert_eq!(store.load().unwrap().tabs, vec![TabLayout::Pane(tab)]);

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&store_dir);
    }

    #[gpui_kit::test]
    fn removing_a_repo_takes_the_rows_and_leaves_the_directories(cx: &mut TestAppContext) {
        let (dir, repo, worktrees) = scratch_repo("state-remove-repo");
        let (state_path, store_dir) = temp_store("state-remove-repo");
        let store = Store::new(state_path.clone());
        store
            .save(&State {
                repos: vec![repo.clone()],
                ..Default::default()
            })
            .unwrap();
        let state = cx.update(|cx| {
            cx.new(|cx| AppState::open(Store::new(state_path), dir.join("workspaces"), cx))
        });

        // The repo's own checkout, which is the row this can be asked about and
        // the one git will not remove: `is_own_checkout` is what the dialog asks.
        cx.update(|cx| {
            let state = state.read(cx);
            assert!(state.is_own_checkout(&repo.path.display().to_string()));
            assert!(
                worktrees
                    .iter()
                    .all(|path| !state.is_own_checkout(&path.display().to_string())),
                "a created worktree was mistaken for the repo's own checkout"
            );
        });

        cx.update(|cx| state.update(cx, |state, cx| state.remove_repo(&repo.id, cx)));
        cx.update(|cx| {
            let state = state.read(cx);
            assert!(state.repos().is_empty());
            assert!(state.worktrees().is_empty());
            // Nothing is left for the pane area to show.
            assert_eq!(state.active_id(), None);
        });
        assert!(store.load().unwrap().repos.is_empty());
        // Left exactly where they were: this is not `worktree remove`.
        assert!(repo.path.is_dir());
        assert!(worktrees.iter().all(|path| path.is_dir()));

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&store_dir);
    }

    #[gpui_kit::test]
    fn activating_a_worktree_persists_it(cx: &mut TestAppContext) {
        let (dir, repo, worktrees) = scratch_repo("state-activate");
        let (state_path, store_dir) = temp_store("state-activate");
        let store = Store::new(state_path.clone());
        store
            .save(&State {
                repos: vec![repo.clone()],
                ..Default::default()
            })
            .unwrap();

        let state = cx.update(|cx| {
            cx.new(|cx| AppState::open(Store::new(state_path), dir.join("workspaces"), cx))
        });
        let wanted = worktrees[1].display().to_string();
        cx.update(|cx| state.update(cx, |state, cx| state.activate(&wanted, cx)));

        cx.update(|cx| assert_eq!(state.read(cx).active_id(), Some(&wanted)));
        let reloaded = store.load().unwrap();
        assert_eq!(reloaded.active_worktree, Some(wanted));

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&store_dir);
    }

    #[gpui_kit::test]
    fn adding_a_subdirectory_adds_the_repository(cx: &mut TestAppContext) {
        let (dir, repo, _) = scratch_repo("state-add");
        let (state_path, store_dir) = temp_store("state-add");
        let nested = repo.path.join("nested");
        std::fs::create_dir_all(&nested).unwrap();

        let state = cx.update(|cx| {
            cx.new(|cx| AppState::open(Store::new(state_path), dir.join("workspaces"), cx))
        });
        cx.update(|cx| state.update(cx, |state, cx| state.add_repo(nested, cx)));

        cx.update(|cx| {
            let state = state.read(cx);
            assert_eq!(state.repos().len(), 1);
            assert_eq!(state.repos()[0].path, repo.path);
            assert!(state.active().is_some(), "adding activates something");
        });

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&store_dir);
    }

    #[gpui_kit::test]
    fn a_directory_that_is_not_a_repo_is_reported_not_added(cx: &mut TestAppContext) {
        let (state_path, store_dir) = temp_store("state-notrepo");
        let (dir, _, _) = scratch_repo("state-notrepo");
        let bare = dir.join("not-a-repo");
        std::fs::create_dir_all(&bare).unwrap();

        let state = cx.update(|cx| {
            cx.new(|cx| AppState::open(Store::new(state_path), dir.join("workspaces"), cx))
        });
        cx.update(|cx| state.update(cx, |state, cx| state.add_repo(bare, cx)));

        cx.update(|cx| {
            let state = state.read(cx);
            assert!(state.repos().is_empty());
            assert!(state.state_error().is_some());
        });

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&store_dir);
    }

    #[gpui_kit::test]
    fn creating_a_worktree_lands_in_the_state_and_in_git(cx: &mut TestAppContext) {
        let (dir, mut repo, _) = scratch_repo("state-create");
        // Untracked, so only the shared path can bring it to the new worktree.
        std::fs::write(repo.path.join(".env"), "KEY=1\n").unwrap();
        repo.shared_paths = vec![".env".into()];
        let (state_path, store_dir) = temp_store("state-create");
        let store = Store::new(state_path.clone());
        store
            .save(&State {
                repos: vec![repo.clone()],
                ..Default::default()
            })
            .unwrap();

        let root = dir.join("workspaces");
        let canonical_root = dir
            .canonicalize()
            .unwrap_or_else(|_| dir.clone())
            .join("workspaces");
        let state = cx.update(|cx| cx.new(|cx| AppState::open(store, root.clone(), cx)));
        cx.update(|cx| {
            state.update(cx, |state, cx| {
                state.create_worktree(&repo.id, "my task", None, cx)
            })
        });
        // The git call is on the background executor; a real app would get
        // there on its own, a test has to pump. Wall clock, not a frame count:
        // a count that is generous on an idle machine is not on a busy one.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while std::time::Instant::now() < deadline {
            cx.run_until_parked();
            let done = cx.update(|cx| {
                let state = state.read(cx);
                state.worktrees_of(&repo.id).any(|w| w.name == "my task")
            });
            if done {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        cx.update(|cx| {
            let state = state.read(cx);
            let created = state
                .worktrees_of(&repo.id)
                .find(|w| w.name == "my task")
                .expect("the worktree never appeared");
            // The layout is <root>/<repo>/<name>, resolved the way git
            // resolves it.
            assert_eq!(
                created.path,
                canonical_root.join(&repo.name).join("my task")
            );
            assert_eq!(created.branch, "my-task");
            assert_eq!(state.active_id(), Some(&created.id));
            assert_eq!(
                std::fs::read_to_string(created.path.join(".env"))
                    .ok()
                    .as_deref(),
                Some("KEY=1\n"),
                "the shared path was not brought into the new worktree"
            );
        });

        // A refresh re-reads git, which knows neither the name nor the base:
        // what the state knew has to survive it.
        let created = cx.update(|cx| {
            let state = state.read(cx);
            state
                .worktrees_of(&repo.id)
                .find(|w| w.name == "my task")
                .cloned()
                .expect("the worktree is gone")
        });
        assert_eq!(created.base_ref, "main");
        cx.update(|cx| state.update(cx, |state, cx| state.refresh(cx)));
        cx.update(|cx| {
            let after = state
                .read(cx)
                .worktrees_of(&repo.id)
                .find(|w| w.path == created.path)
                .cloned()
                .expect("the refresh dropped the worktree");
            assert_eq!(after.name, "my task");
            assert_eq!(after.base_ref, "main", "the base ref was forgotten");
        });

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&store_dir);
    }

    /// The name reaches the state through the action, which is a door the dialog
    /// does not stand in: a name that is a path is refused here, before git is
    /// asked, so nothing lands outside the worktree root.
    #[gpui_kit::test]
    fn a_name_that_is_a_path_never_reaches_git(cx: &mut TestAppContext) {
        let (dir, repo, _) = scratch_repo("state-bad-name");
        let (state_path, store_dir) = temp_store("state-bad-name");
        let store = Store::new(state_path);
        store
            .save(&State {
                repos: vec![repo.clone()],
                ..Default::default()
            })
            .unwrap();

        let state = cx.update(|cx| cx.new(|cx| AppState::open(store, dir.join("workspaces"), cx)));
        let before = cx.update(|cx| state.read(cx).worktrees_of(&repo.id).count());
        for name in ["../escape", "/tmp/escape", "", "---"] {
            cx.update(|cx| {
                state.update(cx, |state, cx| {
                    state.create_worktree(&repo.id, name, None, cx)
                })
            });
        }
        cx.run_until_parked();

        cx.update(|cx| {
            let state = state.read(cx);
            assert_eq!(
                state.worktrees_of(&repo.id).count(),
                before,
                "a refused name must not add a worktree"
            );
            assert!(
                state.error_for(&repo.id).is_some(),
                "the name was not reported"
            );
        });
        // Outside the root, where git would have put `../escape`, and none of the
        // root itself: nothing was asked of git at all.
        assert!(!dir.join("escape").exists());
        assert!(!dir.join("workspaces").exists());

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&store_dir);
    }
}
