//! The source control panel: the branch, what is on it, and the way to commit and push it.

use std::collections::BTreeMap;
use std::path::PathBuf;

use std::time::{Duration, Instant};

use gpui_kit::base::{ColorTokens, StyledExt as _, TestSupportExt as _, Theme};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme as _, Icon, IconName};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rust_i18n::t;
use syro_core::event::CoreEvent;
use syro_core::git::{self, Branch, BranchStatus, Commit, FileChange};
use syro_core::model::{AgentStatus, WorktreeId};
use syro_core::session;

use crate::state::{AppState, diff, editor};

/// The header's height, which the activity strip's and the tab bar's is.
const HEADER_HEIGHT: f32 = 36.;

/// How much history the list holds: a page of commits, and a "50 +" count in
/// the section header that says there are more.
const COMMITS: usize = 50;

/// How many changed files the section lists before it says the rest are in the
/// diff tab. A worktree with four hundred changed files is one where the
/// history below matters less than the file list, not more.
const FILES: usize = 30;

/// How often the panel re-reads while it is on screen.
const POLL: Duration = Duration::from_secs(2);

/// How tall the history gets before it scrolls inside itself.
pub(crate) const SECTION_MAX_HEIGHT: f32 = 240.;

/// The colours a change is drawn in: the semantic theme's own success, danger and info,
/// resolved once per frame rather than looked up per row.
#[derive(Debug, Clone, Copy)]
struct Accents {
    /// `+85`: lines that arrived.
    added: Hsla,
    /// `-9`: lines that left.
    removed: Hsla,
    /// A commit the upstream has not got, drawn blue: `info` is the
    /// token that means "new", and `success` would read as "done".
    outgoing: Hsla,
}

impl Accents {
    fn of(cx: &App) -> Self {
        let theme = cx.theme();
        Self {
            added: theme.success,
            removed: theme.danger,
            outgoing: theme.info,
        }
    }
}

/// What the panel's one button does, from what git says about the worktree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    /// There is uncommitted work: commit it under the message.
    Commit,
    /// A clean tree with commits the upstream does not have: push them.
    Push,
    /// The same, for a branch git has no upstream for yet.
    Publish,
    /// Nothing to commit and nothing to push.
    Idle,
}

impl Action {
    fn label(self) -> String {
        match self {
            Action::Commit => t!("git.commit"),
            Action::Push => t!("git.push"),
            Action::Publish => t!("git.publish"),
            Action::Idle => t!("git.commit"),
        }
        .to_string()
    }

    fn icon(self) -> IconName {
        match self {
            // The commit button carries a check, and the push an arrow.
            Action::Idle | Action::Commit => IconName::Check,
            Action::Push | Action::Publish => IconName::ArrowUp,
        }
    }
}

/// One operation the header's menu runs: what the panel can do *to* the
/// repository, rather than with what is in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sync {
    Fetch,
    Pull,
    Push,
    Publish,
}

impl Sync {
    /// Every row, in the order a reader runs them: what is out there, then what
    /// comes in, then what goes out.
    const ALL: [Sync; 4] = [Sync::Fetch, Sync::Pull, Sync::Push, Sync::Publish];

    fn label(self) -> String {
        match self {
            Sync::Fetch => t!("git.fetch"),
            Sync::Pull => t!("git.pull"),
            Sync::Push => t!("git.push"),
            Sync::Publish => t!("git.publish"),
        }
        .to_string()
    }

    /// What the notice says while it runs.
    fn working(self) -> String {
        match self {
            Sync::Fetch => t!("git.fetching"),
            Sync::Pull => t!("git.pulling"),
            Sync::Push => t!("git.pushing"),
            Sync::Publish => t!("git.publishing"),
        }
        .to_string()
    }

    fn id(self) -> &'static str {
        match self {
            Sync::Fetch => "git-sync-fetch",
            Sync::Pull => "git-sync-pull",
            Sync::Push => "git-sync-push",
            Sync::Publish => "git-sync-publish",
        }
    }

    /// Whether there is anything for it to do. A pull and a push are between two
    /// branches, so a branch git has no upstream for has neither of them — it has
    /// a publish, which is what makes the upstream in the first place. A detached
    /// HEAD has no branch to publish *as*, so it has only the fetch.
    fn enabled(self, status: Option<&BranchStatus>) -> bool {
        let upstream = status.is_some_and(|status| status.upstream.is_some());
        let named = status.is_some_and(|status| status.branch.is_some());
        match self {
            Sync::Fetch => true,
            Sync::Pull | Sync::Push => upstream,
            Sync::Publish => named && !upstream,
        }
    }

    fn run(self, root: &std::path::Path) -> anyhow::Result<String> {
        match self {
            Sync::Fetch => git::fetch(root),
            Sync::Pull => git::pull(root),
            Sync::Push => git::push(root, false),
            Sync::Publish => git::push(root, true),
        }
    }
}

/// One commit's files, as the history's row opened onto them.
struct OpenCommit {
    /// The commit the rows under it belong to.
    id: String,
    /// What git said it changed, or why it would not say. Read on the click and
    /// kept: a commit's files are asked for when they are looked at, never for
    /// the fifty rows above and below — and they do not change afterwards.
    files: Result<Vec<FileChange>, String>,
}

/// Which menu is up, and where it was opened: the header's git operations, or
/// the branches the branch line offers to switch to.
enum Menu {
    Sync(Point<Pixels>),
    Branch(Point<Pixels>),
}

impl Menu {
    /// Where it was opened, which is where it is drawn from.
    fn at(&self) -> Point<Pixels> {
        match self {
            Self::Sync(at) | Self::Branch(at) => *at,
        }
    }
}

pub struct GitPanel {
    state: Entity<AppState>,
    /// The branch on screen. `None` until the first read lands, and after one
    /// that failed.
    status: Option<BranchStatus>,
    commits: Vec<Commit>,
    /// The commit whose files are open under its row, if one is: one at a time,
    /// because the history scrolls inside 240 px and a second open commit would
    /// push the rows between them out of it.
    open: Option<OpenCommit>,
    /// The changes on the branch, and how much each one moved: the section's
    /// rows are the first and the `+N -M` are the second.
    changes: Vec<FileChange>,
    stats: BTreeMap<String, (u32, u32)>,
    /// Whether there is uncommitted work. This is what decides between Commit
    /// and Push, and it is a different question from "what is on the branch":
    /// `changes` counts both, and a commit needs only the first.
    dirty: bool,
    /// What the changes are relative to, as the worktree records it — the base
    /// ref the diff tab compares against, so the panel and the tab agree.
    base_ref: String,
    /// The worktree's directory: what the panel's relative paths are relative
    /// *to*, and what opening one in the editor has to make them absolute with.
    root: PathBuf,
    /// Which sections are folded away. The chevron in a section's header is what
    /// changes these; a remembered per-panel option needs somewhere else to live.
    files_collapsed: bool,
    commits_collapsed: bool,
    /// The menu that is up, and where it was opened. `None` is no menu, which is
    /// the same shape the pane's menu and the tab bar's use.
    menu: Option<Menu>,
    /// Every local branch this checkout can switch to, read with the rest.
    branches: Vec<Branch>,
    /// The commit message, mirrored from the field on every keystroke.
    message: String,
    input: Option<Entity<InputState>>,
    _input_events: Option<Subscription>,
    /// git's own words after a commit or a push, or why there was no commit.
    /// Drawn under the button until the next read replaces it.
    notice: Option<String>,
    /// The worktree the notice came from. git's words are about one repository,
    /// and they are the panel's only while that one is the one on screen.
    notice_worktree: Option<WorktreeId>,
    /// A commit that finished wants the message box empty, and clearing an
    /// `InputState` takes a window — which is the next frame, not the task.
    clear_message: bool,
    /// A commit or a push is in flight. The button is disabled meanwhile:
    /// two of them at once is two commits, or a push racing its own commit.
    busy: bool,
    /// Why the last read failed, if it did. git's own words: "does not have any
    /// commits yet" is more useful than anything this file could say.
    error: Option<String>,
    /// Whether a read has ever happened, so the empty state can say "reading"
    /// rather than "nothing".
    read: bool,
    /// Whether something moved under the panel while it was not on screen.
    stale: bool,
    /// The panel's own focus, so a click that dispatches `diff::Open` — which
    /// needs a window — dispatches from inside the workspace's subtree, where
    /// the workspace's own element listener sees it.
    focus: FocusHandle,
    /// The last commit or push. Held so it is not dropped mid-flight.
    _task: Option<Task<()>>,
    /// When the panel was last drawn. [`POLL`] reads this to tell the panel in
    /// front of the reader from one behind another icon: `git` runs for the
    /// first and not for the second.
    drawn: Option<Instant>,
    /// Re-reads when the thing it describes moves.
    _events: Subscription,
}

impl GitPanel {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        // The tick behind [`POLL`]: see `Self::poll` for what it does with it.
        // Ended by the view going away, which is the only reason `update` fails.
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(POLL).await;
                if this.update(cx, |panel, cx| panel.poll(cx)).is_err() {
                    return;
                }
            }
        })
        .detach();
        let events = cx.subscribe(&state, |this, _state, event: &CoreEvent, cx| {
            match event {
                CoreEvent::ActiveWorktreeChanged { .. }
                | CoreEvent::WorktreeCreated { .. }
                | CoreEvent::WorktreeRemoved { .. } => {
                    this.stale = true;
                    cx.notify();
                }
                // A turn that ended is when files changed, which is what the changes count and the history are
                // about.
                CoreEvent::AgentStatusChanged {
                    worktree_id,
                    status: Some(AgentStatus::Idle),
                    ..
                } if this.state.read(cx).active_id() == Some(worktree_id) => {
                    this.stale = true;
                    cx.notify();
                }
                _ => {}
            }
        });
        Self {
            state,
            status: None,
            commits: Vec::new(),
            open: None,
            changes: Vec::new(),
            stats: BTreeMap::new(),
            dirty: false,
            base_ref: String::new(),
            root: PathBuf::new(),
            files_collapsed: false,
            commits_collapsed: false,
            menu: None,
            branches: Vec::new(),
            message: String::new(),
            input: None,
            _input_events: None,
            notice: None,
            notice_worktree: None,
            clear_message: false,
            busy: false,
            error: None,
            read: false,
            stale: false,
            focus: cx.focus_handle(),
            _task: None,
            drawn: None,
            _events: events,
        }
    }

    /// Read the branch, the changes and the history again.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        // git's own words are about the worktree they were said in, and they are
        // the panel's only while that worktree is the one on screen: a push that
        // lands after the reader has moved on is not this repository's news.
        if self.notice_worktree != self.state.read(cx).active_id().cloned() {
            self.notice = None;
            self.notice_worktree = None;
        }
        let Some(worktree) = self.state.read(cx).active().cloned() else {
            self.status = None;
            self.commits.clear();
            self.open = None;
            self.changes.clear();
            self.stats.clear();
            self.dirty = false;
            self.base_ref.clear();
            self.root.clear();
            self.error = None;
            self.read = true;
            cx.notify();
            return;
        };
        // The changes list is the state's, and it is what the diff tab reads:
        // refreshing it here is what keeps the two panels from disagreeing.
        self.state.update(cx, |state, cx| state.refresh_changes(cx));
        match (
            git::branch_status(&worktree.path),
            git::commits(&worktree.path, COMMITS),
        ) {
            (Ok(status), Ok(commits)) => {
                self.status = Some(status);
                self.commits = commits;
                self.error = None;
            }
            (branch, log) => {
                // One sentence, and git's own words: the branch's failure if it had one (a history that failed
                // too is almost always the same failure twice), otherwise the log's — which for
                // a repository with no commits yet is exactly "does not have any commits yet".
                self.status = branch.as_ref().ok().cloned();
                self.error = branch
                    .as_ref()
                    .err()
                    .or_else(|| log.as_ref().err())
                    .map(|error| format!("{error:#}"));
                self.commits = log.unwrap_or_default();
            }
        }
        self.changes = self.state.read(cx).changes(&worktree.id).to_vec();
        // A commit the history no longer has — another worktree, another branch,
        // a rebase — has no row to be open under, and its files are another
        // history's.
        if self
            .open
            .as_ref()
            .is_some_and(|open| !self.commits.iter().any(|commit| commit.id == open.id))
        {
            self.open = None;
        }
        self.stats = git::diff_stat(&worktree.path, &worktree.base_ref).unwrap_or_default();
        self.dirty = git::worktree_changes(&worktree.path)
            .map(|changes| !changes.is_empty())
            .unwrap_or(false);
        self.base_ref = worktree.base_ref.clone();
        self.root = worktree.path.clone();
        self.read = true;
        self.stale = false;
        cx.notify();
    }

    /// Open a commit's files under its row, or close them if they are the ones
    /// already open. git is asked here, on the click, which is what keeps the
    /// fifty rows the reader did *not* open from each costing a `git show`.
    fn toggle_commit(&mut self, commit: &Commit, cx: &mut Context<Self>) {
        self.open = match &self.open {
            Some(open) if open.id == commit.id => None,
            _ => Some(OpenCommit {
                id: commit.id.clone(),
                // git's own words if it would not answer: "unknown revision",
                // rather than an empty commit it never read.
                files: git::commit_files(&self.root, &commit.id)
                    .map_err(|error| format!("{error:#}")),
            }),
        };
        cx.notify();
    }

    /// What the button does now.
    fn action(&self) -> Action {
        primary_action(self.dirty, self.status.as_ref())
    }

    /// Whether the button can be pressed: a commit needs a message, and nothing
    /// else has anything to wait for.
    fn action_ready(&self, action: Action) -> bool {
        if self.busy {
            return false;
        }
        match action {
            Action::Commit => !self.message.trim().is_empty(),
            Action::Push | Action::Publish => true,
            Action::Idle => false,
        }
    }

    /// Do what the button says, on the background executor.
    fn run_action(&mut self, action: Action, cx: &mut Context<Self>) {
        let Some(worktree) = self.state.read(cx).active().cloned() else {
            return;
        };
        let message = self.message.trim().to_string();
        let (working, clears_message) = match action {
            Action::Commit => (t!("git.committing").to_string(), true),
            Action::Push => (t!("git.pushing").to_string(), false),
            Action::Publish => (t!("git.publishing").to_string(), false),
            Action::Idle => return,
        };
        let root = worktree.path;
        self.run_operation(working, clears_message, cx, move || match action {
            Action::Commit => git::commit_all(&root, &message),
            Action::Push => git::push(&root, false),
            Action::Publish => git::push(&root, true),
            Action::Idle => Ok(String::new()),
        });
    }

    /// Do what a row of the header's menu says, on the background executor. The
    /// menu closes first: every one of these changes what is on screen, and a
    /// menu floating over the new state is the kind of thing that makes a UI
    /// feel broken.
    fn run_sync(&mut self, sync: Sync, cx: &mut Context<Self>) {
        self.menu = None;
        // A second operation while one is running is a second `git` on the same
        // repository, which is what the button's own `busy` guard is for.
        if self.busy {
            return;
        }
        let Some(worktree) = self.state.read(cx).active().cloned() else {
            return;
        };
        let root = worktree.path;
        self.run_operation(sync.working(), false, cx, move || sync.run(&root));
    }

    /// Switch this checkout to `branch`, on the background executor: the branch,
    /// the files and the history all move, so it goes through the same door a
    /// commit does. git decides whether the tree is clean enough, and its refusal
    /// is the notice.
    fn switch_branch(&mut self, branch: String, cx: &mut Context<Self>) {
        self.menu = None;
        let root = self.root.clone();
        let working = t!("git.switching", branch = branch).to_string();
        self.run_operation(working, false, cx, move || git::checkout(&root, &branch));
    }

    /// Open the header's menu where its button was clicked.
    fn open_menu(&mut self, event: &ClickEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.menu = Some(Menu::Sync(event.position()));
        // The keyboard goes to the panel so Escape can close it, which is the
        // same road the pane's menu takes.
        window.focus(&self.focus, cx);
        cx.notify();
    }

    /// Open the branch list where the branch line was clicked. Nothing while a
    /// git operation is running: it would switch under the menu.
    fn open_branch_menu(
        &mut self,
        event: &ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.busy {
            return;
        }
        // Read the branches here, on the click: the menu is the only thing that
        // needs them, and a `for-each-ref` on every tick would be a process
        // nobody asked for. A remote-tracking branch is not one to switch to —
        // checking it out would leave a detached HEAD — so only the local ones
        // are kept.
        self.branches = git::branches(&self.root)
            .unwrap_or_default()
            .into_iter()
            .filter(|branch| branch.remote.is_none())
            .collect();
        self.menu = Some(Menu::Branch(event.position()));
        // The keyboard goes to the panel so Escape can close it, the same road
        // the header's menu takes.
        window.focus(&self.focus, cx);
        cx.notify();
    }

    fn close_menu(&mut self, cx: &mut Context<Self>) {
        self.menu = None;
        cx.notify();
    }

    /// Escape closes the menu, the way it closes every other overlay. The panel
    /// itself has no other key of its own.
    fn handle_key_down(
        &mut self,
        event: &KeyDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.menu.is_some() && event.keystroke.key == "escape" {
            self.menu = None;
            cx.stop_propagation();
            cx.notify();
        }
    }

    /// Run one git operation on the background executor: the panel is busy while
    /// it runs, git's own words land in the notice, and the repository is read
    /// again afterwards — the branch, the counts and the history all moved. The
    /// one place a commit, a push and a fetch go through.
    fn run_operation(
        &mut self,
        working: String,
        clears_message: bool,
        cx: &mut Context<Self>,
        work: impl FnOnce() -> anyhow::Result<String> + Send + 'static,
    ) {
        self.busy = true;
        // Whose words these are, so that a slow push landing after a switch is
        // filed under the worktree it was about rather than the one on screen.
        let owner = self.state.read(cx).active_id().cloned();
        self.notice = Some(working);
        self.notice_worktree = owner.clone();
        cx.notify();
        let task = cx.spawn(async move |this, cx| {
            let result = cx.background_executor().spawn(async move { work() }).await;
            this.update(cx, |panel, cx| {
                panel.busy = false;
                panel.notice_worktree = owner;
                match result {
                    // git's own summary of what it did, which for a commit is
                    // the message that is now in the log.
                    Ok(output) => {
                        panel.notice = Some(output.trim().to_string());
                        panel.clear_message = clears_message;
                    }
                    Err(error) => panel.notice = Some(format!("{error:#}")),
                }
                panel.refresh(cx);
            })
            .ok();
        });
        self._task = Some(task);
    }

    /// Build the message field on the first frame with a window, and listen to
    /// it. The text lives here, the field is only where it is typed.
    fn ensure_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.clear_message
            && let Some(input) = self.input.clone()
        {
            input.update(cx, |input, cx| input.set_value("", window, cx));
            self.message.clear();
            self.clear_message = false;
        }
        if self.input.is_some() {
            return;
        }
        let input = cx.new(|cx| InputState::new(window, cx).placeholder(t!("git.message")));
        let events = cx.subscribe(&input, |this, input, _event: &InputEvent, cx| {
            let message = input.read(cx).value().to_string();
            if this.message != message {
                this.message = message;
                cx.notify();
            }
        });
        self.input = Some(input);
        self._input_events = Some(events);
    }

    /// One tick: re-read if this is the panel being looked at.
    fn poll(&mut self, cx: &mut Context<Self>) {
        let on_screen = self.drawn.is_some_and(|at| at.elapsed() < POLL);
        if on_screen {
            self.refresh(cx);
        }
    }

    /// What the last read found, for a test that asserts on the read rather than
    /// on the drawing.
    #[cfg(test)]
    pub fn status(&self) -> Option<&BranchStatus> {
        self.status.as_ref()
    }

    #[cfg(test)]
    pub fn commits(&self) -> &[Commit] {
        &self.commits
    }

    #[cfg(test)]
    pub fn changes(&self) -> &[FileChange] {
        &self.changes
    }

    /// What is in the message field, which is what the Commit button reads.
    #[cfg(test)]
    pub fn message(&self) -> &str {
        &self.message
    }

    /// The two sentences the panel leads with, for a test that reads them
    /// without hunting for the label.
    #[cfg(test)]
    pub fn headline(&self) -> String {
        summary(self.status.as_ref(), self.changes.len(), &self.base_ref).0
    }

    fn refresh_button(&self, colors: &ColorTokens, cx: &mut Context<Self>) -> AnyElement {
        div()
            .id("git-refresh")
            .test_support()
            .aria_label(t!("git.refresh"))
            .h_flex()
            .items_center()
            .justify_center()
            .size(px(22.))
            .rounded_sm()
            .cursor_pointer()
            .text_color(colors.muted_foreground)
            .hover(|el| el.bg(colors.muted))
            .child(Icon::new(IconName::RefreshCw).size(px(14.)))
            .on_click(cx.listener(|this, _event, _window, cx| this.refresh(cx)))
            .into_any_element()
    }

    /// The way into the graph: the commits section is this panel's history, and
    /// the graph is the same history across every branch, in the room a window
    /// has for it. Drawn in the section's own header, at the trailing end of the
    /// row that names the commits it is about.
    fn graph_button(&self, colors: &ColorTokens, cx: &mut Context<Self>) -> AnyElement {
        div()
            .id("git-graph-open")
            .test_support()
            .aria_label(t!("git.graph"))
            .h_flex()
            .items_center()
            .justify_center()
            .size(px(16.))
            .rounded_sm()
            .cursor_pointer()
            .text_color(colors.muted_foreground)
            .hover(|el| el.bg(colors.muted))
            .child(Icon::new(IconName::Maximize).size(px(12.)))
            .on_click(cx.listener(|this, _event, window, cx| {
                // The header's own click folds the section; this one opens the
                // graph. Both would happen without this.
                cx.stop_propagation();
                // Focused first, for the same reason a file row is: the action
                // only reaches the workspace's element listener from inside its
                // subtree, and this click is the only way to the graph.
                window.focus(&this.focus, cx);
                window.dispatch_action(Box::new(crate::state::gitgraph::Open), cx);
            }))
            .into_any_element()
    }

    /// The header's menu button: fetching, pulling and pushing. It sits beside
    /// the fresh-read button because a reader who wants the remote's history, or
    /// wants to send their own, is looking at that end of the header.
    fn sync_button(&self, colors: &ColorTokens, cx: &mut Context<Self>) -> AnyElement {
        div()
            .id("git-sync")
            .test_support()
            .aria_label(t!("git.sync"))
            .h_flex()
            .items_center()
            .justify_center()
            .size(px(22.))
            .rounded_sm()
            .cursor_pointer()
            .text_color(colors.muted_foreground)
            .hover(|el| el.bg(colors.muted))
            .child(Icon::new(IconName::Ellipsis).size(px(14.)))
            .on_click(cx.listener(|this, event, window, cx| this.open_menu(event, window, cx)))
            .into_any_element()
    }

    /// The operations, in a layer above the panel: a menu drawn inside the
    /// panel's own column would be clipped at the column's edge, which is
    /// exactly where a menu in a narrow sidebar has to go.
    fn menu_overlay(
        &self,
        window_size: Size<Pixels>,
        colors: &ColorTokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(menu) = self.menu.as_ref() else {
            return div().into_any_element();
        };
        let at = menu.at();
        let panel = match menu {
            Menu::Sync(_) => self.sync_menu(colors, cx),
            Menu::Branch(_) => self.branch_menu(colors, cx),
        };
        let this = cx.entity().downgrade();

        // The scrim: a click anywhere outside the menu, on either button, closes
        // it — which is what makes the menu feel like a menu.
        let dismiss = this.clone();
        deferred(
            // The scrim needs a size of its own: an `anchored` box is as big as
            // its child, and a `size_full` child inside it is a percentage of
            // nothing — a scrim written that way covers no pixels and closes
            // nothing.
            anchored().position(point(px(0.), px(0.))).child(
                div()
                    .id("git-sync-scrim")
                    .test_support()
                    .w(window_size.width)
                    .h(window_size.height)
                    // Blocks the mouse from everything behind it: the rows under
                    // the menu are not hovered and not clicked while it is up.
                    .occlude()
                    .on_mouse_down(MouseButton::Left, move |_event, _window, cx| {
                        dismiss.update(cx, |panel, cx| panel.close_menu(cx)).ok();
                    })
                    .on_mouse_down(MouseButton::Right, {
                        let dismiss = this.clone();
                        move |_event, _window, cx| {
                            dismiss.update(cx, |panel, cx| panel.close_menu(cx)).ok();
                        }
                    })
                    .child(
                        anchored()
                            .position(at)
                            .anchor(Anchor::TopLeft)
                            .snap_to_window_with_margin(px(8.))
                            .child(panel),
                    ),
            ),
        )
        .with_priority(gpui_kit::base::POPUP_PRIORITY)
        .into_any_element()
    }

    /// The header's operations as a menu: the ones the branch can run.
    fn sync_menu(&self, colors: &ColorTokens, cx: &mut Context<Self>) -> AnyElement {
        let this = cx.entity().downgrade();
        let mut panel = div()
            .id("git-sync-menu")
            .test_support()
            .v_flex()
            .w(px(200.))
            .p_1()
            .rounded_md()
            .border_1()
            .border_color(colors.border)
            // Solid at any window opacity, like every menu.
            .bg(crate::settings_view::solid(colors.surface))
            .text_xs()
            // The whole panel is the menu, its padding and the pixels between
            // two rows included: a click there is a click on the menu.
            .occlude()
            .on_scroll_wheel(|_event, _window, cx| cx.stop_propagation());
        for sync in Sync::ALL
            .into_iter()
            // Only the ones that can run: a branch git has no upstream for is not
            // one to pull or push, and the row that is offered instead is Publish,
            // which is what makes the upstream.
            .filter(|sync| sync.enabled(self.status.as_ref()))
        {
            let view = this.clone();
            panel = panel.child(
                div()
                    .id(sync.id())
                    .test_support()
                    .aria_label(SharedString::from(sync.label()))
                    .h_flex()
                    .items_center()
                    .justify_between()
                    .gap_4()
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .cursor_pointer()
                    .hover(|el| el.bg(colors.muted))
                    .on_click(move |_event, _window, cx| {
                        view.update(cx, |panel, cx| panel.run_sync(sync, cx)).ok();
                    })
                    .child(SharedString::from(sync.label())),
            );
        }
        panel.into_any_element()
    }

    /// Every local branch as a menu, with the one this checkout is on marked:
    /// picking another switches to it. A remote-tracking branch is not here —
    /// switching to it would leave a detached HEAD — and the current one has
    /// nothing to switch to, so it only carries the mark.
    fn branch_menu(&self, colors: &ColorTokens, cx: &mut Context<Self>) -> AnyElement {
        let this = cx.entity().downgrade();
        let current = self
            .status
            .as_ref()
            .and_then(|status| status.branch.clone());
        let mut panel = div()
            .id("git-branch-menu")
            .test_support()
            .v_flex()
            // Wider than the sync menu: a branch name is long (`feat/…-solana`),
            // and a picker that cuts every one of them to the same stub is a
            // picker nobody can choose from.
            .w(px(260.))
            .p_1()
            .rounded_md()
            .border_1()
            .border_color(colors.border)
            // Solid at any window opacity, like every menu.
            .bg(crate::settings_view::solid(colors.surface))
            .text_xs()
            // The whole panel is the menu, its padding and the pixels between
            // two rows included: a click there is a click on the menu.
            .occlude()
            .on_scroll_wheel(|_event, _window, cx| cx.stop_propagation());
        for branch in &self.branches {
            let chosen = current.as_deref() == Some(branch.name.as_str());
            let name = branch.name.clone();
            let view = this.clone();
            panel = panel.child(
                div()
                    .id(SharedString::from(format!("git-switch-{}", branch.name)))
                    .test_support()
                    .aria_label(SharedString::from(
                        t!("git.switch_branch", branch = branch.name).to_string(),
                    ))
                    .h_flex()
                    .items_center()
                    .justify_between()
                    .gap_4()
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .text_color(if chosen {
                        colors.primary
                    } else {
                        colors.foreground
                    })
                    .when(!chosen, |el| {
                        el.cursor_pointer().hover(|el| el.bg(colors.muted))
                    })
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .child(SharedString::from(branch.name.clone())),
                    )
                    .when(chosen, |el| {
                        el.child(
                            div()
                                .flex_shrink_0()
                                .child(Icon::new(IconName::Check).size(px(12.))),
                        )
                    })
                    .when(!chosen, |el| {
                        el.on_click(move |_event, _window, cx| {
                            view.update(cx, |panel, cx| panel.switch_branch(name.clone(), cx))
                                .ok();
                        })
                    }),
            );
        }
        panel.into_any_element()
    }

    /// The branch, what it moved, and the upstream it is measured against.
    fn branch_block(
        &self,
        colors: &ColorTokens,
        accents: Accents,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut block = div()
            .id("git-branch")
            .test_support()
            .v_flex()
            .gap_1()
            .w_full()
            .px_2()
            .pb_2()
            .border_b_1()
            .border_color(colors.border);
        let Some(status) = &self.status else {
            return block.into_any_element();
        };
        let (added, deleted) = self.totals();
        block = block.child(
            div()
                .h_flex()
                .items_center()
                .justify_between()
                .gap_2()
                .w_full()
                .child(
                    div()
                        .id("git-branch-name")
                        .test_support()
                        .aria_label(SharedString::from(
                            t!("git.switch_branch", branch = status.name()).to_string(),
                        ))
                        .h_flex()
                        .items_center()
                        .gap_1()
                        .min_w_0()
                        .overflow_hidden()
                        .rounded_sm()
                        .cursor_pointer()
                        .hover(|el| el.bg(colors.muted))
                        .child(
                            div()
                                .min_w_0()
                                .overflow_hidden()
                                .text_ellipsis()
                                .text_sm()
                                .font_weight(FontWeight::MEDIUM)
                                .child(SharedString::from(status.name().to_string())),
                        )
                        // The chevron says the name is a control, not a label.
                        .child(Icon::new(IconName::ChevronDown).size(px(12.)))
                        .on_click(cx.listener(|this, event, window, cx| {
                            this.open_branch_menu(event, window, cx)
                        })),
                )
                .when(added > 0 || deleted > 0, |el| {
                    // `+85 -9`, the two numbers a reader checks before deciding
                    // whether to commit: how much went in and how much came out.
                    el.child(
                        div()
                            .h_flex()
                            .items_center()
                            .gap_1()
                            .flex_shrink_0()
                            .text_xs()
                            .child(
                                div()
                                    .text_color(accents.added)
                                    .child(SharedString::from(format!("+{added}"))),
                            )
                            .child(
                                div()
                                    .text_color(accents.removed)
                                    .child(SharedString::from(format!("-{deleted}"))),
                            ),
                    )
                }),
        );
        if let Some(upstream) = &status.upstream {
            block = block.child(
                div()
                    .h_flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .w_full()
                    .text_xs()
                    .text_color(colors.muted_foreground)
                    .child(
                        div()
                            .h_flex()
                            .items_center()
                            .gap_1()
                            .min_w_0()
                            .overflow_hidden()
                            .child(Icon::new(IconName::ArrowRight).size(px(12.)))
                            .child(
                                div()
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .child(SharedString::from(upstream.clone())),
                            ),
                    )
                    .when(status.ahead > 0, |el| {
                        // The `↑3`: how many commits the upstream has not
                        // got, which is what the Push button would send.
                        el.child(
                            div()
                                .flex_shrink_0()
                                .child(SharedString::from(format!("↑{}", status.ahead))),
                        )
                    }),
            );
        }
        block.into_any_element()
    }

    /// The two numbers of the branch line: every change on the branch added up.
    fn totals(&self) -> (u32, u32) {
        self.stats
            .values()
            .fold((0, 0), |(added, deleted), (a, d)| (added + a, deleted + d))
    }

    /// The message box and the one button, which is what the panel is *for*.
    fn action_block(&self, colors: &ColorTokens, cx: &mut Context<Self>) -> AnyElement {
        let action = self.action();
        let ready = self.action_ready(action);
        let mut block = div()
            .id("git-action")
            .test_support()
            .v_flex()
            .gap_1()
            .w_full()
            .px_2()
            .py_2();
        // The message is only a thing to type when there is something to commit:
        // hidden on a clean tree, where the button is a push.
        if self.dirty {
            block = block.when_some(self.input.clone(), |el, input| {
                el.child(
                    div()
                        .id("git-message")
                        .test_support()
                        .aria_label(t!("git.commit_message"))
                        .w_full()
                        .child(Input::new(&input)),
                )
            });
        }
        block = block.child(
            div()
                .id("git-primary")
                .test_support()
                .aria_label(SharedString::from(action.label()))
                .h_flex()
                .items_center()
                .justify_center()
                .gap_1()
                .w_full()
                .py_1()
                .rounded_md()
                .border_1()
                .border_color(colors.border)
                .text_xs()
                .when(ready, |el| {
                    el.cursor_pointer()
                        .text_color(colors.foreground)
                        .hover(|el| el.bg(colors.muted))
                })
                .when(!ready, |el| el.text_color(colors.muted_foreground))
                .child(Icon::new(action.icon()).size(px(13.)))
                .child(action.label())
                .on_click(cx.listener(move |this, _event, _window, cx| {
                    if this.action_ready(action) {
                        this.run_action(action, cx);
                    }
                })),
        );
        if let Some(notice) = self.notice.clone() {
            block = block.child(
                div()
                    .id("git-notice")
                    .test_support()
                    .role(Role::Status)
                    .aria_label(SharedString::from(notice.clone()))
                    .w_full()
                    .overflow_hidden()
                    .text_ellipsis()
                    .text_xs()
                    .text_color(colors.muted_foreground)
                    .child(SharedString::from(notice)),
            );
        }
        block.into_any_element()
    }

    /// The changes on the branch: what the commit would hold, and what the base
    /// ref does not have.
    fn changes_block(
        &self,
        colors: &ColorTokens,
        accents: Accents,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let block = div()
            .id("git-changes")
            .test_support()
            .v_flex()
            // Fills the space between the message box and the history: this is
            // the list a reader works in, and the empty part of it is where the
            // next change will appear.
            .flex_1()
            .min_h_0()
            .w_full()
            .border_t_1()
            .border_color(colors.border)
            .pt_1()
            .child(
                self.section_header(
                    Section::Changes,
                    t!("git.committed_on", count = self.changes.len()).to_string(),
                    self.files_collapsed,
                    Some(
                        div()
                            .id("git-view-all")
                            .test_support()
                            .aria_label(t!("git.view_all"))
                            .flex_shrink_0()
                            .cursor_pointer()
                            .text_color(colors.muted_foreground)
                            .hover(|el| el.text_color(colors.foreground))
                            .child(t!("git.view_all"))
                            // The header's click folds the section; this one opens the
                            // diff tab. Both would happen without this.
                            .on_click(cx.listener(|this, _event, window, cx| {
                                cx.stop_propagation();
                                window.focus(&this.focus, cx);
                                window.dispatch_action(Box::new(diff::Open), cx);
                            }))
                            .into_any_element(),
                    ),
                    colors,
                    cx,
                ),
            );
        if self.files_collapsed {
            return block.into_any_element();
        }
        // The rows scroll inside the section rather than stretching the panel:
        // eighteen files in a 260 px column is most of a screen, and the history
        // below is not worth less than the files above.
        let mut rows = div()
            .id("git-change-rows")
            .test_support()
            .v_flex()
            .flex_1()
            .min_h_0()
            .w_full()
            .overflow_y_scroll();
        for change in self.changes.iter().take(FILES) {
            rows = rows.child(self.file_row(change, colors, accents, cx));
        }
        if self.changes.len() > FILES {
            rows = rows.child(
                div()
                    .px_2()
                    .py_1()
                    .text_xs()
                    .text_color(colors.muted_foreground)
                    .child(SharedString::from(
                        t!("git.more_view_all", count = self.changes.len() - FILES).to_string(),
                    )),
            );
        }
        block.child(rows).into_any_element()
    }

    /// A section's header: the chevron that folds it, its title, and whatever the
    /// caller puts at the trailing end.
    fn section_header(
        &self,
        section: Section,
        title: String,
        collapsed: bool,
        trailing: Option<AnyElement>,
        colors: &ColorTokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut header = div()
            .id(SharedString::from(format!("{}-header", section.id())))
            .test_support()
            .aria_label(SharedString::from(title.clone()))
            .h_flex()
            .items_center()
            .justify_between()
            .gap_2()
            .w_full()
            .px_2()
            .py_1()
            .cursor_pointer()
            .text_xs()
            .text_color(colors.muted_foreground)
            .hover(|el| el.text_color(colors.foreground))
            .child(
                div()
                    .h_flex()
                    .items_center()
                    .gap_1()
                    .min_w_0()
                    .overflow_hidden()
                    .child(
                        Icon::new(if collapsed {
                            IconName::ChevronRight
                        } else {
                            IconName::ChevronDown
                        })
                        .size(px(12.)),
                    )
                    .child(
                        div()
                            .overflow_hidden()
                            .text_ellipsis()
                            .child(SharedString::from(title)),
                    ),
            )
            .on_click(cx.listener(move |this, _event, _window, cx| {
                match section {
                    Section::Changes => this.files_collapsed = !this.files_collapsed,
                    Section::Commits => this.commits_collapsed = !this.commits_collapsed,
                }
                cx.notify();
            }));
        if let Some(trailing) = trailing {
            header = header.child(trailing);
        }
        header.into_any_element()
    }

    /// One commit: the dot on the history's spine, the message, the names
    /// pointing at it — and the files it changed, which are what a click on the
    /// row opens underneath.
    fn commit_row(
        &self,
        commit: &Commit,
        published: bool,
        last: bool,
        colors: &ColorTokens,
        accents: Accents,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let dot = if published {
            // On the upstream already: the quiet colour, because this part of
            // the history is done.
            colors.muted_foreground
        } else {
            // Not upstream yet, which is what the Push button would send, and
            // what the button above is counting.
            accents.outgoing
        };
        let mut spine = div()
            .absolute()
            .left(px(8.))
            .top(px(0.))
            .h_full()
            .w(px(8.))
            .child(
                div()
                    .absolute()
                    .left(px(3.5))
                    .top(px(0.))
                    .h_full()
                    .w(px(1.))
                    .bg(colors.border),
            );
        if last {
            // The history ends here, so the line does too: from the middle of the
            // row down, the line is the surface again. Drawn *under* the dot —
            // the dot is painted after it — so cutting the line at the dot's own
            // middle leaves a whole dot and a line that stops at it, whatever
            // height the row turned out to be.
            spine = spine.child(
                div()
                    .absolute()
                    .left(px(3.5))
                    .top(relative(0.5))
                    .h_full()
                    .w(px(1.))
                    .bg(colors.surface),
            );
        }
        spine = spine.child(
            // The dot is centred by the layout and not by a number: a row is as
            // tall as the text in it, and a dot placed at a fixed `top` sits
            // above the middle of the line it belongs to — which is what it did
            // at 6 px, on rows that are 28.
            div()
                .absolute()
                .left(px(0.5))
                .top(px(0.))
                .size_full()
                .v_flex()
                .items_center()
                .justify_center()
                .child(
                    div()
                        .id(SharedString::from(format!("commit-dot-{}", commit.short)))
                        .test_support()
                        .size(px(7.))
                        .rounded_full()
                        .bg(dot),
                ),
        );
        // One line: the message gives up its width so the names pointing at the commit keep theirs.
        let mut body = div()
            .flex_1()
            .min_w_0()
            .h_flex()
            .items_center()
            .gap_1()
            .text_xs()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .text_ellipsis()
                    .child(SharedString::from(commit.summary.clone())),
            );
        for (index, name) in commit.refs.iter().enumerate() {
            body = body.child(ref_badge(name, index == 0, colors));
        }
        let wanted = commit.clone();
        div()
            .id(SharedString::from(format!("commit-{}", commit.short)))
            .test_support()
            .aria_label(SharedString::from(commit.summary.clone()))
            .relative()
            .h_flex()
            .items_center()
            .gap_1()
            .w_full()
            .pl(px(22.))
            .pr_2()
            .py_1()
            .cursor_pointer()
            .hover(|el| el.bg(colors.muted))
            .child(spine)
            .child(body)
            .on_click(cx.listener(move |this, _event, _window, cx| this.toggle_commit(&wanted, cx)))
            .into_any_element()
    }

    /// What an open commit's row opened onto: who made it and when, then the
    /// files it changed. The spine runs through it, because the history below it
    /// is still the same history, and the rows under the author are not
    /// clickable: the click a changed file answers to everywhere else in this
    /// panel is the diff tab, which shows the worktree's changes against the
    /// branch's base — a different comparison from this commit's own.
    fn open_commit_block(
        &self,
        commit: &Commit,
        files: &Result<Vec<FileChange>, String>,
        colors: &ColorTokens,
    ) -> AnyElement {
        let block = div()
            .id(SharedString::from(format!("commit-open-{}", commit.short)))
            .test_support()
            .relative()
            .v_flex()
            .w_full()
            .pb_1()
            // The line the rows above and below are strung on, at the x their
            // own spines put it: 8 px in, and 3.5 across.
            .child(
                div()
                    .id(SharedString::from(format!("commit-spine-{}", commit.short)))
                    .test_support()
                    .absolute()
                    .left(px(11.5))
                    .top(px(0.))
                    .h_full()
                    .w(px(1.))
                    .bg(colors.border),
            )
            .child(under_commit(
                SharedString::from(format!("commit-author-{}", commit.short)),
                SharedString::from(commit_line(commit)),
                colors.muted_foreground,
            ));
        match files {
            Err(error) => block.child(under_commit(
                SharedString::from(format!("commit-error-{}", commit.short)),
                SharedString::from(error.clone()),
                colors.destructive,
            )),
            Ok(files) if files.is_empty() => block.child(under_commit(
                SharedString::from(format!("commit-empty-{}", commit.short)),
                SharedString::from(t!("git.commit_empty").to_string()),
                colors.muted_foreground,
            )),
            Ok(files) => {
                let mut rows = block;
                for change in files.iter().take(FILES) {
                    rows = rows.child(commit_file_row(
                        &commit.id,
                        change,
                        colors,
                        self.focus.clone(),
                    ));
                }
                // The ceiling the changes section has, for the same reason: an
                // initial commit holds every file in the repository, and the
                // history they are opened under is worth more than the tail of a
                // list nobody reads to the end of.
                if files.len() > FILES {
                    rows = rows.child(under_commit(
                        SharedString::from(format!("commit-more-{}", commit.short)),
                        SharedString::from(
                            t!("git.more_files", count = files.len() - FILES).to_string(),
                        ),
                        colors.muted_foreground,
                    ));
                }
                rows
            }
        }
        .into_any_element()
    }
}

/// One line under an open commit: who made it and how long ago, git's own
/// complaint about it, or how many files are not listed. Indented past the spine
/// the rows all hang on, in the small print the panel's other asides use.
fn under_commit(id: SharedString, text: SharedString, colour: Hsla) -> AnyElement {
    div()
        .id(id)
        .test_support()
        .pl(px(30.))
        .pr_2()
        .text_xs()
        .text_color(colour)
        .child(text)
        .into_any_element()
}

/// What an open commit's author line says: who made it, and how long ago. The age
/// is worked out here rather than taken from git, whose `%ar` says "2 hours ago"
/// in English whatever language the app is in.
fn commit_line(commit: &Commit) -> String {
    format!(
        "{} · {}",
        commit.author,
        session::relative_time(session::now_ms(), commit.at.saturating_mul(1000))
    )
}

/// One file of an open commit: what it is called, where it lives, and git's
/// letter for it. The changes section's row without the things that belong to
/// the worktree — a `+N -M` these numbers are not, and the jump to the editor —
/// because what a click under a commit shows is that commit's own change to the
/// file, which is the comparison `diff::OpenCommitFile` carries.
///
/// Free rather than a method: the graph draws these rows too, and `focus` is how
/// the caller's action reaches the workspace — the click focuses the view it is
/// inside, and the dispatch bubbles up from there.
pub(crate) fn commit_file_row(
    commit: &str,
    change: &FileChange,
    colors: &ColorTokens,
    focus: FocusHandle,
) -> AnyElement {
    let path = std::path::Path::new(&change.path);
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| change.path.clone());
    let dir = path
        .parent()
        .map(|dir| dir.to_string_lossy().into_owned())
        .unwrap_or_default();
    let (id, file) = (commit.to_string(), change.path.clone());
    div()
        .id(SharedString::from(format!("commit-file-{}", change.path)))
        .test_support()
        // The letter is part of the name, the way it is in the changes section:
        // a screen reader cannot see the colour, and neither can a test.
        .aria_label(SharedString::from(format!(
            "{} {}",
            change.status, change.path
        )))
        .h_flex()
        .items_center()
        .gap_1()
        .w_full()
        .pl(px(30.))
        .pr_2()
        .py_0p5()
        .text_xs()
        .cursor_pointer()
        .hover(|el| el.bg(colors.muted))
        .child(div().flex_shrink_0().child(SharedString::from(name)))
        .when(!dir.is_empty(), |el| {
            el.child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .text_ellipsis()
                    .text_color(colors.muted_foreground)
                    .child(SharedString::from(dir)),
            )
        })
        .child(
            div()
                .flex_shrink_0()
                .text_color(colors.muted_foreground)
                .child(SharedString::from(change.status.to_string())),
        )
        .on_click(move |_event, window, cx| {
            window.focus(&focus, cx);
            window.dispatch_action(
                Box::new(diff::OpenCommitFile {
                    commit: id.clone(),
                    path: file.clone(),
                }),
                cx,
            );
        })
        .into_any_element()
}

/// Which of the panel's two lists a header belongs to. An enum rather than the
/// id string, because the click handler has to know *which* flag it flips.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Section {
    Changes,
    Commits,
}

impl Section {
    /// The test-support id of the section's box, which is also what the header
    /// and its rows hang off.
    fn id(self) -> &'static str {
        match self {
            Section::Changes => "git-changes",
            Section::Commits => "git-commits",
        }
    }
}

/// The button's ladder, from what the worktree has: uncommitted work is a commit, a clean tree
/// with commits the upstream has not got is a push, and a branch git does not know upstream is
/// published instead.
fn primary_action(dirty: bool, status: Option<&BranchStatus>) -> Action {
    if dirty {
        return Action::Commit;
    }
    match status.map(|status| (status.ahead, status.upstream.as_deref())) {
        Some((ahead, _)) if ahead > 0 => match status.and_then(|status| status.upstream.as_deref())
        {
            Some(_) => Action::Push,
            None => Action::Publish,
        },
        _ => Action::Idle,
    }
}

/// The letter is the one the diff tab and the file tree already colour files by
/// (`status_color` in `main.rs`), so a file reads the same in all three places.
impl GitPanel {
    /// One changed file: what it is called, where it lives, how much it moved, and git's letter for
    /// it.
    fn file_row(
        &self,
        change: &FileChange,
        colors: &ColorTokens,
        accents: Accents,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let stat = self.stats.get(&change.path);
        let path = std::path::Path::new(&change.path);
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| change.path.clone());
        let dir = path
            .parent()
            .map(|dir| dir.to_string_lossy().into_owned())
            .unwrap_or_default();
        let file = change.path.clone();
        let absolute = self.root.join(&change.path);
        let open_label = t!("git.open_in_editor", file = file).to_string();
        div()
            .id(SharedString::from(format!("git-file-{}", change.path)))
            .test_support()
            // The status letter is part of the name: a screen reader cannot see
            // the colour it is drawn in, and neither can a test.
            .aria_label(SharedString::from(format!(
                "{} {}",
                change.status, change.path
            )))
            .h_flex()
            .items_center()
            .gap_1()
            .w_full()
            .px_2()
            .py_0p5()
            .text_xs()
            .cursor_pointer()
            .hover(|el| el.bg(colors.muted))
            .child(div().flex_shrink_0().child(SharedString::from(name)))
            .when(!dir.is_empty(), |el| {
                el.child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .text_ellipsis()
                        .text_color(colors.muted_foreground)
                        .child(SharedString::from(dir)),
                )
            })
            .when_some(stat.copied(), |el, (added, deleted)| {
                // An untracked file has no counts: git has nothing to compare it
                // against, which is why its row shows only the letter.
                el.child(
                    div()
                        .flex_shrink_0()
                        .text_color(accents.added)
                        .child(SharedString::from(format!("+{added}"))),
                )
                .child(
                    div()
                        .flex_shrink_0()
                        .text_color(accents.removed)
                        .child(SharedString::from(format!("-{deleted}"))),
                )
            })
            // The one thing the row cannot do by being clicked: the other half of
            // "show me this change", which is going to the file itself. The diff
            // tab's own rows carried this before its list moved here.
            .child(
                div()
                    .id(SharedString::from(format!("git-open-{}", change.path)))
                    .test_support()
                    .aria_label(SharedString::from(open_label))
                    .flex_shrink_0()
                    .px_1()
                    .rounded_sm()
                    .text_color(colors.muted_foreground)
                    .hover(|el| el.bg(colors.border))
                    .child("\u{2197}")
                    .on_click(cx.listener(move |_this, _event, window, cx| {
                        // The row's click shows the diff; this one edits the file, so
                        // it must not do both.
                        cx.stop_propagation();
                        window.dispatch_action(
                            Box::new(editor::Open {
                                path: absolute.clone(),
                            }),
                            cx,
                        );
                    })),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .text_color(colors.muted_foreground)
                    .child(SharedString::from(change.status.to_string())),
            )
            .on_click(cx.listener(move |this, _event, window, cx| {
                // Focused first: `diff::OpenFile` needs a window, and an action only
                // reaches the workspace's element listener from inside its subtree.
                window.focus(&this.focus, cx);
                window.dispatch_action(Box::new(diff::OpenFile { path: file.clone() }), cx);
            }))
            .into_any_element()
    }
}

/// The panel's two sentences: what is on the branch, and how it stands against its upstream.
fn summary(status: Option<&BranchStatus>, changes: usize, base_ref: &str) -> (String, String) {
    let Some(status) = status else {
        return (
            t!("git.no_branch").to_string(),
            t!("git.not_on_branch").to_string(),
        );
    };
    if changes == 0 {
        return (
            t!("git.no_changes").to_string(),
            t!("git.clean_has", summary = status.summary()).to_string(),
        );
    }
    let headline = if changes == 1 {
        t!("git.one_changed_file").to_string()
    } else {
        t!("git.changed_files", count = changes).to_string()
    };
    (
        headline,
        t!("git.against", base = base_ref, summary = status.summary()).to_string(),
    )
}

/// One ref name as a pill: the first one is the branch this checkout is on, the
/// rest are tags and other branches — which is the reading of `%D` here, and
/// the reason the first is the brighter of the two colours.
fn ref_badge(name: &str, first: bool, colors: &ColorTokens) -> AnyElement {
    div()
        .flex_shrink_0()
        .px_1()
        .rounded_full()
        .border_1()
        .border_color(if first { colors.primary } else { colors.border })
        .text_color(if first {
            colors.primary
        } else {
            colors.muted_foreground
        })
        .text_xs()
        .child(SharedString::from(name.to_string()))
        .into_any_element()
}

impl Render for GitPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.ensure_input(window, cx);
        // What makes "on screen" observable to the tick: see [`Self::poll`].
        self.drawn = Some(Instant::now());
        // A panel is only rendered while it is the sidebar's selected one, so
        // this frame is when git is asked — the first look, or the first look
        // since something moved under it.
        if !self.read || self.stale {
            self.refresh(cx);
        }
        let colors = Theme::global(cx).tokens.colors;
        let accents = Accents::of(cx);
        let header = div()
            .id("git-header")
            .test_support()
            .h_flex()
            .items_center()
            .justify_between()
            .px_2()
            .h(px(HEADER_HEIGHT))
            .child(
                div()
                    .text_xs()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(colors.muted_foreground)
                    .child(t!("git.source_control")),
            )
            .child(
                div()
                    .h_flex()
                    .items_center()
                    .gap_1()
                    .child(self.sync_button(&colors, cx))
                    .child(self.refresh_button(&colors, cx)),
            );

        let (headline, detail) = summary(self.status.as_ref(), self.changes.len(), &self.base_ref);
        // No scroll of its own: each list scrolls inside itself, and the panel's
        // shape — branch, message, button, list, footer — holds while they do.
        let body = div()
            .id("git-body")
            .test_support()
            .v_flex()
            .flex_1()
            .min_h_0()
            .w_full()
            .child(self.branch_block(&colors, accents, cx))
            // The two sentences come before the message box: what the branch has,
            // then what to do about it.
            .child(
                div()
                    .id("git-summary")
                    .test_support()
                    .v_flex()
                    .gap_1()
                    .w_full()
                    .px_2()
                    .pb_2()
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(SharedString::from(headline)),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(colors.muted_foreground)
                            .child(SharedString::from(detail)),
                    ),
            )
            .child(self.action_block(&colors, cx))
            // With nothing changed the section is not drawn at all, but its space
            // is: the history stays against the bottom edge either way.
            .child(if self.changes.is_empty() {
                div().flex_1().into_any_element()
            } else {
                self.changes_block(&colors, accents, cx)
            })
            .child(self.history(&colors, accents, cx));

        // `flex_1` and not `h_full`: the panel is a flex item under the activity strip, and `height:
        // 100%` there resolves against the sidebar's whole height — 36 px more than the space
        // it is actually given, which puts its bottom edge (and the footer pinned to it) below
        // the window.
        let panel = div()
            .id("git-panel")
            .test_support()
            .track_focus(&self.focus)
            // The menu is the only thing this panel listens for a key about.
            .on_key_down(cx.listener(Self::handle_key_down))
            .v_flex()
            .w_full()
            .flex_1()
            .min_h_0()
            .child(header)
            .child(body);
        if self.menu.is_some() {
            return panel
                .child(self.menu_overlay(window.viewport_size(), &colors, cx))
                .into_any_element();
        }
        panel.into_any_element()
    }
}

impl GitPanel {
    /// The history: the commits section, the outgoing work above it, and one row
    /// per commit.
    fn history(
        &self,
        colors: &ColorTokens,
        accents: Accents,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let title = match self.commits.len() {
            // The section header says "50 +" when the cap bit.
            0 => t!("git.commits").to_string(),
            count if count == COMMITS => t!("git.commits_count_more", count = count).to_string(),
            count => t!("git.commits_count", count = count).to_string(),
        };
        let history = div()
            .id("git-history")
            .test_support()
            .v_flex()
            // The footer: it keeps its height and the changes give up theirs, so
            // a folded history is a header against the bottom edge of the panel
            // and not a header floating in the middle of it.
            .flex_shrink_0()
            .w_full()
            .border_t_1()
            .border_color(colors.border)
            .pt_1()
            .child(self.section_header(
                Section::Commits,
                title,
                self.commits_collapsed,
                Some(self.graph_button(colors, cx)),
                colors,
                cx,
            ));
        if self.commits_collapsed {
            return history.into_any_element();
        }
        if self.commits.is_empty() {
            let empty = match &self.error {
                Some(error) => error.clone(),
                None if !self.read => t!("git.reading").to_string(),
                None => t!("git.no_commits").to_string(),
            };
            return history
                .child(
                    div()
                        .px_2()
                        .py_1()
                        .text_xs()
                        .text_color(colors.muted_foreground)
                        .child(SharedString::from(empty)),
                )
                .into_any_element();
        }

        // The boundary between what the upstream has and what it does not: every commit from the one
        // the upstream points at downwards is published, and the ones above it are the push's
        // payload.
        let upstream = self
            .status
            .as_ref()
            .and_then(|status| status.upstream.clone());
        let first_published = upstream.as_ref().and_then(|upstream| {
            self.commits
                .iter()
                .position(|commit| commit.refs.iter().any(|name| name == upstream))
        });

        // The rows scroll inside the section, for the same reason the files do:
        let mut rows = div()
            .id("git-commit-rows")
            .test_support()
            .v_flex()
            .w_full()
            .max_h(px(SECTION_MAX_HEIGHT))
            .overflow_y_scroll();
        // The first row: the work that has not been committed yet, drawn
        // as a hollow ring above the newest commit because it is not one.
        if self.dirty {
            rows = rows.child(hollow_row(colors));
        }
        let last = self.commits.len() - 1;
        for (index, commit) in self.commits.iter().enumerate() {
            let published = first_published.is_some_and(|first| index >= first);
            let open = self
                .open
                .as_ref()
                .filter(|open| open.id == commit.id)
                .map(|open| &open.files);
            // The last row's spine is cut off under its dot because the history
            // ends there — unless the files under it are open, in which case the
            // history carries on past it.
            rows = rows.child(self.commit_row(
                commit,
                published,
                index == last && open.is_none(),
                colors,
                accents,
                cx,
            ));
            if let Some(files) = open {
                rows = rows.child(self.open_commit_block(commit, files, colors));
            }
        }
        history.child(rows).into_any_element()
    }
}

/// "Outgoing Changes": the row above the newest commit for the work in the
/// worktree, which has no commit of its own yet — a hollow ring rather than a
/// dot, and an honest name for what it is.
fn hollow_row(colors: &ColorTokens) -> AnyElement {
    div()
        .id("git-outgoing")
        .test_support()
        .aria_label(t!("git.outgoing"))
        .relative()
        .h_flex()
        .items_center()
        .h(px(24.))
        .w_full()
        .pl(px(22.))
        .pr_2()
        .child(
            div()
                .absolute()
                .left(px(8.))
                .top(px(0.))
                .h_full()
                .w(px(8.))
                .child(
                    div()
                        .absolute()
                        .left(px(3.5))
                        .top(px(12.))
                        .h_full()
                        .w(px(1.))
                        .bg(colors.border),
                )
                .child(
                    // Centred by the layout, like the commits' own dots: the row
                    // is a fixed 24 px and the text in it is centred too, so the
                    // ring has to be.
                    div()
                        .absolute()
                        .left(px(0.5))
                        .top(px(0.))
                        .size_full()
                        .v_flex()
                        .items_center()
                        .justify_center()
                        .child(
                            div()
                                .id("git-outgoing-dot")
                                .test_support()
                                .size(px(7.))
                                .rounded_full()
                                .border_1()
                                .border_color(colors.muted_foreground),
                        ),
                ),
        )
        .child(div().text_xs().child(t!("git.outgoing")))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::{Action, commit_line, primary_action, summary};
    use syro_core::git::{BranchStatus, Commit};
    use syro_core::session::now_ms;

    /// The line an open commit leads with: an author and an age, in the app's own
    /// words rather than git's English `%ar`.
    #[test]
    fn a_commit_under_its_row_says_who_and_how_long_ago() {
        let commit = |at: i64| Commit {
            id: "abc123".into(),
            short: "abc1".into(),
            summary: "fix the flaky test".into(),
            author: "Ada".into(),
            at,
            refs: Vec::new(),
            parents: Vec::new(),
        };
        let now = now_ms() / 1000;
        assert_eq!(commit_line(&commit(now - 7_200)), "Ada · 2h ago");
        assert_eq!(commit_line(&commit(now - 3 * 86_400)), "Ada · 3d ago");
        assert_eq!(commit_line(&commit(now)), "Ada · just now");
        // A clock that ran backwards is an age of nothing, not a negative one.
        assert_eq!(commit_line(&commit(now + 600)), "Ada · just now");
    }

    /// The two sentences the panel leads with, in the two states the reference
    /// panel has: a clean branch, and a branch with work on it.
    #[test]
    fn the_summary_says_what_the_branch_has() {
        let status = BranchStatus {
            branch: Some("main".into()),
            upstream: Some("origin/main".into()),
            ahead: 0,
            behind: 0,
        };
        let (headline, detail) = summary(Some(&status), 0, "origin/main");
        assert_eq!(headline, "No changes on this branch");
        assert_eq!(
            detail,
            "This workspace is clean and this branch has no changes ahead of origin/main"
        );

        let (headline, detail) = summary(Some(&status), 3, "origin/main");
        assert_eq!(headline, "3 changed files");
        assert!(detail.contains("origin/main"), "{detail}");
        assert_eq!(summary(Some(&status), 1, "origin/main").0, "1 changed file");

        // Nothing to describe: no worktree, or one git cannot name a branch for.
        let (headline, _) = summary(None, 0, "");
        assert_eq!(headline, "No branch");
    }

    /// The button's ladder: work to commit, then commits to push, then either a
    /// push that creates the upstream or nothing at all.
    #[test]
    fn the_button_follows_what_is_there_to_do() {
        let status = |ahead: u32, upstream: Option<&str>| BranchStatus {
            branch: Some("main".into()),
            upstream: upstream.map(str::to_string),
            ahead,
            behind: 0,
        };

        assert_eq!(primary_action(true, None), Action::Commit);
        // Uncommitted work outranks a push: the commit has to happen first.
        assert_eq!(
            primary_action(true, Some(&status(3, Some("origin/main")))),
            Action::Commit
        );
        assert_eq!(
            primary_action(false, Some(&status(0, Some("origin/main")))),
            Action::Idle
        );
        assert_eq!(
            primary_action(false, Some(&status(3, Some("origin/main")))),
            Action::Push
        );
        // A branch git knows no upstream for is published, not pushed.
        assert_eq!(
            primary_action(false, Some(&status(3, None))),
            Action::Publish
        );
        // And before the first read: a disabled commit, not a moving frame.
        assert_eq!(primary_action(false, None), Action::Idle);
        assert_eq!(Action::Idle.label(), "Commit");
    }
}
