//! syro's window: the sidebar of repos and worktrees, the active worktree's file tree, and the
//! tab bar over the panes those tabs hold.

// The app's own translations, at the crate root because `t!` expands to
// `crate::_rust_i18n_t!`. `locales/ui.yml` is the file; `i18n.rs` is which
// language is spoken.
rust_i18n::i18n!("locales", fallback = "en");

mod agent_logo;
mod awake;
mod dialogs;
mod file_icons;
mod file_tree;
mod git_graph;
mod git_panel;
mod i18n;
mod opener;
mod palette;
mod pane;
mod resize;
mod resource_bar;
mod restore;
mod right_sidebar;
mod session_panel;
mod settings_view;
mod sidebar;
mod split;
mod state;
mod usage_chart;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

use gpui_kit::base::{ColorTokens, StyledExt as _, TestSupportExt as _, Theme};
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, WindowExt as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rust_i18n::t;
use syro_core::config::Settings;
use syro_core::event::CoreEvent;
use syro_core::model::{AgentSpec, RepoId, TabLayout, TabSpec, Worktree, WorktreeId};
use syro_core::session;
use syro_core::store::Store;
use syro_core::term::{TermSize, TermSpec};

use crate::agent_logo::{Logo, TERMINAL_LOGO, agent_logo};
use crate::dialogs::{NewWorktree, RemoveWorktree};
use crate::file_tree::FileTreeView;
use crate::git_graph::GitGraph;
use crate::git_panel::GitPanel;
use crate::opener::Opened;
use crate::pane::PaneItem as _;
use crate::pane::diff::DiffView;
use crate::pane::editor::EditorView;
use crate::pane::image::ImageView;
use crate::pane::terminal::{PaneStatus, SessionRef, TerminalView, term_spec_for_agent};
use crate::resize::WidthDrag;
use crate::resource_bar::TerminalName;
use crate::right_sidebar::Panel;
use crate::session_panel::SessionPanel;
use crate::sidebar::Sidebar;
use crate::state::{AppState, branch_or_name, diff, editor, tab, terminal, worktree};

/// The identifier the app gives the platform, and the name beside it.
const TAB_BAR_HEIGHT: f32 = 36.;

const BUNDLE_ID: &str = "com.syro.app";
const APP_NAME: &str = "syro";

/// One tab's contents.
enum PaneView {
    Terminal {
        view: Entity<TerminalView>,
        /// Held, not read: the subscription that forwards this terminal's agent
        /// status to the state, and it must die with the tab.
        _status: Subscription,
    },
    Editor {
        view: Entity<EditorView>,
    },
    Image {
        view: Entity<ImageView>,
    },
    Diff {
        view: Entity<DiffView>,
    },
    /// Restored documents keep only their spec until their pane is visible.
    Deferred {
        spec: TabSpec,
    },
}

impl PaneView {
    fn title(&self, cx: &App) -> SharedString {
        match self {
            PaneView::Terminal { view, .. } => view.read(cx).title(cx),
            PaneView::Editor { view } => view.read(cx).title(cx),
            PaneView::Image { view } => view.read(cx).title(cx),
            PaneView::Diff { view } => view.read(cx).title(cx),
            PaneView::Deferred {
                spec: TabSpec::File { path },
            } => path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string())
                .into(),
            PaneView::Deferred { .. } => t!("pane.diff").to_string().into(),
        }
    }

    fn is_dirty(&self, cx: &App) -> bool {
        match self {
            PaneView::Terminal { view, .. } => view.read(cx).is_dirty(cx),
            PaneView::Editor { view } => view.read(cx).is_dirty(cx),
            PaneView::Image { view } => view.read(cx).is_dirty(cx),
            PaneView::Diff { view } => view.read(cx).is_dirty(cx),
            PaneView::Deferred { .. } => false,
        }
    }

    /// The agent this pane runs, if it runs one: the name a tab is titled with,
    /// because an agent's own title is the task it is on and that changes.
    fn agent_name<'a>(&'a self, cx: &'a App) -> Option<&'a str> {
        match self {
            PaneView::Terminal { view, .. } => view.read(cx).agent_name(),
            PaneView::Editor { .. }
            | PaneView::Image { .. }
            | PaneView::Diff { .. }
            | PaneView::Deferred { .. } => None,
        }
    }

    fn spec(&self, cx: &App) -> TabSpec {
        match self {
            PaneView::Terminal { view, .. } => view
                .read(cx)
                .to_spec(cx)
                .expect("a terminal describes itself"),
            PaneView::Editor { view } => view
                .read(cx)
                .to_spec(cx)
                .expect("an editor describes itself"),
            PaneView::Image { view } => view
                .read(cx)
                .to_spec(cx)
                .expect("an image describes itself"),
            PaneView::Diff { view } => view.read(cx).to_spec(cx).expect("a diff describes itself"),
            PaneView::Deferred { spec } => spec.clone(),
        }
    }

    /// Where the keyboard goes when this tab becomes the visible one.
    fn focus_handle(&self, cx: &App) -> Option<FocusHandle> {
        match self {
            PaneView::Terminal { view, .. } => Some(view.read(cx).focus_handle(cx)),
            PaneView::Editor { view } => Some(view.read(cx).focus_handle(cx)),
            PaneView::Image { view } => Some(view.read(cx).focus_handle(cx)),
            PaneView::Diff { view } => Some(view.read(cx).focus_handle(cx)),
            PaneView::Deferred { .. } => None,
        }
    }

    /// Whether this pane's child has exited. Only a terminal has one, and a
    /// tab that says so is where the restart affordance lives.
    /// A shell or an agent, live or not yet restored: never closed in bulk.
    fn is_terminal(&self) -> bool {
        matches!(
            self,
            PaneView::Terminal { .. }
                | PaneView::Deferred {
                    spec: TabSpec::Terminal { .. }
                }
        )
    }

    fn exited(&self, cx: &App) -> bool {
        match self {
            PaneView::Terminal { view, .. } => view.read(cx).exit_status().is_some(),
            _ => false,
        }
    }

    /// Whether this pane is showing `path`. Two clicks on the same file are
    /// one tab because of this.
    fn shows(&self, cx: &App, path: &Path) -> bool {
        match self {
            PaneView::Terminal { .. } => false,
            PaneView::Editor { view } => view.read(cx).path() == path,
            PaneView::Image { view } => view.read(cx).path() == path,
            PaneView::Deferred { spec } => {
                matches!(spec, TabSpec::File { path: saved } if saved == path)
            }
            // A diff is a file's changes, not the file: opening the file has to
            // be its own tab.
            PaneView::Diff { .. } => false,
        }
    }

    fn save(&self, cx: &mut App) -> anyhow::Result<()> {
        match self {
            PaneView::Terminal { view, .. } => view.update(cx, |view, cx| view.save(cx)),
            PaneView::Editor { view } => view.update(cx, |view, cx| view.save(cx)),
            PaneView::Image { view } => view.update(cx, |view, cx| view.save(cx)),
            PaneView::Diff { view } => view.update(cx, |view, cx| view.save(cx)),
            PaneView::Deferred { .. } => Ok(()),
        }
    }

    fn element(&self) -> AnyElement {
        match self {
            PaneView::Terminal { view, .. } => view.clone().into_any_element(),
            PaneView::Editor { view } => view.clone().into_any_element(),
            PaneView::Image { view } => view.clone().into_any_element(),
            PaneView::Diff { view } => view.clone().into_any_element(),
            PaneView::Deferred { .. } => div().into_any_element(),
        }
    }
}

/// A tab being dragged along the bar: which one, and the name its chip shows.
struct DraggedTab {
    worktree_id: WorktreeId,
    index: usize,
    title: SharedString,
}

/// One row of the tab bar, read off the tab's tree before anything mutates.
struct TabRow {
    title: SharedString,
    /// The agent's logo, which stands in for its name in the row: `title` is then
    /// only the task.
    logo: Option<Logo>,
    /// The agent and the task in words, for the screen reader.
    aria: SharedString,
    dirty: bool,
    /// The tab is more than one pane, divided this way.
    axis: Option<split::Axis>,
    /// The first pane in the tab whose child has exited, if any. What the row's
    /// restart button belongs to.
    exited: Option<split::Pane>,
    /// An agent is still running in one of the tab's panes: closing asks first.
    live_agent: bool,
}

/// One id per pane, so a pane has a name that does not move when the pane beside
/// it closes: the layout holds ids, and the workspace's map of views is keyed by
/// them, which is what keeps a close from renumbering anything.
static NEXT_PANE: AtomicUsize = AtomicUsize::new(0);

/// The root view: sidebar | the tab bar and the pane under it | right sidebar.
pub struct Workspace {
    state: Entity<AppState>,
    sidebar: Entity<Sidebar>,
    /// The right sidebar's three panels.
    file_tree: Entity<FileTreeView>,
    sessions: Entity<SessionPanel>,
    git: Entity<GitPanel>,
    /// Which panel the right sidebar is showing. `None` is a closed sidebar —
    /// one field rather than an open flag per panel, because "which panel" and
    /// "is it up" are the same question.
    panel: Option<Panel>,
    /// The panel that comes back when the sidebar is toggled on again: what was
    /// last looked at, not the first tab.
    last_panel: Panel,
    /// The sidebar's width, dragged by its leading edge.
    right_width: WidthDrag,
    /// The resource readout across the bottom. A view of its own because it owns
    /// the sweep timer and the breakdown's open state, neither of which the tab
    /// list has anything to say about.
    resource_bar: Entity<crate::resource_bar::ResourceBar>,
    /// Every live pane's view, by content id, per worktree — whatever tab and tree position it is
    /// in.
    panes: BTreeMap<WorktreeId, BTreeMap<split::Pane, PaneView>>,
    /// The worktrees whose saved tabs have already been looked at.
    restored: BTreeSet<WorktreeId>,
    /// Each worktree's tabs: which one is on screen, and the panes each is made
    /// of (see [`crate::split`]). The bar lists these tabs, so a split is a pane
    /// under its tab and not a row of its own.
    layout: BTreeMap<WorktreeId, split::Layout>,
    /// Why the active worktree has no terminal, when it has none.
    terminal_error: Option<String>,
    /// Where the tab bar's `+` was clicked, in window coordinates: its menu of
    /// new tabs. `None` is no menu, and the same shape the pane's menu and the
    /// file tree's use.
    add_menu: Option<Point<Pixels>>,
    /// The `+`'s bounds as last painted: its menu opens under its right edge.
    add_button: std::rc::Rc<std::cell::Cell<Bounds<Pixels>>>,
    /// Where the tab bar was right-clicked: its menu (close all files).
    tab_menu: Option<Point<Pixels>>,
    /// The pane that should have the keyboard has not been handed it yet.
    focus_pending: bool,
    /// The full-window graph, while it is up. Held rather than rebuilt on every
    /// open, so the history it read and the commit it has open survive the graph
    /// being closed and opened again.
    graph: Option<Entity<GitGraph>>,
    /// The graph and its selection survive while a diff tab is in front; this is
    /// only whether the graph is currently drawn over the panes.
    graph_visible: bool,
    /// A file the tree opened and the opener refused. Shown as a line above the
    /// pane rather than instead of it: the tab that was already open is still
    /// the one the user is reading.
    open_error: Option<String>,
    /// The view's own focus handle.
    focus: FocusHandle,
    _events: Subscription,
}

impl Workspace {
    fn new(cx: &mut Context<Self>, state: Entity<AppState>) -> Self {
        let events = cx.subscribe(&state, |this, _state, event: &CoreEvent, cx| {
            match event {
                CoreEvent::ActiveWorktreeChanged { .. } => this.ensure_terminal(cx),
                CoreEvent::WorktreeRemoved { worktree } => {
                    // Dropping the list drops the views, and a terminal's view
                    // holds the last `Rc<TermSession>` — which hangs its shell
                    // up.
                    this.forget_worktree(&worktree.id, cx);
                    this.restored.remove(&worktree.id);
                }
                _ => {}
            }
            // The queued events are flushed at the top of the next frame.
            cx.notify();
        });
        let resource_bar = cx.new(crate::resource_bar::ResourceBar::new);
        let workspace = Self {
            state: state.clone(),
            sidebar: cx.new(|cx| Sidebar::new(state.clone(), resource_bar.clone(), cx)),
            file_tree: cx.new(|cx| FileTreeView::new(state.clone(), cx)),
            sessions: cx.new(|cx| SessionPanel::new(state.clone(), cx)),
            git: cx.new(|cx| GitPanel::new(state.clone(), cx)),
            panel: Some(Panel::default()),
            last_panel: Panel::default(),
            // The width a previous run was dragged to, or the default.
            // `WidthDrag::new` clamps, so a width written by a wider window
            // cannot open this one wider than the sidebar is allowed to be.
            right_width: WidthDrag::new(
                state.read(cx).right_width().unwrap_or(right_sidebar::WIDTH),
                right_sidebar::MIN_WIDTH,
                right_sidebar::MAX_WIDTH,
            ),
            resource_bar,
            panes: BTreeMap::new(),
            restored: BTreeSet::new(),
            layout: BTreeMap::new(),
            terminal_error: None,
            add_menu: None,
            add_button: Default::default(),
            tab_menu: None,
            focus_pending: false,
            graph: None,
            graph_visible: false,
            open_error: None,
            focus: cx.focus_handle(),
            _events: events,
        };
        // Nothing is built here: saved tabs and the terminal come back on the
        // first frame (see `restore_and_ensure`).
        workspace
    }

    /// Open a path in an editor tab, resolved under the active worktree when it
    /// is relative: what quick open does with the file it picked.
    pub fn open_path(
        &mut self,
        path: PathBuf,
        line: Option<u32>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // `line` is accepted and dropped: gpui-kit 0.7.1's `EditorState` has no public cursor API, so a
        // caret cannot be placed. The file opens either way.
        let _ = line;
        self.open_file(&editor::Open { path }, window, cx);
    }

    /// The tabs of the worktree on screen, each with the panes it is made of —
    /// what persistence writes, and what the bar draws. The order is the user's,
    /// so it is a `Vec`, not a `BTreeMap` of titles.
    pub fn active_tabs(&self, cx: &App) -> Vec<TabLayout> {
        let Some(worktree) = self.state.read(cx).active_id() else {
            return Vec::new();
        };
        self.tab_layouts(worktree, cx)
    }

    /// Every tab of one worktree, as it persists: the tree of specs, leaves in
    /// draw order. A pane with no view is left out of its parent — a pane the
    /// user closed is not a tab to come back to.
    fn tab_layouts(&self, worktree_id: &WorktreeId, cx: &App) -> Vec<TabLayout> {
        let Some(layout) = self.layout(worktree_id) else {
            return Vec::new();
        };
        (0..layout.tab_count())
            .filter_map(|tab| {
                let root = layout.root(tab)?;
                self.tab_spec(root, worktree_id, cx)
            })
            .collect()
    }

    /// One node of a tab, as it persists.
    fn tab_spec(
        &self,
        node: &split::Node,
        worktree_id: &WorktreeId,
        cx: &App,
    ) -> Option<TabLayout> {
        match node {
            split::Node::Pane { pane } => {
                let view = self.pane(worktree_id, *pane)?;
                Some(TabLayout::Pane(view.spec(cx)))
            }
            split::Node::Split {
                axis,
                first,
                second,
                ..
            } => {
                match (
                    self.tab_spec(first, worktree_id, cx),
                    self.tab_spec(second, worktree_id, cx),
                ) {
                    (Some(first), Some(second)) => Some(TabLayout::Split {
                        axis: *axis,
                        first: Box::new(first),
                        second: Box::new(second),
                    }),
                    // One half gone: the half that is left is the tab.
                    (Some(one), None) | (None, Some(one)) => Some(one),
                    (None, None) => None,
                }
            }
        }
    }

    /// The active worktree's terminal, whichever tab it is. What a test needs to
    /// right-click: the split's entries are in that pane's own menu, and after a
    /// split the terminal is no longer the tab on screen.
    #[cfg(test)]
    pub fn terminal_tab(&self, cx: &App) -> Option<Entity<TerminalView>> {
        let worktree_id = self.state.read(cx).active_id().cloned()?;
        self.layout(&worktree_id)?
            .panes()
            .into_iter()
            .find_map(|pane| match self.pane(&worktree_id, pane)? {
                PaneView::Terminal { view, .. } => Some(view.clone()),
                _ => None,
            })
    }

    /// The `index`th pane of the active worktree in draw order, for a test that
    /// needs both halves of a split at once — which is the only way to say that
    /// one of them got something the other did not.
    #[cfg(test)]
    pub fn terminal_at(&self, index: usize, cx: &App) -> Option<Entity<TerminalView>> {
        let worktree_id = self.state.read(cx).active_id().cloned()?;
        let pane = *self.layout(&worktree_id)?.panes().get(index)?;
        match self.pane(&worktree_id, pane)? {
            PaneView::Terminal { view, .. } => Some(view.clone()),
            _ => None,
        }
    }

    /// The tab that is on screen and what it holds, for what reads it from
    /// outside the view. A worktree's first tab is its terminal, so this is what
    /// the window tests ask about when they ask what a worktree is running.
    #[allow(
        dead_code,
        reason = "the read surface of the visible tab: what the window tests assert on"
    )]
    pub fn active_terminal(&self, cx: &App) -> Option<Entity<TerminalView>> {
        let worktree_id = self.state.read(cx).active_id().cloned()?;
        match self.active_view(&worktree_id, cx)? {
            PaneView::Terminal { view, .. } => Some(view.clone()),
            _ => None,
        }
    }

    /// The diff tab that is on screen, for what reads it from outside the view.
    #[allow(
        dead_code,
        reason = "the read surface of the visible tab: what the window tests assert on"
    )]
    pub fn active_diff(&self, cx: &App) -> Option<Entity<DiffView>> {
        let worktree_id = self.state.read(cx).active_id().cloned()?;
        match self.active_view(&worktree_id, cx)? {
            PaneView::Diff { view } => Some(view.clone()),
            _ => None,
        }
    }

    /// The editor tab that is on screen, for what reads it from outside the
    /// view — a restored file tab is asserted through it.
    #[allow(
        dead_code,
        reason = "the read surface of the visible tab: what the window tests assert on"
    )]
    pub fn active_editor(&self, cx: &App) -> Option<Entity<EditorView>> {
        let worktree_id = self.state.read(cx).active_id().cloned()?;
        match self.active_view(&worktree_id, cx)? {
            PaneView::Editor { view } => Some(view.clone()),
            _ => None,
        }
    }

    /// One pane's view by content id.
    fn pane(&self, worktree_id: &WorktreeId, pane: split::Pane) -> Option<&PaneView> {
        self.panes.get(worktree_id)?.get(&pane)
    }

    /// The view in the pane with the keyboard.
    fn active_view(&self, worktree_id: &WorktreeId, _cx: &App) -> Option<&PaneView> {
        let pane = self.layout(worktree_id)?.focused_pane()?;
        self.pane(worktree_id, pane)
    }

    /// The panes of the tab on screen, in draw order, with what each holds.
    /// The read surface of "what is this tab made of", which is what the window
    /// tests assert on.
    #[allow(
        dead_code,
        reason = "the read surface of the visible tab: what the window tests assert on"
    )]
    pub fn pane_specs(&self, cx: &App) -> Vec<TabSpec> {
        let Some(worktree_id) = self.state.read(cx).active_id() else {
            return Vec::new();
        };
        let Some(layout) = self.layout(worktree_id) else {
            return Vec::new();
        };
        layout
            .panes_of(layout.current())
            .into_iter()
            .filter_map(|pane| self.pane(worktree_id, pane).map(|view| view.spec(cx)))
            .collect()
    }

    /// A worktree's layout, if it has been visited.
    fn layout(&self, worktree_id: &WorktreeId) -> Option<&split::Layout> {
        self.layout.get(worktree_id)
    }

    fn layout_mut(&mut self, worktree_id: &WorktreeId) -> Option<&mut split::Layout> {
        self.layout.get_mut(worktree_id)
    }

    /// The tab on screen: the one whose panes are drawn.
    fn active_index(&self, worktree_id: &WorktreeId) -> Option<usize> {
        self.layout(worktree_id).map(|layout| layout.current())
    }

    /// Make a tab the visible one, handing the keyboard to the pane that had it
    /// there. What a click on the bar says.
    fn set_active_index(&mut self, worktree_id: &WorktreeId, index: usize) -> bool {
        let moved = self
            .layout_mut(worktree_id)
            .is_some_and(|layout| layout.set_current(index));
        if moved {
            self.focus_pending = true;
        }
        moved
    }

    /// Give the keyboard to the pane the layout says is focused, on the first
    /// frame after something moved it. Called from `render`, which is where this
    /// view has a window to focus with.
    fn ensure_pane_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !std::mem::take(&mut self.focus_pending) {
            return;
        }
        let Some(worktree_id) = self.state.read(cx).active_id().cloned() else {
            return;
        };
        self.focus_focused_pane(&worktree_id, window, cx);
    }

    /// Start the active worktree's terminal once.
    fn ensure_terminal(&mut self, cx: &mut Context<Self>) {
        let Some(worktree) = self.state.read(cx).active().cloned() else {
            return;
        };
        if !self.restored.contains(&worktree.id) {
            return;
        }
        if self.has_terminal(&worktree.id) {
            return;
        }
        if !worktree.path.is_dir() {
            self.terminal_error =
                Some(t!("workspace.gone", path = worktree.path.display()).to_string());
            return;
        }
        let spec = TermSpec::shell(worktree.path.clone(), TermSize::new(80, 24));
        if let Some(reason) = unrunnable(&spec) {
            self.terminal_error = Some(reason);
            return;
        }
        let terminal = cx.new(|cx| {
            TerminalView::new(cx, spec, None, None, None)
                .expect("the shell and the directory were checked")
        });
        let view = self.wire_terminal(&worktree.id, terminal, cx);
        let worktree_id = worktree.id.clone();
        self.add_tab(&worktree_id, view);
        self.terminal_error = None;
        self.panes_changed(&worktree_id, cx);
    }

    fn has_terminal(&self, worktree_id: &WorktreeId) -> bool {
        self.panes.get(worktree_id).is_some_and(|panes| {
            panes
                .values()
                .any(|pane| matches!(pane, PaneView::Terminal { .. }))
        })
    }

    /// The first frame of a worktree is when its saved tabs come back, and it is also where its
    /// terminal is made sure of.
    fn restore_and_ensure(&mut self, cx: &mut Context<Self>) {
        let Some(worktree) = self.state.read(cx).active().cloned() else {
            return;
        };
        if self.restored.insert(worktree.id.clone()) {
            self.restore_worktree(&worktree.id, cx);
            // Nothing was saved, or everything dropped: the worktree still needs somewhere to run an agent.
            if self.tab_count(&worktree.id) == 0 {
                self.ensure_terminal(cx);
            }
        }
    }

    /// Push the saved tabs of `worktree_id` back, each with the panes it was made of and in the
    /// shape it was saved in.
    fn restore_worktree(&mut self, worktree_id: &WorktreeId, cx: &mut Context<Self>) {
        let (saved, worktrees) = {
            let state = self.state.read(cx);
            (state.tabs().to_vec(), state.worktrees().to_vec())
        };
        let plan = restore::plan(&saved, &worktrees, worktree_id);

        for tab in &plan.restore {
            // Documents need only a spec here; visible panes load before drawing.
            let mut panes: Vec<Option<split::Pane>> = Vec::new();
            for spec in tab.specs() {
                panes.push(match spec {
                    TabSpec::Terminal {
                        cwd,
                        agent,
                        session,
                    } => self.restore_terminal(
                        worktree_id,
                        cwd,
                        agent.as_deref(),
                        session.as_deref(),
                        cx,
                    ),
                    TabSpec::File { .. } | TabSpec::Diff { .. } => {
                        Some(self.new_pane(worktree_id, PaneView::Deferred { spec: spec.clone() }))
                    }
                });
            }
            let Some(first) = panes.first().copied().flatten() else {
                // Not one pane of this tab came back, so it is not a tab.
                continue;
            };
            let tab_index = self
                .layout
                .entry(worktree_id.clone())
                .or_default()
                .add_tab(first);
            let mut rest = panes.into_iter().skip(1);
            self.place_tab(worktree_id, tab_index, &[], None, tab, &mut rest);
        }

        // A pane that found no place in a tree is dropped: nothing else would
        // ever draw it, and its child would stay alive behind the frame.
        self.forget_placeless_panes(worktree_id, cx);
        // A restored list shows its first tab, and reads each tab from its first pane: `add_tab` puts
        // every new tab on screen and `split` hands the keyboard to each pane it adds, so
        // without this the *last* of both would be what is up.
        if let Some(layout) = self.layout_mut(worktree_id) {
            for tab in 0..layout.tab_count() {
                layout.focus_first(tab);
            }
            layout.set_current(0);
        }

        let dropped = &plan.dropped;
        if let Some(first) = dropped.first() {
            let rest = dropped.len() - 1;
            self.open_error = Some(if rest == 0 {
                t!(
                    "workspace.not_restored",
                    what = restore::describe(&first.spec),
                    reason = first.reason
                )
                .to_string()
            } else {
                t!(
                    "workspace.not_restored_more",
                    what = restore::describe(&first.spec),
                    reason = first.reason,
                    rest = rest
                )
                .to_string()
            });
        }

        self.persist_tabs(cx);
    }

    /// A restored terminal: the same cwd and the same agent, a new child — and, when the
    /// saved spec names a session, the conversation it was in. The pane it made, for the
    /// tab's tree to be built around it.
    fn restore_terminal(
        &mut self,
        worktree_id: &WorktreeId,
        cwd: &Path,
        agent: Option<&str>,
        session: Option<&str>,
        cx: &mut Context<Self>,
    ) -> Option<split::Pane> {
        let preset = agent.and_then(|id| self.state.read(cx).agent(id));
        let settings = self.state.read(cx).settings().terminal.clone();
        let size = TermSize::new(80, 24);
        // A saved session comes back the way the history panel opens one: the transcript
        // is found, and the preset's `resume_args` re-enter it.
        let resume = match (preset.as_ref(), session, session_roots()) {
            (Some(agent), Some(id), Some(roots)) if !agent.resume_args.is_empty() => {
                saved_session(&roots, agent, cwd, id)
            }
            _ => None,
        };
        let spec = match &preset {
            Some(agent) => term_spec_for_agent(agent, cwd.to_path_buf(), size, resume.as_ref()),
            None => TermSpec::shell(cwd.to_path_buf(), size),
        };
        let spec = with_settings(spec, &settings);
        let spec = match startable(spec) {
            Ok(spec) => spec,
            Err(reason) => {
                self.terminal_error = Some(reason);
                return None;
            }
        };
        let terminal = cx.new(|cx| {
            TerminalView::new(cx, spec, preset, resume, None)
                .expect("the command and the directory were checked")
        });
        let view = self.wire_terminal(worktree_id, terminal, cx);
        Some(self.new_pane(worktree_id, view))
    }

    /// Read a restored document once, when its pane first becomes visible.
    fn load_pane(
        &mut self,
        worktree_id: &WorktreeId,
        pane: split::Pane,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(PaneView::Deferred { spec }) = self.pane(worktree_id, pane) else {
            return;
        };
        let loaded = match spec.clone() {
            TabSpec::File { path } => match opener::open(&path) {
                Ok(Opened::Text(text)) => Ok(PaneView::Editor {
                    view: cx.new(|cx| EditorView::new(window, cx, path, text)),
                }),
                Ok(Opened::Image) => Ok(PaneView::Image {
                    view: cx.new(|cx| ImageView::new(path, cx)),
                }),
                Ok(Opened::Refused(refusal)) => Err(refusal.message(&path)),
                Err(error) => Err(format!("{}: {error:#}", path.display())),
            },
            TabSpec::Diff {
                worktree_id: target,
            } => match self.state.read(cx).worktree(&target).cloned() {
                Some(worktree) => {
                    self.state.update(cx, |state, cx| state.refresh_changes(cx));
                    Ok(PaneView::Diff {
                        view: cx.new(|cx| DiffView::new(window, cx, self.state.clone(), &worktree)),
                    })
                }
                None => Err(t!("workspace.gone", path = target).to_string()),
            },
            TabSpec::Terminal { .. } => unreachable!("terminals are restored immediately"),
        };
        match loaded {
            Ok(view) => {
                self.panes.get_mut(worktree_id).unwrap().insert(pane, view);
                self.focus_pending = true;
            }
            Err(error) => {
                self.open_error = Some(error);
                if let Some((tab, path)) = self
                    .layout(worktree_id)
                    .and_then(|layout| layout.path_of_pane(pane))
                {
                    self.close_pane_path(worktree_id, tab, &path, cx);
                }
            }
        }
    }

    /// Every pane of the visible tab, including both halves of a split, needs its view.
    fn load_visible_panes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(worktree_id) = self.state.read(cx).active_id().cloned() else {
            return;
        };
        loop {
            let pending: Vec<split::Pane> = self
                .layout(&worktree_id)
                .map(|layout| layout.panes_of(layout.current()))
                .unwrap_or_default()
                .into_iter()
                .filter(|pane| {
                    matches!(
                        self.pane(&worktree_id, *pane),
                        Some(PaneView::Deferred { .. })
                    )
                })
                .collect();
            if pending.is_empty() {
                break;
            }
            for pane in pending {
                self.load_pane(&worktree_id, pane, window, cx);
            }
            // A refused file can close its tab and reveal another deferred tab.
        }
    }

    /// Put `view` in the worktree's pane map under a fresh id, without giving it
    /// a place in the tree: the caller decides whether it is a new tab or the
    /// second half of a split, and a pane with no place is a pane on its way out.
    fn new_pane(&mut self, worktree_id: &WorktreeId, view: PaneView) -> split::Pane {
        let pane = NEXT_PANE.fetch_add(1, Ordering::Relaxed);
        self.panes
            .entry(worktree_id.clone())
            .or_default()
            .insert(pane, view);
        pane
    }

    /// A new tab in `worktree_id` holding `view`, at the end of the bar and on
    /// screen. Answers the pane it made.
    fn add_tab(&mut self, worktree_id: &WorktreeId, view: PaneView) -> split::Pane {
        let pane = self.new_pane(worktree_id, view);
        self.layout
            .entry(worktree_id.clone())
            .or_default()
            .add_tab(pane);
        pane
    }

    /// How many tabs a worktree has.
    fn tab_count(&self, worktree_id: &WorktreeId) -> usize {
        self.layout(worktree_id)
            .map_or(0, |layout| layout.tab_count())
    }

    /// Drop one pane: its view — which hangs its child up — and its agent status,
    /// because a dot for a process that is gone is worse than no dot.
    fn drop_pane(&mut self, worktree_id: &WorktreeId, pane: split::Pane, cx: &mut Context<Self>) {
        let Some(view) = self
            .panes
            .get_mut(worktree_id)
            .and_then(|panes| panes.remove(&pane))
        else {
            return;
        };
        if let PaneView::Terminal { view, .. } = &view {
            let terminal_id = view.read(cx).terminal_id().clone();
            self.state.update(cx, |state, cx| {
                state.note_agent_status(worktree_id, &terminal_id, None, None, false, cx)
            });
        }
        drop(view);
    }

    /// What every change to the panes ends with: the tabs are saved for the next
    /// restart, the readout is told what is running, and the frame is asked for.
    fn panes_changed(&mut self, _worktree_id: &WorktreeId, cx: &mut Context<Self>) {
        self.persist_tabs(cx);
        self.sync_resource_bar(cx);
        cx.notify();
    }

    /// Rebuild one saved tab's tree: `spec` is the tree to build at `path` in `tab`, `axis` is the
    /// axis of the split that made that position (`None` at the root, where the pane is the tab
    /// itself), and `panes` is the views for the leaves it has not been given yet, in draw
    /// order.
    fn place_tab(
        &mut self,
        worktree_id: &WorktreeId,
        tab: usize,
        path: &[bool],
        axis: Option<split::Axis>,
        spec: &TabLayout,
        panes: &mut impl Iterator<Item = Option<split::Pane>>,
    ) {
        match spec {
            TabLayout::Pane(_) => {
                // The root pane is the tab that was just made: nothing to do.
                let Some(axis) = axis else {
                    return;
                };
                let Some(pane) = panes.next().flatten() else {
                    return;
                };
                let parent = &path[..path.len() - 1];
                if !self
                    .layout_mut(worktree_id)
                    .is_some_and(|layout| layout.split(tab, parent, axis, pane))
                {
                    tracing::warn!("a restored pane had no place to go");
                }
            }
            TabLayout::Split {
                axis: own,
                first,
                second,
            } => {
                // The first child keeps the position; the second is split off it,
                // which is what makes the pane order here draw order.
                self.place_tab(worktree_id, tab, path, axis, first, panes);
                let Some(pane) = panes.next().flatten() else {
                    return;
                };
                if !self
                    .layout_mut(worktree_id)
                    .is_some_and(|layout| layout.split(tab, path, *own, pane))
                {
                    tracing::warn!("a restored split had no place to go");
                    return;
                }
                let mut right = path.to_vec();
                right.push(true);
                self.place_tab(worktree_id, tab, &right, Some(*own), second, panes);
            }
        }
    }

    /// Drop every pane of a worktree that no tab holds. What is left over when a
    /// restore could not give a pane a place.
    fn forget_placeless_panes(&mut self, worktree_id: &WorktreeId, cx: &mut Context<Self>) {
        let placed: BTreeSet<split::Pane> = self
            .layout(worktree_id)
            .map(|layout| layout.panes().into_iter().collect())
            .unwrap_or_default();
        let orphans: Vec<split::Pane> = self
            .panes
            .get(worktree_id)
            .map(|panes| {
                panes
                    .keys()
                    .copied()
                    .filter(|pane| !placed.contains(pane))
                    .collect()
            })
            .unwrap_or_default();
        for pane in orphans {
            self.drop_pane(worktree_id, pane, cx);
        }
    }

    /// `session::Resume`: re-enter a past session in the worktree it came from.
    fn resume_session(&mut self, action: &session_panel::Resume, cx: &mut Context<Self>) {
        let session = &action.session;
        let target = {
            let state = self.state.read(cx);
            session::resume_target(session, state.worktrees())
                .map(|worktree| (worktree.id.clone(), worktree.name.clone()))
        };
        let Some((worktree_id, _name)) = target else {
            self.terminal_error =
                Some(t!("workspace.no_worktree_resume", path = session.cwd.display()).to_string());
            cx.notify();
            return;
        };
        // The worktree the session belongs to is the one to look at, so the
        // resume is visible rather than a tab opening off screen.
        let worktree_id = worktree_id.clone();
        self.state
            .update(cx, |state, cx| state.activate(&worktree_id, cx));
        let action = terminal::New {
            worktree_id,
            agent: Some(session.agent.id().to_string()),
            resume: Some(SessionRef {
                id: session.id.clone(),
                path: session.path.clone(),
            }),
            seed: None,
            model: None,
        };
        self.start_terminal(&action, cx);
    }

    /// `terminal::New`: open a terminal **tab** in a worktree, optionally with one of the
    /// configured agents in it.
    fn start_terminal(&mut self, action: &terminal::New, cx: &mut Context<Self>) {
        let Some(view) = self.open_terminal(action, cx) else {
            return;
        };
        self.add_tab(&action.worktree_id, view);
        // The tab that was just asked for is the tab to look at: a `+` that
        // opened something off screen would read as a `+` that did nothing.
        self.focus_pending = true;
        self.panes_changed(&action.worktree_id, cx);
    }

    /// Build the view a `terminal::New` asks for, without placing it: a tab and a
    /// split want the same terminal in two different places.
    fn open_terminal(
        &mut self,
        action: &terminal::New,
        cx: &mut Context<Self>,
    ) -> Option<PaneView> {
        let worktree = self.state.read(cx).worktree(&action.worktree_id).cloned()?;
        let agent = match &action.agent {
            Some(id) => match self.state.read(cx).agent(id) {
                Some(agent) => Some(agent),
                None => {
                    self.terminal_error = Some(t!("workspace.no_agent", id = id).to_string());
                    return None;
                }
            },
            None => None,
        };

        let settings = self.state.read(cx).settings().terminal.clone();
        let size = TermSize::new(80, 24);
        // A resume needs something to resume with: an agent whose preset has no
        // `resume_args` would silently start a new conversation under a row that
        // promised the old one.
        if action.resume.is_some()
            && agent
                .as_ref()
                .is_some_and(|agent| agent.resume_args.is_empty())
        {
            let name = agent.map(|agent| agent.name).unwrap_or_default();
            self.terminal_error = Some(t!("workspace.no_resume_args", name = name).to_string());
            return None;
        }
        let mut spec = match &agent {
            Some(agent) => {
                term_spec_for_agent(agent, worktree.path.clone(), size, action.resume.as_ref())
            }
            None => TermSpec::shell(worktree.path.clone(), size),
        };
        if action.resume.is_none() && agent.is_some() {
            if let Some(model) = &action.model {
                spec.args.extend(["--model".into(), model.clone()]);
            }
        }
        let spec = with_settings(spec, &settings);
        let spec = match startable(spec) {
            Ok(spec) => spec,
            Err(reason) => {
                self.terminal_error = Some(reason);
                return None;
            }
        };

        let resume = action.resume.clone();
        let terminal = cx.new(|cx| {
            TerminalView::new(cx, spec, agent, resume, action.seed.as_deref())
                .expect("the command and the directory were checked")
        });
        self.terminal_error = None;
        Some(self.wire_terminal(&worktree.id, terminal, cx))
    }

    /// `terminal::Add`: a shell tab in the worktree on screen. What the tab bar's
    /// `+` and `cmd-t` dispatch — neither of which knows a worktree id, which is
    /// why this resolves it here.
    fn add_terminal_tab(&mut self, cx: &mut Context<Self>) {
        let Some(worktree_id) = self.state.read(cx).active_id().cloned() else {
            return;
        };
        let action = terminal::New {
            worktree_id,
            agent: None,
            resume: None,
            seed: None,
            model: None,
        };
        self.start_terminal(&action, cx);
    }

    /// `split::Right` / `split::Down`: divide the pane with the keyboard in the tab on screen, with
    /// a **new terminal** in the half it adds.
    fn open_split(&mut self, axis: split::Axis, cx: &mut Context<Self>) {
        let Some(worktree_id) = self.state.read(cx).active_id().cloned() else {
            return;
        };
        let Some((tab, path)) = self
            .layout(&worktree_id)
            .map(|layout| (layout.current(), layout.focused_path()))
        else {
            return;
        };
        let action = terminal::New {
            worktree_id: worktree_id.clone(),
            agent: None,
            resume: None,
            seed: None,
            model: None,
        };
        let Some(view) = self.open_terminal(&action, cx) else {
            return;
        };
        let pane = self.new_pane(&worktree_id, view);
        let placed = self
            .layout_mut(&worktree_id)
            .is_some_and(|layout| layout.split(tab, &path, axis, pane));
        if !placed {
            // The pane went away between the two: the view has no place.
            self.drop_pane(&worktree_id, pane, cx);
            return;
        }
        // The new half takes the keyboard, which is what a split does everywhere.
        self.focus_pending = true;
        self.panes_changed(&worktree_id, cx);
    }

    /// `split::ClosePaneAt`: the `×` in a pane's corner. The pane's view goes —
    /// its child is hung up with it — and its sibling takes the area. The last
    /// pane of a tab takes the tab.
    fn close_pane_at(&mut self, pane: split::Pane, cx: &mut Context<Self>) {
        let Some(worktree_id) = self.state.read(cx).active_id().cloned() else {
            return;
        };
        let Some((tab, path)) = self
            .layout(&worktree_id)
            .and_then(|layout| layout.path_of_pane(pane))
        else {
            return;
        };
        self.close_pane_path(&worktree_id, tab, &path, cx);
    }

    /// Close the pane at `(tab, path)`, whatever asked for it.
    fn close_pane_path(
        &mut self,
        worktree_id: &WorktreeId,
        tab: usize,
        path: &[bool],
        cx: &mut Context<Self>,
    ) {
        let gone = self
            .layout_mut(worktree_id)
            .and_then(|layout| layout.close_pane(tab, path));
        let Some((pane, _)) = gone else {
            return;
        };
        self.drop_pane(worktree_id, pane, cx);
        // Whoever had the keyboard may have been the pane that just went away;
        // the layout has already moved it to the half that stayed.
        self.focus_pending = true;
        self.panes_changed(worktree_id, cx);
    }

    /// `split::ClosePane` — `cmd-w`: the pane with the keyboard and its
    /// terminal go away, and the rest of the tree takes the area.
    fn close_pane(&mut self, cx: &mut Context<Self>) {
        let Some(worktree_id) = self.state.read(cx).active_id().cloned() else {
            return;
        };
        let Some((tab, path)) = self
            .layout(&worktree_id)
            .map(|layout| (layout.current(), layout.focused_path()))
        else {
            return;
        };
        self.close_pane_path(&worktree_id, tab, &path, cx);
    }

    /// `terminal::Restart`: replace the terminal in `pane` with a new child, from
    /// the same spec — same directory, same agent, and the same session if it was
    /// a resume. The pane keeps its place in its tab and in the split.
    fn restart_terminal(&mut self, action: &terminal::Restart, cx: &mut Context<Self>) {
        let Some(PaneView::Terminal { view, .. }) = self.pane(&action.worktree_id, action.pane)
        else {
            return;
        };
        let (cwd, agent_id, resume) = {
            let view = view.read(cx);
            (
                view.cwd().to_path_buf(),
                view.agent_id().cloned(),
                view.resume().cloned(),
            )
        };
        let agent = agent_id.and_then(|id| self.state.read(cx).agent(id.as_str()));
        let settings = self.state.read(cx).settings().terminal.clone();
        let size = TermSize::new(80, 24);
        let spec = match &agent {
            Some(agent) => term_spec_for_agent(agent, cwd, size, resume.as_ref()),
            None => TermSpec::shell(cwd, size),
        };
        let spec = with_settings(spec, &settings);
        let spec = match startable(spec) {
            Ok(spec) => spec,
            Err(reason) => {
                self.terminal_error = Some(reason);
                return;
            }
        };
        let terminal = cx.new(|cx| {
            TerminalView::new(cx, spec, agent, resume, None)
                .expect("the command and the directory were checked")
        });
        let replacement = self.wire_terminal(&action.worktree_id, terminal, cx);
        let Some(panes) = self.panes.get_mut(&action.worktree_id) else {
            return;
        };
        // Dropping the old view drops the last `Rc<TermSession>` it held, and
        // `Drop` hangs the old child up. Replacing in place rather than closing
        // and pushing: the pane keeps its place in its tab and in the split.
        let old = panes.insert(action.pane, replacement);
        drop(old);
        self.persist_tabs(cx);
        self.sync_resource_bar(cx);
        self.terminal_error = None;
        cx.notify();
    }

    /// A terminal view as a pane, with its status subscription wired to the state — which is what
    /// the sidebar's dot, the hooks and the notification read.
    fn wire_terminal(
        &mut self,
        worktree_id: &WorktreeId,
        terminal: Entity<TerminalView>,
        cx: &mut Context<Self>,
    ) -> PaneView {
        let forwarded = worktree_id.clone();
        let status = cx.subscribe(&terminal, move |this, terminal, event: &PaneStatus, cx| {
            let status = event.status;
            let stalled = event.stalled;
            let agent = terminal.read(cx).agent_id().cloned();
            let terminal_id = terminal.read(cx).terminal_id().clone();
            this.state.update(cx, |state, cx| {
                state.note_agent_status(&forwarded, &terminal_id, agent, status, stalled, cx)
            });
            // The readout's rows are pushed, not polled, so a child that exited
            // has to be handed over again for its row to say `Done` or `Failed`.
            this.sync_resource_bar(cx);
            // And the tab's name may have moved with it.
            cx.notify();
        });
        PaneView::Terminal {
            view: terminal,
            _status: status,
        }
    }

    /// The readout, for a test that wants to check what it grouped.
    pub fn resource_bar(&self) -> &Entity<crate::resource_bar::ResourceBar> {
        &self.resource_bar
    }

    /// Escape closes the `+` menu. On the workspace's own root: the menu is
    /// drawn over the pane area, which is the whole subtree below it.
    fn handle_workspace_key(
        &mut self,
        event: &KeyDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if (self.add_menu.is_some() || self.tab_menu.is_some()) && event.keystroke.key == "escape" {
            self.add_menu = None;
            self.tab_menu = None;
            cx.stop_propagation();
            cx.notify();
        }
        // Escape closes the graph, the way it closes every other overlay.
        if self.graph_visible && event.keystroke.key == "escape" {
            self.graph_visible = false;
            cx.stop_propagation();
            cx.notify();
        }
    }

    /// Open the `+` menu where it was clicked.
    fn open_add_menu(&mut self, at: Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        self.add_menu = Some(at);
        // The keyboard goes to the workspace so Escape can close it, which is
        // the same road the pane's menu takes.
        window.focus(&self.focus, cx);
        cx.notify();
    }

    /// The `+` menu: a new terminal, and one row per configured agent. The `+` is
    /// a menu of *what kind of tab*, and an agent is the kind this app has.
    fn add_menu_overlay(
        &self,
        at: Point<Pixels>,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = Theme::global(cx).tokens.colors;
        let Some(worktree_id) = self.state.read(cx).active_id().cloned() else {
            return div().into_any_element();
        };
        let mut items: Vec<(String, String, Option<&str>, terminal::New)> = vec![(
            "add-menu-terminal".to_string(),
            t!("workspace.new_terminal").to_string(),
            // The key that does the same thing, shown beside each row.
            Some("⌘T"),
            terminal::New {
                worktree_id: worktree_id.clone(),
                agent: None,
                resume: None,
                seed: None,
                model: None,
            },
        )];
        for spec in self.state.read(cx).agents() {
            items.push((
                format!("add-menu-agent-{}", spec.id),
                spec.name.clone(),
                None,
                terminal::New {
                    worktree_id: worktree_id.clone(),
                    agent: Some(spec.id.clone()),
                    resume: None,
                    seed: None,
                    model: None,
                },
            ));
        }

        let mut panel = div()
            .id("add-menu")
            .test_support()
            .v_flex()
            .w(px(220.))
            .p_1()
            .rounded_md()
            .border_1()
            .border_color(colors.border)
            .bg(settings_view::popup_surface(colors.surface))
            .shadow_lg()
            .text_xs()
            .text_color(colors.foreground)
            .on_scroll_wheel(|_event, _window, cx| cx.stop_propagation());
        for (index, (id, label, shortcut, action)) in items.into_iter().enumerate() {
            if index == 1 {
                // The terminals and the agents are two groups, drawn apart by a line.
                panel = panel.child(div().my_1().h(px(1.)).w_full().bg(colors.border));
            }
            panel = panel.child(
                div()
                    .id(SharedString::from(id))
                    .test_support()
                    .aria_label(SharedString::from(label.clone()))
                    .h_flex()
                    .items_center()
                    .justify_between()
                    .gap_4()
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .cursor_pointer()
                    .hover(|el| el.bg(colors.muted))
                    .child(SharedString::from(label))
                    .when_some(shortcut, |el, shortcut| {
                        el.child(
                            div()
                                .text_color(colors.muted_foreground)
                                .child(SharedString::from(shortcut)),
                        )
                    })
                    .on_click(cx.listener(move |this, _event, window, cx| {
                        this.add_menu = None;
                        window.dispatch_action(Box::new(action.clone()), cx);
                        cx.notify();
                    })),
            );
        }
        self.popup(at, Anchor::TopRight, panel, window, cx)
    }

    /// The tab bar's right-click menu: one row, closing every file tab.
    fn tab_menu_overlay(
        &self,
        at: Point<Pixels>,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = Theme::global(cx).tokens.colors;
        let Some(worktree_id) = self.state.read(cx).active_id().cloned() else {
            return div().into_any_element();
        };
        let label = t!("workspace.close_all_files");
        let panel = div()
            .id("tab-menu")
            .test_support()
            .v_flex()
            .w(px(220.))
            .p_1()
            .rounded_md()
            .border_1()
            .border_color(colors.border)
            .bg(settings_view::popup_surface(colors.surface))
            .shadow_lg()
            .text_xs()
            .text_color(colors.foreground)
            .child(
                div()
                    .id("tab-menu-close-files")
                    .test_support()
                    .aria_label(label.clone())
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .cursor_pointer()
                    .hover(|el| el.bg(colors.muted))
                    .child(label)
                    .on_click(cx.listener(move |this, _event, _window, cx| {
                        this.tab_menu = None;
                        this.close_file_tabs(&worktree_id, cx);
                        cx.notify();
                    })),
            );
        self.popup(at, Anchor::TopLeft, panel, window, cx)
    }

    /// A menu panel at `at`, over everything; a click outside it closes it.
    fn popup(
        &self,
        at: Point<Pixels>,
        anchor: Anchor,
        panel: impl IntoElement,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let this = cx.entity().downgrade();
        let close = move |_: &MouseDownEvent, _: &mut Window, cx: &mut App| {
            this.update(cx, |workspace, cx| {
                workspace.add_menu = None;
                workspace.tab_menu = None;
                cx.notify();
            })
            .ok();
        };
        // The backdrop is sized to the window: `size_full` inside `anchored`
        // resolves to nothing, and a click outside the menu would miss it.
        let window_size = window.viewport_size();
        deferred(
            anchored().child(
                div()
                    .w(window_size.width)
                    .h(window_size.height)
                    .occlude()
                    .on_mouse_down(MouseButton::Left, close.clone())
                    .on_mouse_down(MouseButton::Right, close)
                    .child(
                        anchored()
                            .position(at)
                            .anchor(anchor)
                            .snap_to_window_with_margin(px(8.))
                            // Occluded, so a click on a row is not also a click on the backdrop.
                            .child(div().occlude().child(panel)),
                    ),
            ),
        )
        .with_priority(gpui_kit::base::POPUP_PRIORITY)
        .into_any_element()
    }

    /// The menus hang at a window point taken when they opened; the sidebar
    /// coming or going (by key, so no click closed them) moves what they hang from.
    fn close_menus(&mut self) {
        self.add_menu = None;
        self.tab_menu = None;
    }

    /// `panel::Show`: put one of the sidebar's panels on screen, or take it away
    /// when it is the one already there — clicking the icon of the panel you are
    /// looking at closes the sidebar.
    fn show_panel(&mut self, panel: Panel, cx: &mut Context<Self>) {
        self.close_menus();
        self.last_panel = panel;
        self.panel = (self.panel != Some(panel)).then_some(panel);
        cx.notify();
    }

    /// `panel::Toggle`: the sidebar's own on/off, wherever the button for it is.
    fn toggle_sidebar(&mut self, cx: &mut Context<Self>) {
        self.close_menus();
        self.panel = match self.panel {
            Some(_) => None,
            None => Some(self.last_panel),
        };
        cx.notify();
    }

    /// The sidebar's width, for a test that reads what a drag did to it.
    #[cfg(test)]
    pub fn right_width(&self) -> f32 {
        self.right_width.width()
    }

    /// Follow the pointer while the sidebar's leading edge is held. `None` is
    /// the release, which is when the width is written: a `state.json` write per
    /// mouse move is a file write per frame.
    fn resize_sidebar(&mut self, pointer_x: Option<f32>, cx: &mut Context<Self>) {
        match pointer_x {
            // This panel is on the right, so its leading edge follows the
            // pointer: moving left is what makes it wider.
            Some(x) => {
                if self.right_width.move_to(x, -1.) {
                    cx.notify();
                }
            }
            None => {
                if self.right_width.dragging() {
                    self.right_width.end();
                    let width = self.right_width.width();
                    self.state
                        .update(cx, |state, cx| state.set_right_width(width, cx));
                    cx.notify();
                }
            }
        }
    }

    /// The handle on the sidebar's leading edge: the seam it shares with the pane area.
    fn resize_handle(&self, colors: &ColorTokens, cx: &mut Context<Self>) -> AnyElement {
        div()
            .id("panel-resize")
            .test_support()
            .aria_label(t!("workspace.aria.resize_sidebar"))
            .absolute()
            .top(px(0.))
            .left(px(0.))
            .h_full()
            .w(px(6.))
            .flex()
            .items_center()
            .justify_center()
            .cursor_col_resize()
            .hover(|el| el.bg(colors.border))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, _window, cx| {
                    this.right_width.begin(event.position.x.as_f32());
                    cx.notify();
                }),
            )
            .into_any_element()
    }

    /// The live terminals, for the resource readout: the worktree each one is in,
    /// what to call it, and the pid whose process tree is what it costs. A
    /// terminal whose child has exited is on the list too — it has no pid left to
    /// cost anything, and its row is where `Done` and `Failed` are read.
    fn live_terminals(&self, cx: &App) -> Vec<crate::resource_bar::LiveTerminal> {
        let mut live = Vec::new();
        for (worktree_id, panes) in &self.panes {
            let mut names = self.agent_names(worktree_id, cx);
            for (pane, view) in panes {
                if let PaneView::Terminal { view, .. } = view {
                    let view = view.read(cx);
                    live.push(crate::resource_bar::LiveTerminal {
                        worktree: worktree_id.clone(),
                        name: names
                            .remove(pane)
                            .unwrap_or_else(|| TerminalName::plain(view.title(cx).to_string())),
                        pid: view.process_id(),
                        span: view.span(),
                        exit_ok: view.exit_status().map(|status| status.success()),
                        status: view.status(cx),
                        stalled: view.stalled(),
                    });
                }
            }
        }
        live
    }

    /// What each agent terminal of a worktree is called, by pane: the agent's logo with the task
    /// beside it (`[π] fix the build`), or `Pi - <task>` for a preset with no logo. The task is
    /// what the title names or the first thing it was asked, and the worktree's branch before
    /// either is known. The tab bar and the sidebar both read this, so a tab and its row match.
    fn agent_names(
        &self,
        worktree_id: &WorktreeId,
        cx: &App,
    ) -> BTreeMap<split::Pane, TerminalName> {
        let called = self
            .state
            .read(cx)
            .worktree(worktree_id)
            .map(|worktree| branch_or_name(worktree).to_string())
            .unwrap_or_else(|| worktree_id.clone());
        let mut panes = Vec::new();
        let mut names = Vec::new();
        let mut logos = Vec::new();
        // Pane order, which is the order the readout lists them in: a collision's numbers agree.
        for (pane, view) in self.panes.get(worktree_id).into_iter().flatten() {
            if let PaneView::Terminal { view, .. } = view {
                let view = view.read(cx);
                if let Some(agent) = view.agent_name() {
                    panes.push(*pane);
                    let detail = view.detail().unwrap_or(&called).to_string();
                    logos.push(
                        view.running_agent_id()
                            .and_then(|id| agent_logo(id.as_str())),
                    );
                    names.push((agent.to_string(), detail));
                }
            }
        }
        let numbered = syro_core::agent::numbered(&names);
        panes
            .into_iter()
            .enumerate()
            .map(|(index, pane)| {
                let (agent, _) = &names[index];
                let (number, detail) = &numbered[index];
                let logo = logos[index];
                let spelled = if *number == 0 {
                    format!("{agent} - {detail}")
                } else {
                    format!("{agent} {number} - {detail}")
                };
                // The logo stands in for the agent's word; a number stays, because
                // it is what tells two identical agents apart.
                let text = match (logo, *number) {
                    (Some(_), 0) => detail.clone(),
                    (Some(_), number) => format!("{number} - {detail}"),
                    (None, _) => spelled.clone(),
                };
                (
                    pane,
                    TerminalName {
                        text,
                        logo,
                        aria: spelled,
                    },
                )
            })
            .collect()
    }

    /// Tell the readout what is running.
    fn sync_resource_bar(&mut self, cx: &mut Context<Self>) {
        let live = self.live_terminals(cx);
        self.resource_bar
            .update(cx, |bar, cx| bar.set_terminals(live, cx));
    }
    /// `WorktreeRemoved`: its panes describe a directory that is gone.
    fn forget_worktree(&mut self, worktree_id: &WorktreeId, cx: &mut Context<Self>) {
        // Dropping the views hangs their children up — a terminal's view holds
        // the last `Rc<TermSession>` — and the agent statuses go with the
        // worktree, which is what `AppState` forgets on the same event.
        drop(self.panes.remove(worktree_id));
        self.layout.remove(worktree_id);
        // The readout is still listing that worktree's terminals until it is
        // told, and `state.json` is still holding its tabs.
        self.panes_changed(worktree_id, cx);
    }

    /// The `editor::Open` listener. Attached to the workspace's own element in
    /// `render`, which is the only place a window is in reach (see
    /// [`register_workspace_actions`]).
    fn handle_open_file(
        &mut self,
        action: &editor::Open,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_file(action, window, cx);
    }

    fn handle_open_search_result(
        &mut self,
        action: &editor::OpenSearchResult,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_search_result(action, window, cx);
    }

    /// Open one search hit and mark its matching text in the editor buffer.
    fn open_search_result(
        &mut self,
        action: &editor::OpenSearchResult,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_file(
            &editor::Open {
                path: action.path.clone(),
            },
            window,
            cx,
        );
        let Some(worktree_id) = self.state.read(cx).active_id().cloned() else {
            return;
        };
        let view = self
            .panes
            .get(&worktree_id)
            .into_iter()
            .flat_map(|panes| panes.values())
            .find_map(|pane| match pane {
                PaneView::Editor { view } if view.read(cx).path() == action.path => {
                    Some(view.clone())
                }
                _ => None,
            });
        if let Some(view) = view {
            view.update(cx, |view, cx| {
                view.show_search_match(action.line, action.column, &action.query, window, cx)
            });
        }
    }

    /// `editor::Open`: the file tree's click, or any other way in.
    fn open_file(&mut self, action: &editor::Open, window: &mut Window, cx: &mut Context<Self>) {
        let Some(worktree_id) = self.state.read(cx).active_id().cloned() else {
            return;
        };

        // Already open: show it rather than open a second tab on one file.
        let existing = self
            .panes
            .get(&worktree_id)
            .into_iter()
            .flat_map(|panes| panes.iter())
            .find(|(_, pane)| pane.shows(cx, action.path.as_path()))
            .map(|(pane, _)| *pane);
        if let Some(pane) = existing {
            self.open_error = None;
            self.show_pane(&worktree_id, pane, window, cx);
            return;
        }

        match opener::open(&action.path) {
            Ok(Opened::Text(text)) => {
                let path = action.path.clone();
                // `EditorView::new` needs the window, which is why this method
                // has one: there is no way to build the state without it.
                let editor = cx.new(|cx| EditorView::new(window, cx, path, text));
                let pane = self.add_tab(&worktree_id, PaneView::Editor { view: editor });
                self.show_pane(&worktree_id, pane, window, cx);
                self.open_error = None;
                // The tabs are what a restart gets back: a tab that opened and
                // was not written is a tab the next session does not have.
                self.persist_tabs(cx);
            }
            Ok(Opened::Image) => {
                let path = action.path.clone();
                let image = cx.new(|cx| ImageView::new(path, cx));
                let pane = self.add_tab(&worktree_id, PaneView::Image { view: image });
                self.show_pane(&worktree_id, pane, window, cx);
                self.open_error = None;
                self.persist_tabs(cx);
            }
            Ok(Opened::Refused(refusal)) => {
                self.open_error = Some(refusal.message(&action.path));
                cx.notify();
            }
            Err(error) => {
                self.open_error = Some(format!("{}: {error:#}", action.path.display()));
                cx.notify();
            }
        }
    }

    /// `editor::Save`: write the pane with the keyboard back over its file, if it
    /// has one.
    fn save_active_tab(&mut self, cx: &mut Context<Self>) {
        let Some(worktree_id) = self.state.read(cx).active_id().cloned() else {
            return;
        };
        let Some(pane) = self
            .layout(&worktree_id)
            .and_then(|layout| layout.focused_pane())
        else {
            return;
        };
        let Some(view) = self.pane(&worktree_id, pane) else {
            return;
        };
        match view.save(cx) {
            Ok(()) => self.open_error = None,
            // Saving is the one place a write can fail with something worth
            // saying: a read-only file, a directory that went away.
            Err(error) => {
                self.open_error =
                    Some(t!("workspace.save_failed", error = format!("{error:#}")).to_string())
            }
        }
        // What the file just became is what git now reports: a save can turn an
        // untracked file into a modified one, and the file tree's letters and the
        // diff tab's list are read from this.
        self.state.update(cx, |state, cx| state.refresh_changes(cx));
        self.persist_tabs(cx);
        cx.notify()
    }

    /// `tab::Activate`: make a tab the visible one, handing the keyboard to the pane that had it
    /// there.
    fn activate_tab(&mut self, action: &tab::Activate, cx: &mut Context<Self>) {
        if self.set_active_index(&action.worktree_id, action.index) {
            cx.notify();
        }
    }

    /// Put a pane on screen and give it the keyboard: what opening a file or a
    /// diff does with the pane it just made, and what a click on a bar row does
    /// with the pane that tab already had.
    fn show_pane(
        &mut self,
        worktree_id: &WorktreeId,
        pane: split::Pane,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The graph is drawn over the editor area, so a pane shown while it is up
        // is a pane nobody can see: a click on one of the graph's own file rows
        // — or on a file in the tree beside it — would be a click that did
        // nothing. The pane takes the front, which is the road VSCode's diff tab
        // takes from its git graph, and the graph's own button is the way back.
        if self.graph_visible {
            self.graph_visible = false;
            cx.notify();
        }
        if let Some(layout) = self.layout_mut(worktree_id) {
            layout.focus_pane(pane);
        }
        self.load_visible_panes(window, cx);
        let Some(focus) = self
            .pane(worktree_id, pane)
            .and_then(|view| view.focus_handle(cx))
        else {
            return;
        };
        // A diff pane re-reads when it is shown: it says what git says now, and
        // that is one `git diff` per look rather than one per frame. This is a
        // window-carrying path, which is what the reload needs.
        self.reload_diff(worktree_id, pane, window, cx);
        window.focus(&focus, cx);
        cx.notify();
    }

    /// Re-load a pane if it is a diff, and do nothing otherwise. Where a diff
    /// gets its window: showing a pane, opening one, and its own constructor.
    fn reload_diff(
        &mut self,
        worktree_id: &WorktreeId,
        pane: split::Pane,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(PaneView::Diff { view }) = self.pane(worktree_id, pane) else {
            return;
        };
        let view = view.clone();
        view.update(cx, |view, cx| view.reload(window, cx));
    }

    /// `diff::Open`: show the active worktree's changes, in the tab that is already showing them if
    /// there is one.
    fn open_diff(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(view) = self.diff_tab(window, cx) {
            // The comparison this action names is the worktree's, so a commit
            // that was on screen is not what is being asked for now.
            view.update(cx, |view, cx| view.show_worktree(window, cx));
        }
    }

    /// `diff::OpenFile`: show one changed file, in the diff tab that is already there.
    fn show_change(
        &mut self,
        path: &str,
        kind: syro_core::git::ChangeKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = self.diff_tab(window, cx) else {
            return;
        };
        view.update(cx, |view, cx| view.show(path, kind, window, cx));
    }

    /// The active worktree's diff tab, shown. Created on the first ask, and the
    /// tab that is already there on every ask after that.
    fn diff_tab(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Entity<DiffView>> {
        let worktree = self.state.read(cx).active().cloned()?;
        let existing = self
            .panes
            .get(&worktree.id)
            .into_iter()
            .flat_map(|panes| panes.iter())
            .find(|(_, pane)| {
                matches!(
                    pane,
                    PaneView::Diff { .. }
                        | PaneView::Deferred {
                            spec: TabSpec::Diff { .. }
                        }
                )
            })
            .map(|(pane, _)| *pane);
        if let Some(pane) = existing {
            self.open_error = None;
            self.show_pane(&worktree.id, pane, window, cx);
            let PaneView::Diff { view } = self.pane(&worktree.id, pane)? else {
                return None;
            };
            return Some(view.clone());
        }
        // The list the view draws is the state's, so the state reads it first: a
        // diff tab may be the first thing to ask for it since a save.
        self.state.update(cx, |state, cx| state.refresh_changes(cx));
        let view = cx.new(|cx| DiffView::new(window, cx, self.state.clone(), &worktree));
        let pane = self.add_tab(&worktree.id, PaneView::Diff { view: view.clone() });
        self.show_pane(&worktree.id, pane, window, cx);
        self.open_error = None;
        self.persist_tabs(cx);
        Some(view)
    }

    fn handle_open_diff(
        &mut self,
        action: &state::diff::Open,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let _ = action;
        self.open_diff(window, cx);
    }

    /// The source control panel's row: the same tab, another file.
    fn handle_open_change(
        &mut self,
        action: &state::diff::OpenFile,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (path, kind) = (action.path.clone(), action.kind);
        self.show_change(&path, kind, window, cx);
    }

    /// A row under a commit, in the source control panel or in the graph: the
    /// same tab, showing what that commit did to that file.
    fn handle_open_commit_change(
        &mut self,
        action: &state::diff::OpenCommitFile,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_commit_file_action(action, window, cx);
    }

    pub fn open_commit_file_action(
        &mut self,
        action: &state::diff::OpenCommitFile,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if action.new_tab {
            self.open_commit_file_in_new_tab(&action.commit, &action.path, window, cx);
        } else {
            self.show_commit_file(&action.commit, &action.path, window, cx);
        }
    }

    fn open_commit_file_in_new_tab(
        &mut self,
        commit: &str,
        path: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(worktree) = self.state.read(cx).active().cloned() else {
            return;
        };
        let existing = self
            .panes
            .get(&worktree.id)
            .into_iter()
            .flat_map(|panes| panes.iter())
            .find(|(_, pane)| {
                matches!(pane, PaneView::Diff { view } if view.read(cx).shows_commit(commit, path))
            })
            .map(|(pane, _)| *pane);
        if let Some(pane) = existing {
            self.show_pane(&worktree.id, pane, window, cx);
            return;
        }
        self.state.update(cx, |state, cx| state.refresh_changes(cx));
        let view = cx.new(|cx| DiffView::new(window, cx, self.state.clone(), &worktree));
        let pane = self.add_tab(&worktree.id, PaneView::Diff { view: view.clone() });
        self.show_pane(&worktree.id, pane, window, cx);
        view.update(cx, |view, cx| view.show_commit(commit, path, window, cx));
        self.open_error = None;
        self.persist_tabs(cx);
    }

    /// One commit's change to one file, in the diff tab that is already there.
    /// `pub` because the app-level listener for the same action — the one a
    /// dialog's rows reach — has no element to dispatch from.
    pub fn show_commit_file(
        &mut self,
        commit: &str,
        path: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = self.diff_tab(window, cx) else {
            return;
        };
        let (commit, path) = (commit.to_string(), path.to_string());
        view.update(cx, |view, cx| view.show_commit(&commit, &path, window, cx));
    }

    /// `gitgraph::Open`: the full-window graph of every branch. One view, kept
    /// across openings: it is a reading of the repository, and the reader's place
    /// in it is worth more than the frame it was built on.
    fn handle_open_graph(
        &mut self,
        _action: &state::gitgraph::Open,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_graph(window, cx);
    }

    /// `gitgraph::Close`: the way out, beside the `×` in the graph's own toolbar
    /// and the Escape key.
    fn handle_close_graph(
        &mut self,
        _action: &state::gitgraph::Close,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.graph_visible {
            self.graph_visible = false;
            self.graph = None;
            self.ensure_pane_focus(window, cx);
            cx.notify();
        }
    }

    /// Put the graph over the window. The whole window and not a pane: the graph
    /// is every branch, which is not a thing to read in a column.
    pub fn open_graph(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let graph = match self.graph.clone() {
            Some(graph) => graph,
            None => {
                let state = self.state.clone();
                cx.new(|cx| git_graph::GitGraph::new(state, cx))
            }
        };
        self.graph = Some(graph.clone());
        self.graph_visible = true;
        // Read again on the way in: the view is kept across openings, and the
        // repository is not — a commit or a branch may have landed since the last
        // look.
        graph.update(cx, |graph, cx| graph.refresh(cx));
        // The focus follows the overlay, so Escape reaches the handler above:
        // the same road the tab bar's `+` menu takes.
        window.focus(&self.focus, cx);
        cx.notify();
    }

    /// `worktree::New`: the new-worktree form. Opened from an element listener
    /// because building it takes a window, and the app-level listener for a
    /// click's action has none (see `register_workspace_actions`).
    fn handle_worktree_new(
        &mut self,
        action: &worktree::New,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_new_worktree_form(&action.repo_id, window, cx);
    }

    /// Open one of the three lists.
    fn open_palette(&mut self, kind: palette::Kind, window: &mut Window, cx: &mut Context<Self>) {
        let ctx = self.palette_ctx(cx);

        let items = match kind {
            palette::Kind::Files => match self.active_worktree_path(cx) {
                // Listed once, when the palette opens: `git ls-files` on every
                // keystroke would be a process per character, and the set does
                // not change while a modal is up. Reopening relists.
                Some(root) => match syro_core::git::ls_files(&root) {
                    Ok(paths) => palette::file_items(&ctx, &root, paths),
                    Err(error) => vec![
                        palette::Item::new(
                            SharedString::from(
                                t!("workspace.git_ls_files_failed", error = error).to_string(),
                            ),
                            |_window, _cx| {},
                        )
                        .group(t!("palette.group.error")),
                    ],
                },
                None => Vec::new(),
            },
            palette::Kind::All => {
                let mut items = palette::repo_items(&ctx, self.repo_rows(cx));
                items.extend(palette::worktree_items(&ctx, self.worktree_rows(cx)));
                // The sessions the readout's last sweep read, rather than a
                // second scan of every transcript: the search lists what the
                // sweep already found, and its own cache keeps that fresh.
                items.extend(palette::session_items(
                    self.resource_bar.read(cx).sessions().to_vec(),
                ));
                items
            }
        };

        let form = cx.new(|cx| palette::Palette::new(kind, items, window, cx));
        window.open_dialog(cx, move |dialog, _, cx| {
            let form = form.clone();
            dialog
                // No title and no close corner: the field says what the list is,
                // and with no heading the X would float over the input. Escape and
                // a click outside are the ways out — the footer names Escape.
                .close_button(false)
                .w(px(840.))
                .bg(dialog_background(cx))
                // No scrim, like the settings form: the list floats over the
                // window rather than alerting over it, and the window behind it
                // is what the transparency setting was asked for. A click outside
                // still closes it — the palette measures its own box and answers
                // at the window level (see `Palette::render`), because the dimmed
                // backdrop that used to catch that click is what went away.
                .overlay(false)
                .content(move |content, _, _| content.child(form.clone()))
        });
    }

    /// Run a command by the name a `[keymap]` entry uses.
    pub fn run_named_action(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(spec) = palette::action_by_name(name) else {
            return;
        };
        let ctx = self.palette_ctx(cx);
        (spec.dispatch)(window, cx, &ctx);
    }

    /// What the commands have to hand: the entities, and the context the titles
    /// were built from.
    fn palette_ctx(&self, cx: &mut Context<Self>) -> palette::Ctx {
        let state = self.state.read(cx);
        let active_worktree = state.active_id().cloned();
        palette::Ctx {
            workspace: cx.weak_entity(),
            active_repo: state
                .active()
                .map(|worktree| worktree.repo_id.clone())
                .or_else(|| state.repos().first().map(|repo| repo.id.clone())),
            active_tab: active_worktree
                .as_ref()
                .and_then(|id| self.active_index(id)),
            active_worktree,
        }
    }

    fn active_worktree_path(&self, cx: &App) -> Option<PathBuf> {
        self.state
            .read(cx)
            .active()
            .map(|worktree| worktree.path.clone())
    }

    /// Every project, with the worktree that going to it means: the
    /// repository's own checkout, or the first worktree it has.
    fn repo_rows(&self, cx: &App) -> Vec<(RepoId, String, String, Option<WorktreeId>)> {
        let state = self.state.read(cx);
        state
            .repos()
            .iter()
            .map(|repo| {
                let worktrees: Vec<&Worktree> = state.worktrees_of(&repo.id).collect();
                let target = worktrees
                    .iter()
                    .find(|worktree| state.is_own_checkout(&worktree.id))
                    .or_else(|| worktrees.first())
                    .map(|worktree| worktree.id.clone());
                (
                    repo.id.clone(),
                    repo.name.clone(),
                    repo.path.display().to_string(),
                    target,
                )
            })
            .collect()
    }

    /// Every worktree of every repository, as the jump lists them.
    fn worktree_rows(&self, cx: &App) -> Vec<(WorktreeId, String, String, String)> {
        let state = self.state.read(cx);
        let mut rows = Vec::new();
        for repo in state.repos() {
            for worktree in state.worktrees_of(&repo.id) {
                rows.push((
                    worktree.id.clone(),
                    repo.name.clone(),
                    worktree.name.clone(),
                    worktree.branch.clone(),
                ));
            }
        }
        rows
    }

    /// Put the new-worktree form up.
    pub fn open_new_worktree_form(
        &mut self,
        repo_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let state = self.state.clone();
        let workspace = cx.weak_entity();
        let repo_id = repo_id.to_string();
        let form = cx.new(|cx| NewWorktree::new(state, workspace, &repo_id, window, cx));
        window.open_dialog(cx, move |dialog, _, cx| {
            // Cloned per call, because the dialog asks for its content on every
            // frame it is up and the form is a live view, not a snapshot.
            let form = form.clone();
            dialog
                .title(t!("dialog.new_worktree"))
                .w(px(420.))
                .bg(dialog_background(cx))
                .content(move |content, _, _| content.child(form.clone()))
        });
    }

    /// `repo::EditSharedPaths`: the project's shared-paths form.
    fn handle_edit_shared_paths(
        &mut self,
        action: &state::repo::EditSharedPaths,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let state = self.state.clone();
        let form = cx.new(|cx| dialogs::SharedPaths::new(state, &action.repo_id, window, cx));
        window.open_dialog(cx, move |dialog, _, cx| {
            let form = form.clone();
            dialog
                .title(t!("sidebar.shared_paths"))
                .w(px(560.))
                .bg(dialog_background(cx))
                .content(move |content, _, _| content.child(form.clone()))
        });
    }

    /// `repo::AskAdd`: the Add-project form, with its three doors (browse, clone,
    /// create) in one place.
    fn handle_add_project(
        &mut self,
        _action: &state::repo::AskAdd,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_add_project_form(window, cx);
    }

    /// The form itself. `pub` because the palette opens it too, and a palette row
    /// is inside a dialog: an action dispatched from there is not inside this
    /// view's element tree, so the listener above never sees it.
    pub fn open_add_project_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let state = self.state.clone();
        let form = cx.new(|cx| dialogs::AddProject::new(state, window, cx));
        window.open_dialog(cx, move |dialog, _, cx| {
            let form = form.clone();
            dialog
                .title(t!("dialog.add_project"))
                .w(px(460.))
                .bg(dialog_background(cx))
                .content(move |content, _, _| content.child(form.clone()))
        });
    }

    /// `worktree::AskRemove`: the confirmation, which is where the forced retry
    /// is offered when git refuses.
    fn handle_worktree_ask_remove(
        &mut self,
        action: &worktree::AskRemove,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_remove_worktree_form(&action.worktree_id, window, cx);
    }

    /// The removal confirmation. `pub` for the same reason as
    /// [`Self::open_new_worktree_form`].
    pub fn open_remove_worktree_form(
        &mut self,
        worktree_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let state = self.state.clone();
        let worktree_id = worktree_id.to_string();
        // The repo's own checkout cannot be removed as a worktree — git will not
        // take the main one — so the dialog for it is the project question, and
        // it says so in the title rather than in a line under it.
        let title = if state.read(cx).is_own_checkout(&worktree_id) {
            t!("dialog.remove_project")
        } else {
            t!("dialog.remove_worktree")
        };
        let form =
            cx.new(|cx| RemoveWorktree::new(state, &worktree_id, window.window_handle(), cx));
        window.open_dialog(cx, move |dialog, _, cx| {
            let form = form.clone();
            dialog
                .title(title.clone())
                .w(px(420.))
                .bg(dialog_background(cx))
                .content(move |content, _, _| content.child(form.clone()))
        });
    }

    /// `tab::Close`: drop the whole tab — every pane it is made of, with their
    /// children hung up. Closing a terminal ends its shell: the view is the last
    /// owner of the session, and `Drop` hangs the child up.
    fn close_tab(&mut self, worktree_id: &WorktreeId, index: usize, cx: &mut Context<Self>) {
        let gone = match self.layout_mut(worktree_id) {
            Some(layout) => layout.remove_tab(index),
            None => return,
        };
        if gone.is_empty() {
            return;
        }
        for pane in gone {
            self.drop_pane(worktree_id, pane, cx);
        }
        // Whoever had the keyboard may have been in the tab that just went away;
        // the layout has already moved the tab on screen to one that exists.
        self.focus_pending = true;
        self.panes_changed(worktree_id, cx);
    }

    /// The tab bar's "close all files": every tab with no terminal in it (agent
    /// or shell) goes. A tab with unsaved edits stays, so nothing is lost.
    fn close_file_tabs(&mut self, worktree_id: &WorktreeId, cx: &mut Context<Self>) {
        let Some(layout) = self.layout(worktree_id) else {
            return;
        };
        let closable: Vec<usize> = (0..layout.tab_count())
            .filter(|tab| {
                layout.panes_of(*tab).into_iter().all(|pane| {
                    self.pane(worktree_id, pane)
                        .is_some_and(|view| !view.is_terminal() && !view.is_dirty(cx))
                })
            })
            .collect();
        // From the end, so each index is still the tab it named.
        for tab in closable.into_iter().rev() {
            self.close_tab(worktree_id, tab, cx);
        }
    }

    /// A tab dropped on another in the bar takes that one's place.
    fn move_tab(
        &mut self,
        worktree_id: &WorktreeId,
        from: usize,
        to: usize,
        cx: &mut Context<Self>,
    ) {
        if self
            .layout_mut(worktree_id)
            .is_some_and(|layout| layout.move_tab(from, to))
        {
            self.panes_changed(worktree_id, cx);
        }
    }

    /// Write the tab list out, for the restart that will not have these views.
    fn persist_tabs(&mut self, cx: &mut Context<Self>) {
        let live: Vec<TabLayout> = self
            .panes
            .keys()
            .flat_map(|worktree_id| self.tab_layouts(worktree_id, cx))
            .collect();
        let covered: Vec<WorktreeId> = self.panes.keys().cloned().collect();
        let carried = self
            .state
            .read(cx)
            .tabs()
            .iter()
            .filter(|tab| {
                match tab
                    .specs()
                    .first()
                    .and_then(|spec| self.worktree_of(spec, cx))
                {
                    // A spec we cannot place is kept: not knowing where it belongs
                    // is not a reason to drop what the user had open.
                    None => !live.contains(tab),
                    Some(id) => !covered.contains(&id),
                }
            })
            .cloned()
            .collect::<Vec<TabLayout>>();

        self.state.update(cx, |state, cx| {
            let mut all = live;
            all.extend(carried);
            state.set_tabs(all, cx)
        });
    }

    /// Which worktree a saved spec belongs to, so the carry-over above can tell
    /// whose list it is. The rule itself lives in [`restore::worktree_of`],
    /// because restoring asks the same question and the two must not drift.
    fn worktree_of(&self, spec: &TabSpec, cx: &App) -> Option<WorktreeId> {
        restore::worktree_of(spec, self.state.read(cx).worktrees())
    }

    /// What to draw when there is no pane: the reason it says, or the terminal's
    /// own error, which is the thing worth saying when there is one.
    fn empty_message(&self, why: String, colors: &ColorTokens) -> AnyElement {
        match &self.terminal_error {
            Some(error) => message(error.clone(), colors),
            None => message(why, colors),
        }
    }

    /// The pane area: the tab on screen and the panes it is made of (see [`crate::split`]).
    fn body(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let colors = Theme::global(cx).tokens.colors.clone();
        let Some(worktree_id) = self.state.read(cx).active_id().cloned() else {
            // Nothing on screen. Which nothing it is decides what to say: no
            // project at all is a different thing from none of them being picked.
            let why = if self.state.read(cx).repos().is_empty() {
                t!("workspace.no_repo")
            } else {
                t!("workspace.no_active_worktree")
            };
            return self.empty_message(why.to_string(), &colors);
        };
        let Some(layout) = self.layout(&worktree_id) else {
            // A worktree whose first frame has not run yet.
            return self.empty_message(t!("workspace.opening").to_string(), &colors);
        };
        let tab = layout.current();
        let Some(root) = layout.root(tab) else {
            // No tabs at all: the repository and the worktree are both there, and
            // nothing is open in it.
            return self.empty_message(t!("workspace.no_tabs").to_string(), &colors);
        };
        if let split::Node::Pane { pane } = root {
            // One pane, which is what almost every tab is: nothing to divide,
            // and no divider for a tracker to listen for.
            return self.pane_element(&worktree_id, *pane, &split::Path::new(), &colors, cx);
        }

        // The tracker paints nothing and is out of layout: it exists for the
        // paint-time call that registers this frame's window-level listeners.
        let tracker_view = cx.weak_entity();
        let tracker_worktree = worktree_id.clone();
        let tracker = canvas(
            |_bounds, _window, _cx| (),
            move |_bounds, _state, window, _cx| {
                resize::follow_pointer(window, &tracker_view, move |workspace, pointer, cx| {
                    let Some(layout) = workspace.layout_mut(&tracker_worktree) else {
                        return;
                    };
                    let moved = match pointer {
                        // The axis the divider divides is the axis the pointer is read along — the same one the mouse-
                        // down above read.
                        Some(position) => {
                            let at = match layout.dragging_axis() {
                                Some(split::Axis::Stacked) => position.y.as_f32(),
                                Some(split::Axis::SideBySide) | None => position.x.as_f32(),
                            };
                            layout.drag_to(at)
                        }
                        // The button came up: the drag ends wherever the
                        // pointer got to.
                        None => {
                            layout.end_drag();
                            true
                        }
                    };
                    if moved {
                        cx.notify();
                    }
                });
            },
        )
        .absolute()
        .top(px(0.))
        .left(px(0.))
        // `size_full`, not a zero-size point: a zero-area element is not
        // painted, and this one exists only for what its paint closure does.
        .size_full();

        div()
            .id("pane-area")
            .test_support()
            .relative()
            .flex_1()
            .min_h_0()
            .min_w_0()
            // A flex column, not a plain block: the tree below takes its height
            // from `flex_1`, which a block container ignores — a pane that was
            // sized by nothing would be a pane with no height.
            .flex()
            .flex_col()
            .child(tracker)
            .child(self.pane_node(&worktree_id, tab, root, &split::Path::new(), &colors, cx))
            .into_any_element()
    }

    /// One pane: what it holds, and the two things a click in it says.
    fn pane_element(
        &self,
        worktree_id: &WorktreeId,
        pane: split::Pane,
        path: &split::Path,
        colors: &ColorTokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let content = match self.pane(worktree_id, pane) {
            Some(view) => view.element(),
            None => message(t!("workspace.empty_pane").to_string(), colors),
        };
        let for_left = (worktree_id.clone(), pane);
        let for_right = (worktree_id.clone(), pane);
        let mut pane_element = div()
            .id(split::key("pane", path))
            .test_support()
            // Positioned, like the divider: a positioned element is hit-tested
            // above the static siblings it overlaps, and the tracker spans the
            // whole pane area.
            .relative()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _event, window, cx| {
                    this.focus_pane(&for_left.0, for_left.1, window, cx)
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, _event, window, cx| {
                    this.focus_pane(&for_right.0, for_right.1, window, cx)
                }),
            )
            .child(content);
        // The `×` is for the *tree*: a pane is the thing a split added, and with
        // two or more of them each needs a way out. A tab with a single pane
        // closes from the bar, where it has a row.
        let in_a_split = self
            .layout(worktree_id)
            .is_some_and(|layout| layout.panes_of(layout.current()).len() > 1);
        if in_a_split {
            pane_element = pane_element.child(self.pane_close_button(pane, path, colors, cx));
        }
        pane_element.into_any_element()
    }

    /// One node of the pane tree: a pane, or the two subtrees and the divider a split is made of.
    fn pane_node(
        &self,
        worktree_id: &WorktreeId,
        tab: usize,
        node: &split::Node,
        path: &split::Path,
        colors: &ColorTokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let split::Node::Split {
            axis,
            size,
            first,
            second,
            ..
        } = node
        else {
            let split::Node::Pane { pane } = node else {
                unreachable!("a node is a pane or a split")
            };
            return self.pane_element(worktree_id, *pane, path, colors, cx);
        };
        let Some(container) = self
            .layout(worktree_id)
            .and_then(|layout| layout.container(tab, path))
        else {
            unreachable!("the node above is a split")
        };

        let side_by_side = *axis == split::Axis::SideBySide;
        // The first child's size along this split's own axis: a width when side
        // by side, a height when stacked.
        let along = size.width();
        let dragging = self.layout(worktree_id).is_some_and(|layout| {
            layout
                .dragging()
                .is_some_and(|(drag_tab, drag_path)| *drag_tab == tab && drag_path == path)
        });
        let mut first_path = path.clone();
        first_path.push(false);
        let mut second_path = path.clone();
        second_path.push(true);

        // The first child is a *basis* rather than a fixed size, and both children have a minimum: a
        // divider dragged past the far edge of a window that has since shrunk, or a split
        // inside a split whose container is smaller than the size it remembers, must squeeze
        // the panes rather than draw one of them at nothing.
        let measured = container.clone();
        let recorder = canvas(
            |_, _, _| (),
            move |bounds, _state, _window, _cx| {
                let size = if side_by_side {
                    bounds.size.width
                } else {
                    bounds.size.height
                };
                measured.set(size.as_f32());
            },
        )
        .absolute()
        .top(px(0.))
        .left(px(0.))
        // `size_full`, not a point: a zero-area element is not painted, and this
        // one exists only for the bounds its paint closure is handed.
        .size_full();

        let first_half = div()
            // Positioned, like the panes and the divider: the recorder above is
            // positioned too, and a static sibling sits under it.
            .relative()
            .flex_shrink_1()
            .min_w_0()
            .min_h_0()
            .when(side_by_side, |el| {
                el.flex_basis(px(along)).min_w(px(split::MIN_PANE))
            })
            .when(!side_by_side, |el| {
                el.flex_basis(px(along)).min_h(px(split::MIN_PANE))
            })
            // A column, so the pane inside it can take the whole half with
            // `flex_1` in either orientation of the split above it.
            .flex()
            .flex_col()
            .child(self.pane_node(worktree_id, tab, first, &first_path, colors, cx));

        let for_divider = worktree_id.clone();
        let divider_tab = tab;
        let divider_path = path.clone();
        let divider = div()
            .id(split::key("pane-divider", path))
            .test_support()
            .aria_label(t!("workspace.aria.resize_panes"))
            // Positioned, like the panes: a positioned element is hit-tested
            // above the static tracker that spans the area.
            .relative()
            .flex_shrink_0()
            // Its own direction is the axis it divides: the line inside it takes
            // its length from `flex_1` along that axis, so nothing here is a
            // percentage of a parent that was sized by flex.
            .flex()
            .when(side_by_side, |el| el.flex_col())
            .when(!side_by_side, |el| el.flex_row())
            .items_center()
            .justify_center()
            .when(side_by_side, |el| {
                el.w(px(split::DIVIDER)).cursor_col_resize()
            })
            .when(!side_by_side, |el| {
                el.h(px(split::DIVIDER)).cursor_row_resize()
            })
            // The panes' own fill, so that the seam is one line and not a band: a
            // region with the window's background once where the panes have it
            // twice is transparent by a whole layer more than they are.
            .bg(colors.background)
            .hover(|el| el.bg(colors.border))
            .child(
                // The line shows the drag the way the sidebar's and the tree's
                // handles do: the seam goes to the accent colour while it is
                // being moved.
                div()
                    .flex_1()
                    .when(side_by_side, |el| el.w(px(1.)))
                    .when(!side_by_side, |el| el.h(px(1.)))
                    .bg(if dragging {
                        colors.primary
                    } else {
                        colors.border
                    }),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, _window, cx| {
                    // Stacked reads the y: the same arithmetic either way.
                    let at = if side_by_side {
                        event.position.x.as_f32()
                    } else {
                        event.position.y.as_f32()
                    };
                    this.begin_split_drag(&for_divider, divider_tab, &divider_path, at, cx)
                }),
            );

        let second_half = div()
            .relative()
            .flex_1()
            .when(side_by_side, |el| el.min_w(px(split::MIN_PANE)))
            .when(!side_by_side, |el| el.min_h(px(split::MIN_PANE)))
            .flex()
            .flex_col()
            .child(self.pane_node(worktree_id, tab, second, &second_path, colors, cx));

        div()
            .id(split::key("pane-split", path))
            .test_support()
            // Positioned, like the panes and the divider: the tracker below the
            // pane area is positioned too, and a static sibling would sit under
            // it — where the pointer never reaches it.
            .relative()
            .flex_1()
            .min_h_0()
            .min_w_0()
            // A container smaller than what its two panes need clips them
            // rather than pushing them out of the window.
            .overflow_hidden()
            // `flex_row`/`flex_col` rather than the `h_flex`/`v_flex` shortcuts:
            // those centre on the cross axis, and a centred pane is as tall as
            // its content — which for a terminal is nothing.
            .flex()
            .when(side_by_side, |el| el.flex_row())
            .when(!side_by_side, |el| el.flex_col())
            .child(recorder)
            .child(first_half)
            .child(divider)
            .child(second_half)
            .into_any_element()
    }

    /// The `×` in a split pane's corner.
    fn pane_close_button(
        &self,
        pane: split::Pane,
        path: &split::Path,
        colors: &ColorTokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .id(split::key("pane-close", path))
            .test_support()
            .aria_label(t!("pane.close"))
            .absolute()
            .top(px(4.))
            .right(px(8.))
            .size(px(18.))
            .flex()
            .items_center()
            .justify_center()
            .rounded_sm()
            .cursor_pointer()
            .text_color(colors.muted_foreground)
            .hover(|el| el.bg(colors.muted).text_color(colors.foreground))
            .child(Icon::new(IconName::Close).size(px(12.)))
            .on_click(cx.listener(move |_this, _event, window, cx| {
                // The click is the button's, not the pane's: the pane's own
                // listener is what moves the keyboard, and this click means
                // "close it", not "focus it".
                cx.stop_propagation();
                window.dispatch_action(Box::new(split::ClosePaneAt { pane }), cx);
            }))
            .into_any_element()
    }

    /// The pointer went into a pane, so the keyboard follows it. Which tab the
    /// bar marks, where a new tab goes, which pane a split divides and what
    /// `cmd-s` saves are all this one flag.
    fn focus_pane(
        &mut self,
        worktree_id: &WorktreeId,
        pane: split::Pane,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(layout) = self.layout_mut(worktree_id) else {
            return;
        };
        if !layout.focus_pane(pane) {
            return;
        }
        // The *window's* focus follows, and this path has a window: a click in a
        // pane is a user saying "the keyboard goes here", and moving only the
        // layout's flag leaves the keys going to the pane beside it.
        self.focus_focused_pane(worktree_id, window, cx);
        cx.notify();
    }

    /// Give the window's keyboard to the pane the layout says has it. `false`
    /// when there is no such pane — a worktree whose tabs are not built yet.
    fn focus_focused_pane(
        &self,
        worktree_id: &WorktreeId,
        window: &mut Window,
        cx: &mut App,
    ) -> bool {
        let Some(pane) = self
            .layout(worktree_id)
            .and_then(|layout| layout.focused_pane())
        else {
            return false;
        };
        let Some(view) = self.pane(worktree_id, pane) else {
            return false;
        };
        let Some(focus) = view.focus_handle(cx) else {
            return false;
        };
        window.focus(&focus, cx);
        true
    }

    fn begin_split_drag(
        &mut self,
        worktree_id: &WorktreeId,
        tab: usize,
        path: &split::Path,
        at: f32,
        cx: &mut Context<Self>,
    ) {
        let Some(layout) = self.layout_mut(worktree_id) else {
            return;
        };
        if layout.begin_drag(tab, path, at) {
            cx.notify();
        }
    }

    /// `settings::Open`: the settings form. Needs a window, so it is handled by
    /// an element listener rather than an app-level one (see
    /// [`register_workspace_actions`]).
    fn handle_settings_open(
        &mut self,
        _action: &settings_view::Open,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_settings_form(window, cx);
    }

    /// The form itself. `pub` because the palette opens it too, and a palette
    /// row is inside a dialog: an action dispatched from there is not inside
    /// this view's element tree, so the listener above never sees it.
    pub fn open_settings_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let state = self.state.clone();
        let form = cx.new(|cx| settings_view::SettingsView::new(state, window, cx));
        // Roomier than a dialog's default, which is the point of this one: two
        // panes side by side, and a page of controls to read.
        window.open_dialog(cx, move |dialog, window, cx| {
            let form = form.clone();
            dialog
                .title(t!("dialog.settings"))
                .w(px(920.))
                .h((window.viewport_size().height - px(96.)).clamp(px(320.), px(680.)))
                .bg(dialog_background(cx))
                // No scrim: the form is a panel over the window rather than an
                // alert over it, and the window behind it is what the
                // transparency setting was asked for. Every change lands as it is
                // made, so the title bar's close button and Escape are the ways
                // out — the dimmed area was also the click-away target.
                .overlay(false)
                .content(move |content, _, _| {
                    content.min_h_0().overflow_hidden().child(form.clone())
                })
        });
    }

    /// The tab bar: the active worktree's tabs, the visible one marked, each
    /// with a close button.
    fn tab_bar(&self, cx: &mut Context<Self>, colors: &ColorTokens) -> AnyElement {
        let Some(worktree_id) = self.state.read(cx).active_id().cloned() else {
            return div().into_any_element();
        };
        let active = self.active_index(&worktree_id);
        let names = self.agent_names(&worktree_id, cx);
        // The rows are read before anything mutates, because a click handler takes `&mut self` and
        // cannot hold a borrow of it.
        let rows: Vec<TabRow> = self
            .layout(&worktree_id)
            .map(|layout| {
                (0..layout.tab_count())
                    .filter_map(|tab| {
                        let focused = layout.tab_focused_pane(tab)?;
                        let view = self.pane(&worktree_id, focused)?;
                        Some(TabRow {
                            title: match names.get(&focused) {
                                Some(name) => name.text.clone().into(),
                                // A shell has no agent to be named after, and its
                                // title is where it is.
                                None => view.title(cx),
                            },
                            logo: match names.get(&focused) {
                                Some(name) => name.logo,
                                // A shell runs no agent, but it is still a
                                // terminal: the glyph says which kind of tab it
                                // is. An editor or a diff draws none.
                                None if matches!(view, PaneView::Terminal { .. }) => {
                                    Some(TERMINAL_LOGO)
                                }
                                None => None,
                            },
                            aria: names
                                .get(&focused)
                                .map(|name| name.aria.clone().into())
                                .unwrap_or_else(|| view.title(cx)),
                            dirty: view.is_dirty(cx),
                            axis: layout.tab_axis(tab),
                            // The first pane of the tab whose child is gone: the
                            // restart affordance is per tab, and a tab with two
                            // exited panes offers them one at a time.
                            exited: layout.panes_of(tab).into_iter().find(|pane| {
                                self.pane(&worktree_id, *pane)
                                    .is_some_and(|view| view.exited(cx))
                            }),
                            live_agent: layout.panes_of(tab).into_iter().any(|pane| {
                                self.pane(&worktree_id, pane).is_some_and(|view| {
                                    view.agent_name(cx).is_some() && !view.exited(cx)
                                })
                            }),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();

        let mut bar = div()
            .id("tab-bar")
            .test_support()
            .h_flex()
            .flex_shrink_0()
            .w_full()
            // The titlebar strip is 36 px, and the file tree's header is the
            // same: the toggle sits in one while the panel is down and in the
            // other while it is up, and anything else makes it jump.
            .h(px(TAB_BAR_HEIGHT))
            .border_b_1()
            .border_color(colors.border)
            .bg(colors.surface)
            // A bar with more tabs than fit keeps them inside its own box: the column can no longer grow to
            // hold them (see the note on `workspace-body`), so without this they would paint
            // over the sidebar beside them.
            .overflow_hidden()
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, event: &MouseDownEvent, window, cx| {
                    this.tab_menu = Some(event.position);
                    // Escape closes it, the same road the `+` menu takes.
                    window.focus(&this.focus, cx);
                    cx.notify();
                }),
            );

        // And a `+` at the trailing end: it opens a menu of new tabs — a terminal,
        // or one of the configured agents.
        let add = div()
            .id("add-tab")
            .test_support()
            .aria_label(t!("workspace.aria.new_tab"))
            .h_flex()
            .items_center()
            .justify_center()
            .size(px(22.))
            .flex_shrink_0()
            .mx_1()
            .rounded_sm()
            .cursor_pointer()
            .text_color(colors.muted_foreground)
            .hover(|el| el.bg(colors.muted))
            .relative()
            .child(Icon::new(IconName::Plus).size(px(14.)))
            // Where the button is, so its menu can hang under it.
            .child({
                let cell = self.add_button.clone();
                canvas(move |bounds, _, _| cell.set(bounds), |_, _, _, _| {})
                    .absolute()
                    .size_full()
            })
            .on_click(cx.listener(|this, event: &ClickEvent, window, cx| {
                let button = this.add_button.get();
                let at = if button.size.width > px(0.) {
                    // Right edge of the `+`, just below the bar it is centred in.
                    point(
                        button.right(),
                        button.center().y + px(TAB_BAR_HEIGHT / 2. + 4.),
                    )
                } else {
                    event.position()
                };
                this.open_add_menu(at, window, cx)
            }));

        // The tabs live in a strip of their own, and the strip scrolls when there
        // are more tabs than fit. The `+` is outside it, so it stays put however
        // far the tabs have been pushed along.
        let mut strip = div()
            .id("tab-strip")
            .test_support()
            .h_flex()
            .items_center()
            .flex_1()
            // A flex item will not go below its content's width without this: the
            // strip would grow and push the panels beside it out instead of
            // scrolling.
            .min_w_0()
            .h_full()
            .overflow_x_scroll();

        if self.graph.is_some() && !self.graph_visible {
            strip = strip.child(
                div()
                    .id("git-graph-tab")
                    .test_support()
                    .aria_label("Git Graph")
                    .aria_selected(false)
                    .h_flex()
                    .items_center()
                    .flex_shrink_0()
                    .px_2()
                    .h_full()
                    .cursor_pointer()
                    .border_r_1()
                    .border_color(colors.border)
                    .hover(|el| el.bg(colors.muted))
                    .text_xs()
                    .text_color(colors.muted_foreground)
                    .child("Git Graph")
                    .on_click(cx.listener(|this, _event, window, cx| {
                        this.open_graph(window, cx);
                    })),
            );
        }

        // No split buttons in this bar: the split's entries are in the pane's

        for (index, row) in rows.into_iter().enumerate() {
            let is_active = active == Some(index);
            let TabRow {
                title,
                logo,
                aria,
                dirty,
                axis,
                exited,
                live_agent,
            } = row;
            // Which way this tab is split, for the mark below: the row carries
            // the glyph of the axis it is divided along, and only when it is.
            let split_axis = axis.map(|axis| match axis {
                split::Axis::SideBySide => IconName::PanelRight,
                split::Axis::Stacked => IconName::PanelBottom,
            });
            // The tab's own colour, which the name's logo shares: a muted mark
            // beside a bright one would read as a different thing.
            let tab_color = if is_active {
                colors.foreground
            } else {
                colors.muted_foreground
            };
            let mut tab = div()
                .id(SharedString::from(format!("tab-{index}")))
                .test_support()
                .aria_label(aria.clone())
                .aria_selected(is_active)
                .h_flex()
                // Natural width, and the strip scrolls to the rest: a tab that
                // shrank to fit would show a row of ellipses instead of names.
                .flex_shrink_0()
                .items_center()
                .gap_1()
                .px_2()
                .h_full()
                .cursor_pointer()
                .border_r_1()
                .border_color(colors.border)
                // No fill for the active one: the bar's own is the colour it would
                // be, and a second fill of it makes this tab more opaque than the
                // bar around it.
                .hover(|el| el.bg(colors.muted))
                .text_xs()
                .text_color(tab_color);

            // The mark goes *before* the name: a tab made of more than one pane
            // carries the glyph of the axis it is divided along. A lone pane has
            // no mark — nothing is split.
            if let Some(axis) = split_axis {
                tab = tab.child(
                    div()
                        .id(SharedString::from(format!("tab-onscreen-{index}")))
                        .test_support()
                        .role(Role::Status)
                        .aria_label(t!("workspace.aria.in_split"))
                        .flex_shrink_0()
                        .text_color(colors.muted_foreground)
                        .child(Icon::new(axis).size(px(12.))),
                );
            }

            // The agent's logo takes the place of its name in the row, so the
            // title beside it is only the task.
            if let Some(logo) = logo {
                tab = tab.child(crate::agent_logo::logo_icon(logo, tab_color));
            }

            tab = tab.child(title.clone());

            if dirty {
                // A dot and not a word: a tab bar is a row of names, and the
                // unsaved marker is the one status every editor shows the same.
                tab = tab.child(
                    div()
                        .id(SharedString::from(format!("tab-dirty-{index}")))
                        .test_support()
                        .aria_label(t!("workspace.aria.unsaved"))
                        .child("•"),
                );
            }

            if exited.is_some() {
                // A child is gone, so the tab says so where the tab is. The
                // word is the accessible name and the glyph is what fits in a
                // row of tab titles.
                tab = tab.child(
                    div()
                        .id(SharedString::from(format!("tab-exited-{index}")))
                        .test_support()
                        .role(Role::Status)
                        .aria_label(t!("workspace.aria.exited"))
                        .text_color(colors.muted_foreground)
                        .child("⏻"),
                );
            }

            let for_close = worktree_id.clone();
            let for_activate = worktree_id.clone();
            let for_restart = worktree_id.clone();
            let for_drop = worktree_id.clone();
            let muted = colors.muted;
            // Drag a tab onto another to put it there.
            tab = tab
                .on_drag(
                    DraggedTab {
                        worktree_id: worktree_id.clone(),
                        index,
                        title: title.clone(),
                    },
                    |dragged, _offset, _window, cx| {
                        cx.new(|_| file_tree::DragChip {
                            name: dragged.title.to_string(),
                        })
                    },
                )
                .drag_over::<DraggedTab>(move |style, _, _, _| style.bg(muted))
                .on_drop(cx.listener(move |this, dragged: &DraggedTab, _window, cx| {
                    if dragged.worktree_id == for_drop {
                        this.move_tab(&for_drop, dragged.index, index, cx);
                    }
                }));
            if let Some(pane) = exited {
                // Restart respawns from the tab's own spec: same cwd, same agent.
                tab = tab.child(
                    div()
                        .id(SharedString::from(format!("tab-restart-{index}")))
                        .test_support()
                        .aria_label(t!("workspace.aria.restart"))
                        .px_1()
                        .rounded_sm()
                        .text_color(colors.muted_foreground)
                        .hover(|el| el.bg(colors.border))
                        .child(Icon::new(IconName::RotateCw).size(px(12.)))
                        .on_click(cx.listener(move |_this, _event, window, cx| {
                            // The click is this button's, not the tab's.
                            cx.stop_propagation();
                            // `Restart`, not `New`: the pane whose child exited
                            // is the one this button belongs to, and it keeps its
                            // place in its tab and in the split.
                            let _ = index;
                            window.dispatch_action(
                                Box::new(terminal::Restart {
                                    worktree_id: for_restart.clone(),
                                    pane,
                                }),
                                cx,
                            );
                        })),
                );
            }
            strip = strip.child(
                tab.child(
                    div()
                        .id(SharedString::from(format!("tab-close-{index}")))
                        .test_support()
                        .aria_label(t!("workspace.aria.close_tab"))
                        .px_1()
                        .rounded_sm()
                        .hover(|el| el.bg(colors.border))
                        .child(Icon::new(IconName::Close).size(px(12.)))
                        .on_click(cx.listener(move |_this, _event, window, cx| {
                            // Closing must not also activate the tab under the
                            // cursor: the click is the button's.
                            cx.stop_propagation();
                            let close = tab::Close {
                                worktree_id: for_close.clone(),
                                index,
                            };
                            if !live_agent {
                                window.dispatch_action(Box::new(close), cx);
                                return;
                            }
                            // An agent's session is easy to lose to a stray click.
                            window.open_alert_dialog(cx, move |dialog, _, cx| {
                                let close = close.clone();
                                dialog
                                    .confirm()
                                    // The alert is a page to read, like every
                                    // other dialog here: solid at any window
                                    // opacity (see `dialog_background`).
                                    .bg(dialog_background(cx))
                                    .title(t!("dialog.close_agent_tab"))
                                    .description(t!("dialog.close_agent_tab_question"))
                                    .ok_text(t!("dialog.close"))
                                    .on_ok(move |_, window, cx| {
                                        window.dispatch_action(Box::new(close.clone()), cx);
                                        true
                                    })
                            });
                        })),
                )
                .on_click(cx.listener(move |_this, _event, window, cx| {
                    // The tab's own action: the workspace moves the keyboard to
                    // the pane that had it in that tab, on the next frame.
                    cx.stop_propagation();
                    window.dispatch_action(
                        Box::new(tab::Activate {
                            worktree_id: for_activate.clone(),
                            index,
                        }),
                        cx,
                    );
                })),
            );
        }

        // The strip fills the bar (`flex_1`), which pushes the `+` after it to
        // the far right, outside the scroll.
        bar = bar.child(strip).child(add);
        // While the sidebar is down its toggle lives at the bar's end, after the
        // `+`, so the two take turns in the row instead of painting over each other.
        if self.panel.is_none() {
            bar = bar.child(
                div()
                    .id("panel-toggle-slot")
                    .test_support()
                    .h_flex()
                    .items_center()
                    .flex_shrink_0()
                    .pr_2()
                    .child(right_sidebar::toggle_button(colors, cx)),
            );
        }

        // No Diff button at the trailing end: the changed files are in the
        // source control panel, and that is where a reader picks one.
        bar.into_any_element()
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Restore the layout first, then load only the documents this frame shows.
        self.restore_and_ensure(cx);
        self.load_visible_panes(window, cx);
        self.ensure_pane_focus(window, cx);
        let colors = Theme::global(cx).tokens.colors.clone();
        let mut column = div()
            .id("workspace-body")
            .test_support()
            .v_flex()
            .relative()
            .flex_1()
            // `flex_1` in a row is the width; without a height the column takes its content's — which is
            // the tab bar, and the pane under it then gets nothing.
            .h_full()
            .min_h_0()
            // And `min_w_0` for the same reason in the other axis: a `flex_1` item's automatic minimum is
            // its *content's* width, so one wide row — a long tab title, a terminal that has
            // not been resized yet — makes the pane area wider than the space it was given and
            // pushes the right sidebar out of the window, where nothing in it can be clicked.
            .min_w_0()
            .when(!self.graph_visible, |column| {
                column.child(self.tab_bar(cx, &colors))
            });

        if let Some(at) = self.add_menu {
            column = column.child(self.add_menu_overlay(at, window, cx));
        }
        if let Some(at) = self.tab_menu {
            column = column.child(self.tab_menu_overlay(at, window, cx));
        }

        if let Some(error) = self.open_error.clone() {
            column = column.child(
                div()
                    .id("open-error")
                    // Both, because the label is what a screen reader reads and
                    // gpui does not derive one from the text inside: without it
                    // the sentence is drawn and never announced.
                    .role(Role::Alert)
                    .aria_label(SharedString::from(error.clone()))
                    .test_support()
                    .w_full()
                    .px_2()
                    .py_1()
                    .text_xs()
                    .bg(colors.muted)
                    .text_color(colors.destructive)
                    .child(SharedString::from(error)),
            );
        }

        // A transparent graph must not reveal the tabs and panes it replaces.
        if !self.graph_visible {
            column = column.child(self.body(cx));
        }

        // The graph, over the panes: every branch is what it draws, and a pane
        // is a column wide. The sidebars and the readout stay where they are —
        // this is the editor area, which is where a log belongs, and the button
        // that opened it is still on screen.
        if self.graph_visible {
            if let Some(graph) = self.graph.clone() {
                column = column.child(
                    div()
                        .id("git-graph-overlay")
                        .test_support()
                        .bg(colors.background)
                        // Absolute, so its height is the pane area's — which is the
                        // height the rows inside it scroll in. A `flex_1` here would
                        // be a percentage of an auto-sized box, and the rows would be
                        // as tall as the history instead of the window.
                        .absolute()
                        .inset_0()
                        // A click inside the graph is the graph's: without this it
                        // would fall through to the pane under it.
                        .occlude()
                        .child(graph),
                );
            }
        }

        let root = div()
            .id("workspace")
            .size_full()
            .v_flex()
            // GPUI's Root already paints the window background; another fill breaks transparency.
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::handle_workspace_key))
            .on_action(cx.listener(Self::handle_add_project))
            .on_action(cx.listener(Self::handle_open_file))
            .on_action(cx.listener(Self::handle_open_search_result))
            .on_action(cx.listener(Self::handle_open_diff))
            .on_action(cx.listener(Self::handle_open_change))
            .on_action(cx.listener(Self::handle_open_commit_change))
            .on_action(cx.listener(Self::handle_open_graph))
            .on_action(cx.listener(Self::handle_close_graph))
            .on_action(cx.listener(Self::handle_worktree_new))
            .on_action(cx.listener(Self::handle_edit_shared_paths))
            .on_action(cx.listener(Self::handle_worktree_ask_remove))
            .on_action(cx.listener(Self::handle_settings_open));

        // With no worktree there is no tab bar to hold the toggle (see `tab_bar`),
        // so it floats at the top-right of the pane area instead.
        if self.panel.is_none() && self.state.read(cx).active_id().is_none() {
            column = column.child(
                div()
                    .id("panel-toggle-slot")
                    .test_support()
                    .absolute()
                    .top(px(0.))
                    .right(px(0.))
                    .h(px(TAB_BAR_HEIGHT))
                    .h_flex()
                    .items_center()
                    .pr_2()
                    .child(right_sidebar::toggle_button(&colors, cx)),
            );
        }

        // The row is what the sidebar, the panes and the tree share; the status bar is under all of it,
        // across the whole window.
        let mut row = div()
            .id("workspace-row")
            .h_flex()
            .flex_1()
            .min_h_0()
            .child(self.sidebar.clone())
            .child(column);
        // The right sidebar: the activity strip, the panel it selects, and the handle on its leading
        // edge.
        if let Some(panel) = self.panel {
            let mut sidebar = div()
                .id("right-sidebar")
                .test_support()
                .relative()
                .v_flex()
                .w(px(self.right_width.width()))
                .h_full()
                .flex_shrink_0()
                .bg(colors.surface)
                .border_l_1()
                // The seam's one line, and the drag's feedback: the handle beside
                // it draws no hairline of its own.
                .border_color(if self.right_width.dragging() {
                    colors.primary
                } else {
                    colors.border
                })
                .child(right_sidebar::strip(Some(panel), &colors, cx));
            sidebar = sidebar.child(match panel {
                Panel::Files => self.file_tree.clone().into_any_element(),
                Panel::Sessions => self.sessions.clone().into_any_element(),
                Panel::Git => self.git.clone().into_any_element(),
            });
            // The drag listeners, registered while the frame is painted:
            let tracker_view = cx.entity().downgrade();
            let tracker = canvas(
                |_bounds, _window, _cx| (),
                move |_bounds, _state, window, _cx| {
                    resize::follow_pointer(window, &tracker_view, |this, pointer, cx| {
                        this.resize_sidebar(pointer.map(|position| position.x.as_f32()), cx)
                    });
                },
            )
            .absolute()
            .top(px(0.))
            .left(px(0.))
            .size_full();
            sidebar = sidebar
                .child(tracker)
                .child(self.resize_handle(&colors, cx));
            row = row.child(sidebar);
        }

        // The readout is told what is running, rather than reaching in for it:
        // it re-measures when the list changes, and an unchanged list is not a
        // repaint.
        let live = self.live_terminals(cx);
        self.resource_bar
            .update(cx, |bar, cx| bar.set_terminals(live, cx));

        root.child(row).child(self.resource_bar.clone())
    }
}

/// A changed file's colour, from git's status letter.
fn status_color(status: char, colors: &ColorTokens, cx: &App) -> Hsla {
    let theme = cx.theme();
    match status {
        'D' => theme.danger,
        'A' | '?' => theme.success,
        'M' | 'R' | 'C' => theme.warning,
        _ => colors.muted_foreground,
    }
}

/// A centred line for a state where there is nothing to show.
fn message(text: String, colors: &ColorTokens) -> AnyElement {
    div()
        .id("workspace-message")
        .test_support()
        // The message is the only thing here, so it is also its own name: a
        // reader that stops at the box has read nothing.
        .aria_label(SharedString::from(text.clone()))
        .flex_1()
        // A message stands in for a pane, so it carries the pane's own fill: the
        // window's background alone would leave it a layer more transparent than
        // the panes it replaces.
        .bg(colors.background)
        .flex()
        .items_center()
        .justify_center()
        .text_sm()
        .child(SharedString::from(text))
        .into_any_element()
}

/// Dialogs and confirmation alerts use the same readable glass tint as menus.
///
/// A dialog gpui-kit opens with a builder of its own — `open_alert_dialog` — is
/// painted on the theme's background instead, and has to be given this by hand.
///
/// Called inside the dialog's builder, which runs every frame: a colour taken
/// once when the dialog opened stays the old theme's after an appearance switch.
fn dialog_background(cx: &App) -> Hsla {
    settings_view::popup_surface(Theme::global(cx).tokens.colors.background)
}

/// The past session a saved terminal named, ready to re-enter: its id, with the
/// transcript file the presets resume by. `None` when no transcript this app knows
/// exists yet — an agent that keeps none, or a session that never wrote a turn.
fn saved_session(
    roots: &session::SessionRoots,
    agent: &AgentSpec,
    cwd: &Path,
    id: &str,
) -> Option<SessionRef> {
    let kind = session::SessionAgent::from_id(agent.id.as_str())?;
    let path = session::transcript_of(roots, kind, cwd, id)?;
    Some(SessionRef {
        id: id.to_string(),
        path,
    })
}

/// The default session roots, from `$HOME`; `None` when there is no home to read.
fn session_roots() -> Option<session::SessionRoots> {
    std::env::var_os("HOME").map(|home| session::SessionRoots::from_home(Path::new(&home)))
}

/// Why this spec cannot be started, if it cannot.
fn with_settings(mut spec: TermSpec, settings: &syro_core::config::TerminalSettings) -> TermSpec {
    spec.scrollback = settings.scrollback as usize;
    spec
}

/// Whether `program` is on the `PATH` string `path`.
fn on_path(path: &str, program: &str) -> bool {
    std::env::split_paths(path).any(|dir| dir.join(program).is_file())
}

/// Why this spec cannot be started, if it cannot. Asked against the app's own
/// `PATH`; [`login_path`] is the second opinion.
fn unrunnable(spec: &TermSpec) -> Option<String> {
    if !spec.cwd.is_dir() {
        return Some(t!("workspace.gone", path = spec.cwd.display()).to_string());
    }
    let program = &spec.command;
    let found = if program.contains('/') {
        std::path::Path::new(program).is_file()
    } else {
        std::env::var_os("PATH").is_some_and(|path| on_path(&path.to_string_lossy(), program))
    };
    if found {
        None
    } else {
        Some(t!("workspace.not_runnable", program = program).to_string())
    }
}

/// The spec to start, or why it cannot be started.
///
/// [`unrunnable`] answers against the app's own `PATH`; a command it cannot find
/// is looked for once more on the login shell's, which is the only thing that
/// knows where an agent a version manager installed went.
fn startable(spec: TermSpec) -> Result<TermSpec, String> {
    let Some(reason) = unrunnable(&spec) else {
        return Ok(spec);
    };
    // A `PATH` can only fix half of what `unrunnable` answers — the command. A
    // worktree that is gone is gone.
    if !spec.cwd.is_dir() {
        return Err(reason);
    }
    match login_path(&spec.command) {
        Some(path) => {
            let mut spec = spec;
            spec.env.push(("PATH".to_string(), path));
            Ok(spec)
        }
        None => Err(reason),
    }
}

/// The login shell's `PATH`, when `program` is on it: what to start a command
/// the app's own `PATH` could not find with.
///
/// A bundle opened from Finder inherits launchd's environment, whose `PATH` is
/// `/usr/bin:/bin:/usr/sbin:/sbin` and none of what the user installed — `pi`
/// lives in a version manager's bin directory, so the agent is not found and the
/// click that asked for it does nothing at all. The login shell is the only thing
/// that knows where it went. Asked once per run, and only after the app's own
/// `PATH` has already failed, so a launch that can find its own commands pays
/// nothing for this.
///
/// `-i` as well as `-l`, because on zsh a version manager is set up in `.zshrc`,
/// which a login shell reads only when it is interactive: `zsh -lc 'echo $PATH'`
/// answers without it, and the command is missed all the same.
fn login_path(program: &str) -> Option<String> {
    static LOGIN: OnceLock<Option<String>> = OnceLock::new();
    let path = LOGIN.get_or_init(|| {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string());
        let out = std::process::Command::new(shell)
            .args(["-ilc", "echo $PATH"])
            .output()
            .ok()?;
        last_line(&String::from_utf8(out.stdout).ok()?)
    });
    path.clone().filter(|path| on_path(path, program))
}

/// The last line of `text` that is not blank: what the login shell's `echo`
/// printed. An rc file is free to print something of its own before it.
fn last_line(text: &str) -> Option<String> {
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .next_back()
        .map(|line| line.trim().to_string())
}

#[cfg(test)]
mod startable_tests {
    use super::{last_line, on_path, startable};
    use syro_core::term::{TermSize, TermSpec};

    /// One line is asked for, and a shell that printed something first must not
    /// hand its own noise over as the `PATH`.
    #[test]
    fn the_login_path_is_the_last_line_the_shell_printed() {
        assert_eq!(last_line("loading\n/a:/b\n").as_deref(), Some("/a:/b"));
        assert_eq!(last_line("/a:/b"), Some("/a:/b".to_string()));
        assert_eq!(last_line("loading\n\n"), Some("loading".to_string()));
        assert_eq!(last_line("\n"), None);
        assert_eq!(last_line(""), None);
    }

    /// A name is looked up on the `PATH` it is given; a directory that does not
    /// hold it, or does not exist, is not a hit.
    #[test]
    fn a_program_is_looked_up_on_the_path_it_is_given() {
        let dir = std::env::temp_dir();
        let name = format!("syro-on-path-test-{}", std::process::id());
        let program = dir.join(&name);
        std::fs::write(&program, "").unwrap();
        assert!(on_path(&dir.to_string_lossy(), &name));
        assert!(!on_path("/nowhere", &name));
        assert!(!on_path(&dir.to_string_lossy(), "syro-no-such-program"));
        std::fs::remove_file(&program).unwrap();
    }

    /// A worktree that is gone is not something a `PATH` can fix, and neither is
    /// a command that no shell has.
    #[test]
    fn a_spec_that_cannot_run_is_refused_with_its_reason() {
        let size = TermSize::new(80, 24);
        let gone = TermSpec::command("/nowhere/syro", size, "syro-no-such-program");
        assert!(startable(gone).is_err(), "a missing directory started");

        let nowhere = std::env::temp_dir();
        let unfindable = TermSpec::command(&nowhere, size, "syro-no-such-program");
        let reason = startable(unfindable).expect_err("an unfindable command started");
        assert!(
            reason.contains("syro-no-such-program"),
            "the reason does not name the command: {reason}"
        );
    }
}

/// The app's own keybindings (the `[keymap]` section is not wired yet; this is what the app
/// ships with).
pub fn app_keymap(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("cmd-q", Quit, None),
        KeyBinding::new("cmd-s", editor::Save, None),
        // The "New terminal" key (`Mod+T`), which is the tab bar's `+` as well.
        KeyBinding::new("cmd-t", terminal::Add, None),
        // "Show Source Control", which is literally what this key does: the panel,
        // not a diff tab.
        KeyBinding::new(
            "cmd-shift-g",
            right_sidebar::Show { panel: Panel::Git },
            None,
        ),
        // The "Terminal Panes" group: `Mod+D` splits right, `Mod+Shift+D` splits down,
        // `Mod+W` closes the active pane.
        KeyBinding::new("cmd-d", split::Right, None),
        KeyBinding::new("cmd-shift-d", split::Down, None),
        KeyBinding::new("cmd-w", split::ClosePane, None),
        // `cmd-j` jumps, and `cmd-shift-p` is the same search under the key a
        // command palette usually has: both open the one global list — projects,
        // worktrees and past sessions — and `cmd-p` stays the file quick open.
        KeyBinding::new(
            "cmd-p",
            palette::Open {
                kind: palette::Kind::Files,
            },
            None,
        ),
        KeyBinding::new(
            "cmd-j",
            palette::Open {
                kind: palette::Kind::All,
            },
            None,
        ),
        KeyBinding::new(
            "cmd-shift-p",
            palette::Open {
                kind: palette::Kind::All,
            },
            None,
        ),
        // Inside the palette, and only there: the dialog the palette sits in
        // binds Enter for its own Confirm, so the list has to claim it in a
        // context of its own (see `palette`'s `render`).
        KeyBinding::new("down", palette::Next, Some("palette")),
        KeyBinding::new("up", palette::Previous, Some("palette")),
        KeyBinding::new("enter", palette::Confirm, Some("palette")),
        // A terminal's Tab is the child's, not the window's focus order:
        // gpui-kit's Root moves to the next tab stop on `tab`, and these
        // `NoAction`s in the terminal's own context out-rank it. The keystroke
        // then falls through to `TerminalView::handle_key_down`, which writes
        // `\t` (`\x1b[Z` for Shift-Tab) to the child.
        KeyBinding::new("tab", NoAction {}, Some("terminal")),
        KeyBinding::new("shift-tab", NoAction {}, Some("terminal")),
    ]);
}

/// `app::Quit` — `cmd-q`. gpui-kit leaves quitting to the application, and with no
/// menu bar nothing else would ever end the process: the close button only hides.
#[derive(Clone, PartialEq, Debug, Action)]
#[action(namespace = app, no_json)]
pub struct Quit;

/// What the close button does: hide the app instead of closing the window, so the
/// terminals and their agents keep running and the Dock icon brings them back.
/// Closing the only window would leave a process with nothing to show and no way
/// to show it.
fn hide_on_close(window: &mut Window, cx: &mut App) {
    window.on_window_should_close(cx, |_, cx| {
        cx.hide();
        false
    });
}

/// The Dock icon (or a second launch) while the window is not on screen: bring it back.
fn reopen_windows(cx: &mut App) {
    cx.activate(true);
    for window in cx.windows() {
        let _ = window.update(cx, |_, window, _| window.activate_window());
    }
}

/// The workspace's own actions.
pub fn register_workspace_actions(workspace: &Entity<Workspace>, cx: &mut App) {
    let handle = workspace.clone();
    cx.on_action(move |action: &terminal::New, cx: &mut App| {
        handle.update(cx, |workspace, cx| workspace.start_terminal(action, cx));
    });

    let handle = workspace.clone();
    cx.on_action(move |action: &editor::Save, cx: &mut App| {
        let _ = action;
        handle.update(cx, |workspace, cx| workspace.save_active_tab(cx));
    });

    let handle = workspace.clone();
    cx.on_action(move |action: &tab::Activate, cx: &mut App| {
        handle.update(cx, |workspace, cx| workspace.activate_tab(action, cx));
    });

    // A session history row's click. App-level like `terminal::New`, and for the
    // same reason: starting a terminal needs no window.
    let handle = workspace.clone();
    cx.on_action(move |action: &session_panel::Resume, cx: &mut App| {
        handle.update(cx, |workspace, cx| workspace.resume_session(action, cx));
    });

    // `cmd-t` and the tab bar's `+`: a new terminal tab in the worktree on
    // screen. App-level like `terminal::New`, and field-less because neither the
    // key nor the button knows a worktree id.
    let handle = workspace.clone();
    cx.on_action(move |_action: &terminal::Add, cx: &mut App| {
        handle.update(cx, |workspace, cx| workspace.add_terminal_tab(cx));
    });

    let handle = workspace.clone();
    cx.on_action(move |action: &terminal::Restart, cx: &mut App| {
        handle.update(cx, |workspace, cx| workspace.restart_terminal(action, cx));
    });

    // The activity strip and the sidebar's own toggle. Handled here because the
    // workspace is the only view that holds all three panels: one of them is on
    // screen at a time, and "which one" is one field rather than three flags.
    let handle = workspace.clone();
    cx.on_action(move |action: &right_sidebar::Show, cx: &mut App| {
        handle.update(cx, |workspace, cx| workspace.show_panel(action.panel, cx));
    });

    let handle = workspace.clone();
    cx.on_action(move |_action: &right_sidebar::Toggle, cx: &mut App| {
        handle.update(cx, |workspace, cx| workspace.toggle_sidebar(cx));
    });

    // The split: app-level, like `terminal::New` — none of it needs a window,
    // and a keybinding is one of the ways in (the palette is the other).
    let handle = workspace.clone();
    cx.on_action(move |_action: &split::Right, cx: &mut App| {
        handle.update(cx, |workspace, cx| {
            workspace.open_split(split::Axis::SideBySide, cx)
        });
    });

    let handle = workspace.clone();
    cx.on_action(move |_action: &split::Down, cx: &mut App| {
        handle.update(cx, |workspace, cx| {
            workspace.open_split(split::Axis::Stacked, cx)
        });
    });

    let handle = workspace.clone();
    cx.on_action(move |_action: &split::ClosePane, cx: &mut App| {
        handle.update(cx, |workspace, cx| workspace.close_pane(cx));
    });

    let handle = workspace.clone();
    cx.on_action(move |action: &split::ClosePaneAt, cx: &mut App| {
        handle.update(cx, |workspace, cx| workspace.close_pane_at(action.pane, cx));
    });

    let handle = workspace.clone();
    cx.on_action(move |action: &tab::Close, cx: &mut App| {
        handle.update(cx, |workspace, cx| {
            workspace.close_tab(&action.worktree_id, action.index, cx)
        });
    });

    // `palette::Open` is app-level and deferred: it needs a window (building the query field's state takes
    // one) and it has no element listener, because a key is the only way in and
    // a key arrives whatever holds the focus.
    let handle = workspace.clone();
    cx.on_action(move |action: &palette::Open, cx: &mut App| {
        let kind = action.kind;
        let handle = handle.clone();
        cx.defer(move |cx| {
            let Some(window) = cx.active_window() else {
                return;
            };
            let _ = window.update(cx, |_, window, cx| {
                handle.update(cx, |workspace, cx| workspace.open_palette(kind, window, cx))
            });
        });
    });

    // What a `[keymap]` binding reaches: one action whose argument is the command's name, looked up
    // in `palette::actions`.
    let handle = workspace.clone();
    cx.on_action(move |action: &palette::Run, cx: &mut App| {
        let name = action.name.clone();
        let handle = handle.clone();
        cx.defer(move |cx| {
            let Some(window) = cx.active_window() else {
                return;
            };
            let _ = window.update(cx, |_, window, cx| {
                handle.update(cx, |workspace, cx| {
                    workspace.run_named_action(&name, window, cx)
                })
            });
        });
    });

    // `diff::OpenCommitFile` is app-level for a reason the other two do not have:
    // the row that dispatches it may be inside a dialog — the graph's own file
    // rows are — and a dialog's element tree is its own layer, so the workspace's
    // element listeners never see the action.
    let handle = workspace.clone();
    cx.on_action(move |action: &diff::OpenCommitFile, cx: &mut App| {
        let action = action.clone();
        let handle = handle.clone();
        cx.defer(move |cx| {
            let Some(window) = cx.active_window() else {
                return;
            };
            let _ = window.update(cx, |_, window, cx| {
                handle.update(cx, |workspace, cx| {
                    workspace.open_commit_file_action(&action, window, cx)
                })
            });
        });
    });

    // `diff::Open` twice on purpose: the element listener in `render` is what a click reaches, and
    // this is what a key reaches when the workspace's own subtree does not hold the focus — the
    // `cmd-shift-d` binding is the case that matters.
    let handle = workspace.clone();
    cx.on_action(move |action: &diff::Open, cx: &mut App| {
        let _ = action;
        let handle = handle.clone();
        cx.defer(move |cx| {
            let Some(window) = cx.active_window() else {
                return;
            };
            let _ = window.update(cx, |_, window, cx| {
                handle.update(cx, |workspace, cx| workspace.open_diff(window, cx))
            });
        });
    });
}

/// The smallest window this layout is legible in: the two panels and the pane
/// between them. Also what `fit_to_display` will not shrink past.
const MIN_WINDOW_WIDTH: f32 = 720.;
const MIN_WINDOW_HEIGHT: f32 = 480.;

/// A margin between the window (or its fit) and the edge of the screen, so it
/// does not sit flush against the Dock or the side of the display.
const WINDOW_MARGIN: f32 = 8.;

/// Shrink the window until all of it is on screen.
fn fit_to_display(window: &mut Window, cx: &mut App) {
    // The display the window opened on: `WindowOptions` names none, so gpui
    // placed it on the primary one and that is the screen to fit into.
    let Some(visible) = cx.primary_display().map(|display| display.visible_bounds()) else {
        return;
    };
    let frame = window.bounds();
    let over = size(
        (frame.right() + px(WINDOW_MARGIN) - visible.right()).max(px(0.)),
        (frame.bottom() + px(WINDOW_MARGIN) - visible.bottom()).max(px(0.)),
    );
    if over.width <= px(0.) && over.height <= px(0.) {
        return;
    }
    let content = window.viewport_size();
    window.resize(size(
        (content.width - over.width).max(px(MIN_WINDOW_WIDTH)),
        (content.height - over.height).max(px(MIN_WINDOW_HEIGHT)),
    ));
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    // The default gpui-kit icon set, so the chrome can draw icons instead of glyphs.
    let app = gpui_kit::application().with_assets(crate::agent_logo::AppAssets);
    app.on_reopen(reopen_windows);
    app.run(move |cx| {
        gpui_kit::init(cx);
        // Before anything can try to notify: gpui drops every system
        // notification when the app has no identity, and says nothing about it.
        cx.set_app_identity(BUNDLE_ID, APP_NAME);

        // A broken settings.toml is said out loud and then ignored: it is the
        // user's file and the app still has to open.
        let settings = match Settings::load() {
            Ok(settings) => settings,
            Err(error) => {
                eprintln!("syro: settings.toml ignored: {error:#}");
                Settings::default()
            }
        };

        // How the app looks, before the window opens: the appearance and the terminal's type from
        // `settings.toml`.
        settings_view::apply(cx, &settings);
        // And what it speaks, before the window draws a word of it. Separate
        // from `apply`: that one is the appearance and the type, and a test
        // that clicks a font button must not switch the test process's locale.
        i18n::apply(settings.language.as_deref());

        // The app's own keys first, then the user's on top — later bindings win
        // for the same keystroke, which is what makes an override an override.
        app_keymap(cx);
        cx.on_action(|_: &Quit, cx: &mut App| cx.quit());
        for name in palette::apply_keymap(cx, &settings.keymap) {
            // Said out loud at startup: a binding that does nothing is the
            // worst answer a keymap can give, and the user is not looking at
            // a terminal when they type it.
            let detail = format!("syro: [keymap] names an action that does not exist: {name}");
            tracing::warn!("{detail}");
            eprintln!("{detail}");
        }

        let (_window, workspace) = gpui_kit::open_window(
            WindowOptions {
                // gpui has no default here, and a window shrunk into the panel
                // edges is a window this layout cannot draw.
                window_min_size: Some(size(px(MIN_WINDOW_WIDTH), px(MIN_WINDOW_HEIGHT))),
                // The setting's platform background from the first frame: the fills are
                // already at its alpha, and this is the half of the transparency that
                // needs a window to set (see `settings_view::apply_opacity`).
                window_background: settings_view::window_background(
                    settings.background_opacity,
                    settings.background_blur,
                ),
                ..Default::default()
            },
            cx,
            |window, cx| {
                hide_on_close(window, cx);
                let state = cx.new(|cx| {
                    AppState::open(Store::default(), syro_core::workspaces_root(), cx)
                        .with_settings(settings)
                });
                // A window that follows the system follows it while it is open, not
                // only at start-up.
                let follows = state.downgrade();
                window
                    .observe_window_appearance(move |_, cx| {
                        let Some(state) = follows.upgrade() else {
                            return;
                        };
                        let settings = state.read(cx).settings().clone();
                        if settings.appearance.is_none() {
                            settings_view::apply(cx, &settings);
                        }
                    })
                    .detach();
                // Actions are the single command channel and are handled
                // app-wide, not on one view: the state is what they act on.
                state::register_actions(&state, cx);

                let workspace = cx.new(|cx| Workspace::new(cx, state));
                register_workspace_actions(&workspace, cx);
                workspace
            },
        )
        .expect("Failed to open window");
        // The window exists now, so it can be measured against the screen it
        // landed on. Nothing to do about one that is already gone.
        let _ = _window.update(cx, |_, window, cx| fit_to_display(window, cx));
        // Kept alive by the window's root view; this binding is what lets the
        // unused-variable check see that nothing else needs it here.
        let _ = workspace;
    });
}

/// The tab bar, the file tree and the panes, in a real (headless) window, driven the way a user
/// drives them: clicks on a row and a tab, and a key.
#[cfg(test)]
mod window_tests {
    use std::path::{Path, PathBuf};

    use gpui_kit::Focusable as _;
    use gpui_kit::InputEvent as _;
    use gpui_kit::base::Root;
    use gpui_kit::component::IconName;
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{
        AppContext, Bounds, Entity, Pixels, Point, ScrollDelta, TestAppContext, WindowBounds,
        WindowHandle, WindowOptions, point, px, size,
    };
    use syro_core::config::{ComputerAwakeMode, Settings};
    use syro_core::model::{AgentSpec, SplitAxis, State, TabLayout, TabSpec};
    use syro_core::session::{SessionRoots, Usage, UsageDay, UsageGroup, UsageSlice, UsageSummary};
    use syro_core::store::Store;

    use crate::pane::PaneItem as _;
    use crate::pane::terminal::TerminalView;
    use crate::right_sidebar::Panel;
    use crate::settings_view::MIN_OPACITY;
    use crate::split;
    use crate::state::fixture::{scratch_repo, temp_store};
    use crate::state::{AppState, branch_or_name, register_actions};
    use crate::{
        TermSize, TermSpec, Workspace, app_keymap, dialog_background, fit_to_display,
        register_workspace_actions, saved_session, unrunnable, with_settings,
    };

    /// A view with nothing in it, for the two tests that are about the window
    /// rather than about what is drawn in it.
    struct Blank;

    impl gpui_kit::Render for Blank {
        fn render(
            &mut self,
            _window: &mut gpui_kit::Window,
            _cx: &mut gpui_kit::Context<Self>,
        ) -> impl gpui_kit::IntoElement {
            gpui_kit::div()
        }
    }

    /// The files the tree tests look for, written into the repo's own checkout
    /// before the window opens: the tree reads a directory when it is expanded,
    /// so a file written after that is a file it has already looked past.
    const NOTES: &str = "notes.md";
    const NOTES_TEXT: &str = "the tree put this here\n";
    const IGNORED: &str = "scratch.tmp";
    const NUL: &str = "nul.bin";
    const BIG: &str = "big.bin";
    /// A real PNG, so an image test exercises the decoder and not only the
    /// pane. It is the app's own logo, which is already in the tree.
    const IMAGE: &str = "logo.png";

    struct Fixture {
        window: WindowHandle<Root>,
        state: Entity<AppState>,
        workspace: Entity<Workspace>,
        /// The directory the files above live in: the repo's own checkout,
        /// which is the worktree `git worktree list` reports first and the one
        /// therefore active.
        root: PathBuf,
        state_path: PathBuf,
        /// Where the settings form writes. A scratch file, because changing a
        /// setting in a test must not write the developer's own configuration.
        settings_path: PathBuf,
        _dir: PathBuf,
        _store_dir: PathBuf,
    }

    fn open(cx: &mut TestAppContext, tag: &str) -> Fixture {
        open_with_settings(cx, tag, Settings::default())
    }

    /// A restart: `state.json` already holds a tab list, so the first frame is
    /// what turns it back into views.
    fn open_restored(
        cx: &mut TestAppContext,
        tag: &str,
        seed: impl FnOnce(&Path, &[PathBuf]) -> Vec<TabSpec>,
    ) -> Fixture {
        open_with(cx, tag, Settings::default(), seed)
    }

    /// The same window with the user's own `settings.toml`, for the things that
    /// read it: a `[keymap]` override, an agent preset.
    fn open_with_settings(cx: &mut TestAppContext, tag: &str, settings: Settings) -> Fixture {
        open_with(cx, tag, settings, |_, _| Vec::new())
    }

    /// The same window with the saved state itself, for the settings that are
    /// not a tab list: the width a previous run was dragged to.
    fn open_seeded(cx: &mut TestAppContext, tag: &str, seed: impl FnOnce(&mut State)) -> Fixture {
        open_with_seeded(cx, tag, Settings::default(), |_, _| Vec::new(), seed)
    }

    /// The fixture, with the user's settings and a saved tab list to find on the first frame — a
    /// test needs whichever of the two it is about.
    fn open_with(
        cx: &mut TestAppContext,
        tag: &str,
        settings: Settings,
        seed: impl FnOnce(&Path, &[PathBuf]) -> Vec<TabSpec>,
    ) -> Fixture {
        open_with_seeded(cx, tag, settings, seed, |_| {})
    }

    fn open_with_seeded(
        cx: &mut TestAppContext,
        tag: &str,
        settings: Settings,
        seed: impl FnOnce(&Path, &[PathBuf]) -> Vec<TabSpec>,
        adjust: impl FnOnce(&mut State),
    ) -> Fixture {
        let (dir, repo, worktrees) = scratch_repo(tag);
        let root = dir.join("main");

        std::fs::write(root.join(NOTES), NOTES_TEXT).unwrap();
        std::fs::write(root.join(IMAGE), include_bytes!("../assets/logo.png")).unwrap();
        std::fs::write(root.join(".gitignore"), format!("{IGNORED}\n")).unwrap();
        std::fs::write(root.join(IGNORED), "not source\n").unwrap();
        std::fs::write(root.join(NUL), b"a\0b").unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src").join("main.rs"), "fn main() {}\n").unwrap();
        // Over the 5 MB editor limit, sparse so the test does not write 5 MB.
        let big = std::fs::File::create(root.join(BIG)).unwrap();
        big.set_len(crate::opener::MAX_TEXT_BYTES + 1).unwrap();
        drop(big);
        // Committed, because that is how a file gets into a repo and how
        // `git check-ignore` sees the ignore rule the way it would in use.
        for args in [
            ["add", "-A"].as_slice(),
            ["commit", "-q", "-m", "fixture files"].as_slice(),
        ] {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(&root)
                .args(args)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
        }

        let (state_path, store_dir) = temp_store(tag);
        let settings_path = dir.join("settings.toml");
        // The active worktree is named rather than left to "whichever git lists
        // first": the tests below place their specs against `repo.path`, and an
        // accident of ordering is not what they are testing.
        let tabs = seed(&repo.path, &worktrees);
        let mut saved = State {
            repos: vec![repo],
            tabs: tabs.into_iter().map(TabLayout::Pane).collect(),
            active_worktree: Some(
                root.canonicalize()
                    .unwrap_or_else(|_| root.clone())
                    .display()
                    .to_string(),
            ),
            ..Default::default()
        };
        adjust(&mut saved);
        Store::new(state_path.clone()).save(&saved).unwrap();

        let mut built: Option<(Entity<AppState>, Entity<Workspace>)> = None;
        let (window, _root) = cx.update(|cx| {
            let bounds = Bounds {
                origin: Point::default(),
                size: size(px(1200.), px(700.)),
            };
            let (window, root) = gpui_kit::open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    ..Default::default()
                },
                cx,
                |_, cx| {
                    // The same wiring `main` does: a test against a window nobody ships proves nothing.
                    gpui_kit::init(cx);
                    cx.set_app_identity(crate::BUNDLE_ID, crate::APP_NAME);
                    app_keymap(cx);
                    let state = cx.new(|cx| {
                        AppState::open(Store::new(state_path.clone()), dir.join("workspaces"), cx)
                            .with_settings(settings.clone())
                            // A scratch file, because the settings form writes
                            // whenever a setting changes: a test must not write the
                            // developer's own configuration.
                            .with_settings_path(settings_path.clone())
                    });
                    register_actions(&state, cx);
                    let workspace = cx.new(|cx| Workspace::new(cx, state.clone()));
                    register_workspace_actions(&workspace, cx);
                    built = Some((state, workspace.clone()));
                    workspace
                },
            )
            .expect("open test window");
            (window.downcast::<Root>().expect("Base Root"), root)
        });
        let (state, workspace) = built.expect("the workspace was built");
        let fixture = Fixture {
            window,
            state,
            workspace,
            root: root.canonicalize().unwrap_or(root),
            state_path,
            settings_path,
            _dir: dir,
            _store_dir: store_dir,
        };
        draw(cx, &fixture);
        // The fixture files are in the repo's own checkout, which git lists
        // first — if the active worktree were one of the two `worktree add`s,
        // every tree test below would be looking at the wrong directory.
        let active = cx.update(|cx| {
            fixture
                .state
                .read(cx)
                .active()
                .expect("something is active")
                .path
                .clone()
        });
        assert_eq!(
            active, fixture.root,
            "the active worktree is not the fixture's"
        );
        fixture
    }

    fn draw(cx: &mut TestAppContext, fixture: &Fixture) {
        // A deferred effect — closing a dialog, an action queued for the next cycle — runs *between*
        // frames, not inside one.
        cx.run_until_parked();
        cx.update_window(fixture.window.into(), |_, window, cx| {
            window.draw(cx).clear(cx);
        })
        .expect("draw a frame");
    }

    /// Click `id` until `done`, a few times at most.
    fn click_until(
        cx: &mut TestAppContext,
        fixture: &Fixture,
        id: String,
        done: impl Fn(&mut TestAppContext) -> bool,
    ) -> bool {
        for _ in 0..3 {
            click_visible(cx, fixture, &id);
            draw(cx, fixture);
            if draw_until(cx, fixture, &done) {
                return true;
            }
        }
        false
    }

    /// Click `id` in a frame that reports it clickable.
    fn click_visible(cx: &mut TestAppContext, fixture: &Fixture, id: &str) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut last = None;
        while std::time::Instant::now() < deadline {
            draw(cx, fixture);
            let found = cx
                .update_window(fixture.window.into(), {
                    let id = id.to_string();
                    move |_, window, _| {
                        window
                            .try_find(id)
                            .map(|element| (element.visible(), element.bounds()))
                    }
                })
                .expect("read the frame");
            if found.as_ref().is_some_and(|(visible, _)| *visible) {
                click(cx, fixture, id.to_string());
                return;
            }
            last = Some(found);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let context = cx
            .update_window(fixture.window.into(), |_, window, _| {
                [
                    "workspace-row",
                    "sidebar",
                    "workspace-body",
                    "tab-bar",
                    "pane",
                    "right-sidebar",
                    "git-panel",
                    "git-action",
                ]
                .map(|parent| {
                    (
                        parent,
                        window.try_find(parent).map(|element| element.bounds()),
                    )
                })
                .to_vec()
            })
            .expect("read the frame");
        panic!("{id} was never reported clickable: {last:?} in {context:?}");
    }

    /// Draw frames until `done`, the way a real event loop would: an action is
    /// dispatched on the next cycle and a git call lands after the background
    /// executor has run.
    fn draw_until(
        cx: &mut TestAppContext,
        fixture: &Fixture,
        done: impl Fn(&mut TestAppContext) -> bool,
    ) -> bool {
        // The budget is wall clock, not a frame count.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while std::time::Instant::now() < deadline {
            cx.background_executor
                .advance_clock(std::time::Duration::from_millis(20));
            cx.run_until_parked();
            draw(cx, fixture);
            if done(cx) {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        false
    }

    fn click(cx: &mut TestAppContext, fixture: &Fixture, id: String) {
        cx.update_window(fixture.window.into(), |_, window, cx| {
            window.click(id, cx);
        })
        .expect("click");
    }

    /// Right-click `id`: the secondary button is where a row's own operations
    /// live, and `click` is the primary one.
    fn right_click(cx: &mut TestAppContext, fixture: &Fixture, id: String) {
        cx.update_window(fixture.window.into(), |_, window, cx| {
            window.right_click(id, cx);
        })
        .expect("right click");
    }

    /// How many terminal tabs the active worktree has, which is what the `+` and
    /// `cmd-t` are measured by.
    fn terminal_tabs(cx: &mut TestAppContext, fixture: &Fixture) -> usize {
        cx.update(|cx| {
            fixture
                .workspace
                .read(cx)
                .active_tabs(cx)
                .iter()
                .flat_map(|tab| tab.specs())
                .filter(|spec| matches!(spec, TabSpec::Terminal { .. }))
                .count()
        })
    }

    /// The terminal rows the readout has for `worktree_id`, by the title each one
    /// is drawn with.
    fn readout_rows(cx: &mut TestAppContext, fixture: &Fixture, worktree_id: &str) -> Vec<String> {
        cx.update(|cx| {
            fixture
                .workspace
                .read(cx)
                .resource_bar()
                .read(cx)
                .worktree_usage(worktree_id)
                .map(|usage| {
                    usage
                        .terminals
                        .into_iter()
                        .map(|row| row.name.aria)
                        .collect()
                })
                .unwrap_or_default()
        })
    }

    fn found(cx: &mut TestAppContext, fixture: &Fixture, id: &str) -> bool {
        let id = id.to_string();
        cx.update_window(fixture.window.into(), move |_, window, _| {
            window.try_find(id).is_some()
        })
        .expect("read the frame")
    }

    fn label(cx: &mut TestAppContext, fixture: &Fixture, id: &str) -> Option<String> {
        let id = id.to_string();
        cx.update_window(fixture.window.into(), move |_, window, _| {
            window
                .try_find(id)
                .and_then(|el| el.label().map(str::to_string))
        })
        .expect("read the frame")
    }

    /// What a checkbox reports as checked, read off the control itself.
    fn checked(cx: &mut TestAppContext, fixture: &Fixture, id: &str) -> Option<bool> {
        let id = id.to_string();
        cx.update_window(fixture.window.into(), move |_, window, _| {
            window.try_find(id).and_then(|el| el.checked())
        })
        .expect("read the frame")
    }

    /// Where an element ended up, in window coordinates. Which side a panel
    /// sits on, and the two ends of a drag, are only readable here.
    fn bounds_of(cx: &mut TestAppContext, fixture: &Fixture, id: &str) -> Bounds<Pixels> {
        let wanted = id.to_string();
        cx.update_window(fixture.window.into(), |_, window, _| {
            window.try_find(wanted).map(|el| el.bounds())
        })
        .expect("read the frame")
        .unwrap_or_else(|| panic!("{id} is not on screen"))
    }

    fn row(path: &std::path::Path) -> String {
        format!("tree-{}", path.display())
    }

    /// The specs open in the worktree on screen, one entry per *pane*: the tab
    /// shape is the layout's business, and these tests are about what is open.
    fn active_tabs(cx: &mut TestAppContext, fixture: &Fixture) -> Vec<TabSpec> {
        cx.update(|cx| {
            fixture
                .workspace
                .read(cx)
                .active_tabs(cx)
                .iter()
                .flat_map(|tab| tab.specs().into_iter().cloned())
                .collect()
        })
    }

    /// How many tabs the bar has: what a split must *not* change.
    fn open_tabs(cx: &mut TestAppContext, fixture: &Fixture) -> usize {
        cx.update(|cx| fixture.workspace.read(cx).active_tabs(cx).len())
    }

    /// The specs of the tab on screen, in draw order: what a split test asserts
    /// about, since a split changes the panes of a tab and not the bar.
    fn on_screen(cx: &mut TestAppContext, fixture: &Fixture) -> Vec<TabSpec> {
        cx.update(|cx| fixture.workspace.read(cx).pane_specs(cx))
    }

    /// The same for a list read straight out of `state.json`.
    fn saved_specs(tabs: &[TabLayout]) -> Vec<TabSpec> {
        tabs.iter()
            .flat_map(|tab| tab.specs().into_iter().cloned())
            .collect()
    }

    fn file_tabs(tabs: &[TabSpec]) -> Vec<PathBuf> {
        tabs.iter()
            .filter_map(|spec| match spec {
                TabSpec::File { path } => Some(path.clone()),
                _ => None,
            })
            .collect()
    }

    /// The row the tree drew for `name` in the fixture's root.
    fn tree_row(fixture: &Fixture, name: &str) -> String {
        row(&fixture.root.join(name))
    }

    #[gpui_kit::test]
    fn the_tree_lists_the_files_and_flags_the_git_ignored_one(cx: &mut TestAppContext) {
        let fixture = open(cx, "tree-list");

        for name in [NOTES, "src", NUL, BIG] {
            assert!(
                found(cx, &fixture, &tree_row(&fixture, name)),
                "{name} is missing from the tree"
            );
        }
        assert_eq!(
            label(cx, &fixture, &tree_row(&fixture, NOTES)).as_deref(),
            Some(NOTES)
        );
        // Ignored is stated, not only dimmed: a colour is not something a
        // screen reader can read out, and it is not something a test can assert.
        assert_eq!(
            label(cx, &fixture, &tree_row(&fixture, IGNORED)).as_deref(),
            Some(format!("{IGNORED} (ignored by git)").as_str())
        );
        // `.git` is never a row.
        assert!(!found(cx, &fixture, &row(&fixture.root.join(".git"))));
    }

    #[gpui_kit::test]
    fn a_directory_expands_and_collapses(cx: &mut TestAppContext) {
        let fixture = open(cx, "tree-expand");
        let nested = row(&fixture.root.join("src").join("main.rs"));
        assert!(!found(cx, &fixture, &nested), "src starts collapsed");

        let dir = tree_row(&fixture, "src");
        click(cx, &fixture, dir.clone());
        draw(cx, &fixture);
        assert!(found(cx, &fixture, &nested), "src did not expand");

        click(cx, &fixture, dir.clone());
        draw(cx, &fixture);
        assert!(!found(cx, &fixture, &nested), "src did not collapse");

        // Re-opening reads it again, which is the point of forgetting on
        // collapse: a directory that changed on disk is re-read.
        std::fs::write(fixture.root.join("src").join("later.rs"), "// later\n").unwrap();
        click(cx, &fixture, dir);
        draw(cx, &fixture);
        assert!(
            found(
                cx,
                &fixture,
                &row(&fixture.root.join("src").join("later.rs"))
            ),
            "the re-read missed a file added while collapsed"
        );
    }

    #[gpui_kit::test]
    fn clicking_a_project_search_result_opens_the_absolute_file_path(cx: &mut TestAppContext) {
        let fixture = open(cx, "tree-search-open");
        let second_match = fixture.root.join("src").join("second.rs");
        std::fs::write(
            &second_match,
            "tree put this here first\ntree put this here second\n",
        )
        .unwrap();
        assert!(found(cx, &fixture, "tree-search-input"));
        click(cx, &fixture, "tree-search-input".into());
        type_text(cx, &fixture, "TREE put this here");
        assert!(
            draw_until(cx, &fixture, |cx| found(
                cx,
                &fixture,
                "tree-search-result-0"
            )),
            "the search result did not appear"
        );

        click(cx, &fixture, "tree-search-result-2".into());
        assert!(
            draw_until(cx, &fixture, |cx| file_tabs(&active_tabs(cx, &fixture))
                == vec![second_match.clone()]),
            "the result did not open its file: {:?}",
            file_tabs(&active_tabs(cx, &fixture))
        );
        assert!(found(cx, &fixture, "editor-pane"));
        assert!(!found(cx, &fixture, "open-error"));
        let editor = cx.update(|cx| {
            fixture
                .workspace
                .read(cx)
                .active_editor(cx)
                .expect("search result editor")
        });
        let count = cx.update(|cx| editor.read(cx).search_label(cx));
        assert_eq!(
            count, "2/2",
            "the clicked match was not selected in the editor search"
        );
        cx.update(|cx| editor.update(cx, |editor, cx| editor.move_search_match(false, cx)));
        assert_eq!(cx.update(|cx| editor.read(cx).search_label(cx)), "1/2");
        cx.update(|cx| editor.update(cx, |editor, cx| editor.move_search_match(true, cx)));
        assert_eq!(cx.update(|cx| editor.read(cx).search_label(cx)), "2/2");
    }

    #[gpui_kit::test]
    fn clicking_a_file_opens_an_editor_tab_holding_its_text(cx: &mut TestAppContext) {
        let fixture = open(cx, "tree-open");
        assert!(
            active_tabs(cx, &fixture).len() == 1,
            "the terminal is the first tab"
        );

        click(cx, &fixture, tree_row(&fixture, NOTES));
        assert!(
            draw_until(cx, &fixture, |cx| file_tabs(&active_tabs(cx, &fixture))
                .len()
                == 1),
            "no editor tab opened for {NOTES}"
        );

        let tabs = active_tabs(cx, &fixture);
        assert_eq!(file_tabs(&tabs), vec![fixture.root.join(NOTES)]);
        // The pane on screen is the editor, and the tab bar says so.
        assert!(
            found(cx, &fixture, "editor-pane"),
            "the editor is not the visible pane"
        );
        assert_eq!(
            label(cx, &fixture, "tab-1").as_deref(),
            Some(NOTES),
            "the tab does not carry the file's name"
        );
        assert!(found(cx, &fixture, "tab-bar"));
    }

    #[gpui_kit::test]
    fn an_image_opens_a_preview_of_itself(cx: &mut TestAppContext) {
        let fixture = open(cx, "tree-image");
        click(cx, &fixture, tree_row(&fixture, IMAGE));
        assert!(
            draw_until(cx, &fixture, |cx| file_tabs(&active_tabs(cx, &fixture))
                .len()
                == 1),
            "no tab opened for {IMAGE}"
        );

        // Neither the editor nor a refusal: the tab draws the picture.
        assert!(
            found(cx, &fixture, "image-pane"),
            "the image is not on screen"
        );
        assert!(!found(cx, &fixture, "editor-pane"));
        assert!(!found(cx, &fixture, "open-error"));
        assert_eq!(label(cx, &fixture, "tab-1").as_deref(), Some(IMAGE));
        assert_eq!(
            file_tabs(&active_tabs(cx, &fixture)),
            vec![fixture.root.join(IMAGE)]
        );
    }

    #[gpui_kit::test]
    fn a_markdown_file_switches_between_source_and_document(cx: &mut TestAppContext) {
        let fixture = open(cx, "tree-preview");
        click(cx, &fixture, tree_row(&fixture, NOTES));
        assert!(draw_until(cx, &fixture, |cx| file_tabs(&active_tabs(
            cx, &fixture
        ))
        .len()
            == 1));

        // The source opens — the keyboard is in the editor — with the switch to
        // the document beside it.
        assert!(found(cx, &fixture, "editor-pane"));
        assert!(!found(cx, &fixture, "editor-preview"));
        assert_eq!(
            label(cx, &fixture, "editor-preview-toggle").as_deref(),
            Some("Preview")
        );

        click_visible(cx, &fixture, "editor-preview-toggle");
        draw(cx, &fixture);
        assert!(
            found(cx, &fixture, "editor-preview"),
            "the document did not draw"
        );
        assert_eq!(
            label(cx, &fixture, "editor-preview-toggle").as_deref(),
            Some("Edit")
        );

        // And back, which is the half a switch is worth having for.
        click_visible(cx, &fixture, "editor-preview-toggle");
        draw(cx, &fixture);
        assert!(
            !found(cx, &fixture, "editor-preview"),
            "the source did not come back"
        );
    }

    #[gpui_kit::test]
    fn opening_the_same_file_twice_leaves_one_tab(cx: &mut TestAppContext) {
        let fixture = open(cx, "tree-twice");
        let target = tree_row(&fixture, NOTES);

        click(cx, &fixture, target.clone());
        assert!(draw_until(cx, &fixture, |cx| file_tabs(&active_tabs(
            cx, &fixture
        ))
        .len()
            == 1));
        click(cx, &fixture, target);
        draw(cx, &fixture);
        // Still one: the second click found the tab and showed it.
        assert_eq!(
            file_tabs(&active_tabs(cx, &fixture)).len(),
            1,
            "a second click opened a second tab"
        );
        assert!(
            !found(cx, &fixture, "tab-2"),
            "the tab bar grew a tab for a file that was already open"
        );
    }

    #[gpui_kit::test]
    fn the_tab_bar_menu_closes_the_files_and_keeps_the_terminal(cx: &mut TestAppContext) {
        let fixture = open(cx, "tree-close-files");
        click(cx, &fixture, tree_row(&fixture, NOTES));
        assert!(draw_until(cx, &fixture, |cx| file_tabs(&active_tabs(
            cx, &fixture
        ))
        .len()
            == 1));

        // A click outside the menu closes it.
        right_click(cx, &fixture, "tab-1".to_string());
        assert!(draw_until(cx, &fixture, |cx| found(
            cx, &fixture, "tab-menu"
        )));
        click(cx, &fixture, "tab-0".to_string());
        assert!(
            draw_until(cx, &fixture, |cx| !found(cx, &fixture, "tab-menu")),
            "a click outside did not close the menu"
        );

        right_click(cx, &fixture, "tab-1".to_string());
        assert!(draw_until(cx, &fixture, |cx| found(
            cx, &fixture, "tab-menu"
        )));
        click_visible(cx, &fixture, "tab-menu-close-files");
        assert!(draw_until(cx, &fixture, |cx| file_tabs(&active_tabs(
            cx, &fixture
        ))
        .is_empty()));
        assert_eq!(
            terminal_tabs(cx, &fixture),
            1,
            "the terminal was closed too"
        );
        assert!(!found(cx, &fixture, "tab-menu"), "the menu stayed open");
    }

    #[gpui_kit::test]
    fn two_tabs_switch_and_the_closed_one_goes_away(cx: &mut TestAppContext) {
        let fixture = open(cx, "tree-switch");
        click(cx, &fixture, tree_row(&fixture, NOTES));
        assert!(draw_until(cx, &fixture, |cx| file_tabs(&active_tabs(
            cx, &fixture
        ))
        .len()
            == 1));
        assert!(found(cx, &fixture, "editor-pane"));

        // Back to the terminal, and forward again: what the tab bar is for.
        click(cx, &fixture, "tab-0".to_string());
        draw(cx, &fixture);
        assert!(
            !found(cx, &fixture, "editor-pane"),
            "the editor is still the visible pane after clicking the terminal's tab"
        );

        click(cx, &fixture, "tab-1".to_string());
        draw(cx, &fixture);
        assert!(
            found(cx, &fixture, "editor-pane"),
            "the editor tab did not come back"
        );

        // Closing the editor leaves the terminal, which is the next tab along.
        click(cx, &fixture, "tab-close-1".to_string());
        assert!(
            draw_until(cx, &fixture, |cx| active_tabs(cx, &fixture).len() == 1),
            "closing the editor tab did not take it away"
        );
        assert!(!found(cx, &fixture, "tab-1"));
        assert!(
            !found(cx, &fixture, "editor-pane"),
            "the closed tab is still the visible pane"
        );
        assert!(found(cx, &fixture, "tab-0"));
    }

    /// A bar with more tabs than fit scrolls them, and the `+` stays where it is:
    /// a tab that shrank to fit would be a row of ellipses instead of names.
    #[gpui_kit::test]
    fn more_tabs_than_fit_scroll_sideways_in_the_tab_bar(cx: &mut TestAppContext) {
        let fixture = open(cx, "tab-bar-scroll");
        // Eight names, long enough to overflow the bar: it is the count that
        // does it, and not one title that happens to be very wide.
        for index in 0..8 {
            let path = fixture
                .root
                .join(format!("a-file-with-a-longer-name-{index:02}.md"));
            std::fs::write(&path, "one line\n").unwrap();
            cx.update_window(fixture.window.into(), |_, window, cx| {
                fixture.workspace.update(cx, |workspace, cx| {
                    workspace.open_path(path.clone(), None, window, cx);
                });
            })
            .expect("open a tab");
        }
        draw(cx, &fixture);

        // The terminal is tab 0, so the last file is tab 8.
        let strip = bounds_of(cx, &fixture, "tab-strip");
        let last = bounds_of(cx, &fixture, "tab-8");
        assert!(
            last.origin.x + last.size.width > strip.origin.x + strip.size.width,
            "the tabs do not overflow the strip: {strip:?}, {last:?}"
        );
        // But the strip itself stays the bar's width: it is the box the tabs
        // scroll in, not a box the tabs widen.
        let bar = bounds_of(cx, &fixture, "tab-bar");
        assert!(
            strip.origin.x + strip.size.width <= bar.origin.x + bar.size.width + px(1.),
            "the strip is wider than the bar: {bar:?}, {strip:?}"
        );

        // A wheel over the bar pushes the tabs along, and the `+` does not go
        // with them: it is the way to a new tab whatever the bar is showing.
        let plus = bounds_of(cx, &fixture, "add-tab");
        let before = bounds_of(cx, &fixture, "tab-0").origin.x;
        cx.update_window(fixture.window.into(), |_, window, cx| {
            window.scroll(
                "tab-strip",
                ScrollDelta::Pixels(point(px(-80.), px(0.))),
                cx,
            );
        })
        .expect("scroll the tab bar");
        let after = bounds_of(cx, &fixture, "tab-0").origin.x;
        assert!(
            after < before,
            "the strip did not scroll: {before:?} -> {after:?}"
        );
        assert_eq!(
            bounds_of(cx, &fixture, "add-tab"),
            plus,
            "the `+` scrolled away with the tabs"
        );
    }

    #[gpui_kit::test]
    fn editing_and_saving_writes_the_file_and_clears_the_marker(cx: &mut TestAppContext) {
        let fixture = open(cx, "tree-save");
        click(cx, &fixture, tree_row(&fixture, NOTES));
        assert!(draw_until(cx, &fixture, |cx| file_tabs(&active_tabs(
            cx, &fixture
        ))
        .len()
            == 1));

        // Typing goes to the editor because opening a tab puts the keyboard in
        // it — which is the behaviour this asserts as much as the write is.
        cx.update_window(fixture.window.into(), |_, window, cx| {
            window.input("edited ", cx);
        })
        .expect("type into the editor");
        draw(cx, &fixture);
        assert!(
            found(cx, &fixture, "tab-dirty-1"),
            "an edit did not mark the tab as having unsaved changes"
        );
        assert_eq!(
            std::fs::read_to_string(fixture.root.join(NOTES)).unwrap(),
            NOTES_TEXT,
            "the file on disk changed before the save"
        );

        cx.update_window(fixture.window.into(), |_, window, cx| {
            window.press("cmd-s", cx);
        })
        .expect("press cmd-s");
        assert!(
            draw_until(cx, &fixture, |cx| !found(cx, &fixture, "tab-dirty-1")),
            "the unsaved marker outlived the save"
        );
        let written = std::fs::read_to_string(fixture.root.join(NOTES)).unwrap();
        assert!(
            written.contains("edited "),
            "cmd-s did not write the buffer: {written:?}"
        );
        // And only the buffer's own edit: the rest of the file is the file.
        assert!(written.contains("the tree put this here"));
    }

    #[gpui_kit::test]
    fn an_oversized_and_a_binary_file_are_refused_with_a_message(cx: &mut TestAppContext) {
        let fixture = open(cx, "tree-refuse");
        let before = active_tabs(cx, &fixture).len();

        click(cx, &fixture, tree_row(&fixture, BIG));
        draw(cx, &fixture);
        let message = label(cx, &fixture, "open-error").expect("a refusal was not shown");
        assert!(
            message.contains("over the 5 MB editor limit"),
            "the refusal does not say why: {message:?}"
        );
        assert_eq!(
            active_tabs(cx, &fixture).len(),
            before,
            "an oversized file opened a tab"
        );

        click(cx, &fixture, tree_row(&fixture, NUL));
        draw(cx, &fixture);
        let message = label(cx, &fixture, "open-error").expect("the second refusal was not shown");
        assert!(
            message.contains("is not text"),
            "the binary refusal does not say why: {message:?}"
        );
        assert_eq!(
            active_tabs(cx, &fixture).len(),
            before,
            "a binary file opened a tab"
        );

        // A file that can be opened clears the complaint: it was about the last
        // click, not about the app.
        click(cx, &fixture, tree_row(&fixture, NOTES));
        assert!(
            draw_until(cx, &fixture, |cx| file_tabs(&active_tabs(cx, &fixture))
                .len()
                == 1),
            "the refusal stood in the way of a file that opens"
        );
        assert!(!found(cx, &fixture, "open-error"));
    }

    /// The diff tests' changes: one of each kind git reports — a modification to a committed file,
    /// an addition, and a deletion.
    fn change_three_files(fixture: &Fixture) {
        std::fs::write(fixture.root.join(NOTES), "rewritten\n").unwrap();
        std::fs::write(fixture.root.join("added.md"), "brand new\n").unwrap();
        git(&fixture.root, &["add", "added.md"]);
        std::fs::remove_file(fixture.root.join("README.md")).unwrap();
    }

    fn git(dir: &std::path::Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn active_worktree(cx: &mut TestAppContext, fixture: &Fixture) -> String {
        cx.update(|cx| {
            fixture
                .state
                .read(cx)
                .active_id()
                .expect("something is active")
                .clone()
        })
    }

    /// `workspace::Refresh`, then wait for the state to have read changes:
    fn refresh(cx: &mut TestAppContext, fixture: &Fixture) {
        cx.dispatch_action(fixture.window.into(), crate::state::Refresh);
        let id = active_worktree(cx, fixture);
        assert!(
            draw_until(cx, fixture, |cx| cx.update(|cx| !fixture
                .state
                .read(cx)
                .changes(&id)
                .is_empty())),
            "the refresh did not read the changed files"
        );
    }

    /// Show one file's diff the way a reader does: open the source control panel and click its row.
    fn show_diff(cx: &mut TestAppContext, fixture: &Fixture, path: &str) {
        show_panel(cx, fixture, "panel-git", Some(Panel::Git));
        let kind = cx.update(|cx| {
            let state = fixture.workspace.read(cx).state.read(cx);
            state
                .changes(state.active_id().unwrap())
                .iter()
                .find(|change| change.path == path)
                .unwrap()
                .kind
        });
        let id = match kind {
            syro_core::git::ChangeKind::Unstaged => format!("git-file-{path}"),
            kind => format!("git-file-{kind:?}-{path}"),
        };
        assert!(
            draw_until(cx, fixture, |cx| found(cx, fixture, &id)),
            "the source control panel does not list {path}"
        );
        let workspace = fixture.workspace.clone();
        let wanted = path.to_string();
        assert!(
            click_until(cx, fixture, id, move |cx| cx.update(|cx| workspace
                .read(cx)
                .active_diff(cx)
                .is_some_and(
                    |view| view.read(cx).selected() == Some(wanted.as_str())
                ))),
            "clicking {path} in the panel did not show its diff"
        );
    }

    /// Open the diff tab by dispatching `diff::Open`, which is what the palette row and any
    /// `[keymap]` binding do — the tab bar's own button is gone (the source control panel is
    /// the way in for a *file*, and this command is the way in for "the changes").
    fn open_diff(cx: &mut TestAppContext, fixture: &Fixture) {
        let focus = cx.update(|cx| fixture.workspace.read(cx).focus.clone());
        cx.update_window(fixture.window.into(), |_, window, cx| {
            window.focus(&focus, cx);
            window.dispatch_action(Box::new(crate::state::diff::Open), cx);
        })
        .expect("dispatch the diff action");
        assert!(
            draw_until(cx, fixture, |cx| cx.update(|cx| fixture
                .workspace
                .read(cx)
                .active_diff(cx)
                .is_some())),
            "the diff tab did not open"
        );
    }

    /// Right-click the terminal pane and wait for its menu: the split entries
    /// live there, and there is no split button in the chrome to click instead.
    fn open_pane_menu(cx: &mut TestAppContext, fixture: &Fixture) {
        let id = cx.update(|cx| {
            fixture
                .workspace
                .read(cx)
                .terminal_tab(cx)
                .expect("the worktree's terminal")
                .read(cx)
                .terminal_id()
                .clone()
        });
        cx.update_window(fixture.window.into(), |_, window, cx| {
            window.right_click(id, cx);
        })
        .expect("right-click the terminal");
        assert!(
            draw_until(cx, fixture, |cx| found(cx, fixture, "pane-menu")),
            "the pane menu did not open"
        );
    }

    fn diff_text(cx: &mut TestAppContext, fixture: &Fixture) -> String {
        cx.update(|cx| {
            fixture
                .workspace
                .read(cx)
                .active_diff(cx)
                .expect("a diff tab is on screen")
                .read(cx)
                .text(cx)
        })
    }

    fn diff_selected(cx: &mut TestAppContext, fixture: &Fixture) -> Option<String> {
        cx.update(|cx| {
            fixture
                .workspace
                .read(cx)
                .active_diff(cx)
                .expect("a diff tab is on screen")
                .read(cx)
                .selected()
                .map(str::to_string)
        })
    }

    /// The changed files are listed once, in the source control panel, with git's
    /// letter on each row — and the diff tab has no list of its own, which is
    /// what that panel is for.
    #[gpui_kit::test]
    fn the_source_control_panel_lists_the_changed_files_with_their_status(cx: &mut TestAppContext) {
        let fixture = open(cx, "diff-list");
        let panel = cx.update(|cx| fixture.workspace.read(cx).git.clone());
        change_three_files(&fixture);
        refresh(cx, &fixture);
        open_diff(cx, &fixture);
        show_panel(cx, &fixture, "panel-git", Some(Panel::Git));
        assert!(
            draw_until(cx, &fixture, |cx| cx
                .update(|cx| panel.read(cx).changes().len())
                == 3),
            "the panel never read the changes"
        );

        // The letter and the path, for each of the three. Found by id rather than
        // by index, because the order is git's to choose.
        for expected in ["M notes.md", "A added.md", "D README.md"] {
            let (_, path) = expected.split_once(' ').expect("a letter and a path");
            let kind_id = if path == "added.md" { "Staged-" } else { "" };
            assert_eq!(
                label(cx, &fixture, &format!("git-file-{kind_id}{path}")).as_deref(),
                Some(expected),
                "the panel's row for {path} does not carry its status"
            );
        }
        // The diff tab is named after the file it shows, whichever one the view
        // picked: a tab bar with two tabs called "Diff" says nothing about which
        // is which.
        let selected = diff_selected(cx, &fixture).expect("nothing was selected");
        let name = std::path::Path::new(&selected)
            .file_name()
            .expect("a file name")
            .to_string_lossy()
            .into_owned();
        assert_eq!(
            label(cx, &fixture, "tab-1").as_deref(),
            Some(name.as_str()),
            "the diff tab is not named after the file it shows"
        );
    }

    #[gpui_kit::test]
    fn a_clean_worktree_says_so(cx: &mut TestAppContext) {
        // The fixture commits everything it lays down, so this is the state the
        // tab is in most of the time on a tree nobody has touched.
        let fixture = open(cx, "diff-clean");
        open_diff(cx, &fixture);

        assert!(
            found(cx, &fixture, "diff-empty"),
            "the empty diff is not drawn"
        );
        assert_eq!(
            label(cx, &fixture, "diff-empty").as_deref(),
            Some("No changes")
        );
        assert!(diff_text(cx, &fixture).is_empty(), "an empty diff has text");
    }

    #[gpui_kit::test]
    fn selecting_a_changed_file_shows_its_diff(cx: &mut TestAppContext) {
        let fixture = open(cx, "diff-select");
        change_three_files(&fixture);
        refresh(cx, &fixture);

        show_diff(cx, &fixture, "notes.md");
        assert_eq!(diff_selected(cx, &fixture).as_deref(), Some("notes.md"));

        // Both sides of the change, from git's own diff.
        let text = diff_text(cx, &fixture);
        assert!(
            text.contains("+rewritten"),
            "the added line is not in the diff: {text:?}"
        );
        assert!(
            text.contains("-the tree put this here"),
            "the removed line is not in the diff: {text:?}"
        );

        // Selecting another file swaps the text rather than appending to it, and
        // it is the same tab: clicking through a list is looking at files, not
        // opening one tab per file.
        show_diff(cx, &fixture, "added.md");
        assert_eq!(diff_selected(cx, &fixture).as_deref(), Some("added.md"));
        assert_eq!(
            active_tabs(cx, &fixture)
                .iter()
                .filter(|spec| matches!(spec, TabSpec::Diff { .. }))
                .count(),
            1,
            "clicking a second file opened a second diff tab"
        );
        let added = diff_text(cx, &fixture);
        assert!(
            added.contains("+brand new") && !added.contains("-the tree put this here"),
            "the diff still shows the previous file: {added:?}"
        );
    }

    #[gpui_kit::test]
    fn the_git_panel_separates_commit_index_and_worktree_diffs(cx: &mut TestAppContext) {
        let fixture = open(cx, "git-change-kinds");
        let file = fixture.root.join("shared.md");
        std::fs::write(&file, "base\n").unwrap();
        git(&fixture.root, &["add", "shared.md"]);
        git(&fixture.root, &["commit", "-q", "-m", "base file"]);
        git(&fixture.root, &["checkout", "-q", "-b", "feature"]);

        std::fs::write(&file, "base\ncommitted\n").unwrap();
        git(&fixture.root, &["add", "shared.md"]);
        git(&fixture.root, &["commit", "-q", "-m", "committed change"]);
        std::fs::write(&file, "base\ncommitted\nstaged\n").unwrap();
        git(&fixture.root, &["add", "shared.md"]);
        std::fs::write(&file, "base\ncommitted\nstaged\nunstaged\n").unwrap();
        refresh(cx, &fixture);
        show_panel(cx, &fixture, "panel-git", Some(Panel::Git));

        for id in [
            "git-file-Committed-shared.md",
            "git-file-Staged-shared.md",
            "git-file-shared.md",
        ] {
            assert!(
                draw_until(cx, &fixture, |cx| found(cx, &fixture, id)),
                "missing {id}"
            );
        }

        let workspace = fixture.workspace.clone();
        assert!(click_until(
            cx,
            &fixture,
            "git-file-Committed-shared.md".into(),
            |cx| {
                cx.update(|cx| {
                    workspace
                        .read(cx)
                        .active_diff(cx)
                        .is_some_and(|view| view.read(cx).text(cx).contains("+committed"))
                })
            }
        ));
        let workspace = fixture.workspace.clone();
        assert!(click_until(
            cx,
            &fixture,
            "git-file-Staged-shared.md".into(),
            |cx| {
                cx.update(|cx| {
                    workspace
                        .read(cx)
                        .active_diff(cx)
                        .is_some_and(|view| view.read(cx).text(cx).contains("+staged"))
                })
            }
        ));
        let workspace = fixture.workspace.clone();
        assert!(click_until(
            cx,
            &fixture,
            "git-file-shared.md".into(),
            |cx| {
                cx.update(|cx| {
                    workspace
                        .read(cx)
                        .active_diff(cx)
                        .is_some_and(|view| view.read(cx).text(cx).contains("+unstaged"))
                })
            }
        ));
    }

    #[gpui_kit::test]
    fn the_git_panel_can_unstage_and_discard_with_confirmation(cx: &mut TestAppContext) {
        let fixture = open(cx, "git-undo-change");
        let staged_path = fixture.root.join("staged.txt");
        std::fs::write(&staged_path, "keep this file\n").unwrap();
        git(&fixture.root, &["add", "staged.txt"]);
        let discard_path = fixture.root.join("discard.txt");
        std::fs::write(&discard_path, "remove this file\n").unwrap();
        refresh(cx, &fixture);
        show_panel(cx, &fixture, "panel-git", Some(Panel::Git));

        let panel = cx.update(|cx| fixture.workspace.read(cx).git.clone());
        assert!(click_until(
            cx,
            &fixture,
            "git-unstage-staged.txt".into(),
            |cx| {
                cx.update(|cx| {
                    panel.read(cx).changes().iter().any(|change| {
                        change.path == "staged.txt"
                            && change.kind == syro_core::git::ChangeKind::Unstaged
                    })
                })
            }
        ));
        assert_eq!(
            std::fs::read_to_string(&staged_path).unwrap(),
            "keep this file\n"
        );

        assert!(click_until(
            cx,
            &fixture,
            "git-discard-discard.txt".into(),
            |cx| { found(cx, &fixture, "git-discard-confirm") }
        ));
        assert!(
            discard_path.exists(),
            "untracked file was deleted before confirmation"
        );
        click(cx, &fixture, "git-discard-cancel".into());
        assert!(draw_until(cx, &fixture, |cx| !found(
            cx,
            &fixture,
            "git-discard-confirm"
        )));
        assert!(
            discard_path.exists(),
            "cancelling the confirmation deleted the file"
        );

        assert!(click_until(
            cx,
            &fixture,
            "git-discard-discard.txt".into(),
            |cx| { found(cx, &fixture, "git-discard-confirm") }
        ));
        assert!(click_until(
            cx,
            &fixture,
            "git-discard-confirm-button".into(),
            |_cx| { !discard_path.exists() }
        ));
        assert!(!discard_path.exists());
    }

    #[gpui_kit::test]
    fn a_deletion_shows_as_removed_lines(cx: &mut TestAppContext) {
        let fixture = open(cx, "diff-delete");
        change_three_files(&fixture);
        refresh(cx, &fixture);
        open_diff(cx, &fixture);

        show_diff(cx, &fixture, "README.md");
        let text = diff_text(cx, &fixture);
        assert!(
            text.contains("-scratch"),
            "the deleted file's content is not shown as a removal: {text:?}"
        );
    }

    #[gpui_kit::test]
    fn a_diff_over_the_editor_limit_is_refused(cx: &mut TestAppContext) {
        let fixture = open(cx, "diff-huge");
        // Modified rather than added, so the diff is both sides of the file and
        // clears the limit. A third of the limit per side is enough: the two
        // lines together are two thirds of a rope the editor would have held.
        let big = fixture.root.join("big.md");
        std::fs::write(&big, "a".repeat(3 * 1024 * 1024)).unwrap();
        git(&fixture.root, &["add", "big.md"]);
        git(&fixture.root, &["commit", "-q", "-m", "big"]);
        std::fs::write(&big, "b".repeat(3 * 1024 * 1024)).unwrap();
        refresh(cx, &fixture);
        open_diff(cx, &fixture);

        show_diff(cx, &fixture, "big.md");

        let message = label(cx, &fixture, "diff-error").expect("the big diff was not refused");
        assert!(
            message.contains("over the 5 MB editor limit"),
            "the refusal does not say why: {message:?}"
        );
        assert!(
            diff_text(cx, &fixture).is_empty(),
            "the refused diff was loaded anyway"
        );
    }

    #[gpui_kit::test]
    fn the_file_tree_marks_the_same_paths_the_diff_lists(cx: &mut TestAppContext) {
        let fixture = open(cx, "tree-status");
        change_three_files(&fixture);
        refresh(cx, &fixture);
        draw(cx, &fixture);

        // The same letters the diff tab shows, on the same paths: one list read
        // twice, not two calls to git.
        assert_eq!(
            label(cx, &fixture, &tree_row(&fixture, NOTES)).as_deref(),
            Some("notes.md M")
        );
        assert_eq!(
            label(cx, &fixture, &tree_row(&fixture, "README.md")).as_deref(),
            Some("README.md D")
        );
        // `added.md` is not asserted: the tree read this directory when the window opened, and a file
        // written after that is not in its cache until the directory is re-read (collapse and
        // expand, or another worktree).
        assert_eq!(
            label(cx, &fixture, &tree_row(&fixture, NUL)).as_deref(),
            Some(NUL)
        );
    }

    /// The `+` menu hangs at a window point; the sidebar opening by key moves
    /// the `+` away, so the menu must not stay behind over the sidebar.
    #[gpui_kit::test]
    fn showing_a_panel_closes_the_add_menu(cx: &mut TestAppContext) {
        let fixture = open(cx, "panel-closes-menu");
        click_visible(cx, &fixture, "add-tab");
        assert!(
            draw_until(cx, &fixture, |cx| found(cx, &fixture, "add-menu")),
            "the + did not open its menu"
        );
        activate(cx, &fixture);
        press(cx, &fixture, "cmd-shift-g");
        assert!(
            draw_until(cx, &fixture, |cx| !found(cx, &fixture, "add-menu")),
            "the menu stayed over the sidebar"
        );
    }

    /// `cmd-shift-g` is "Show Source Control", and that is what it does
    /// here too: the panel, which is where the changed files are and therefore
    /// where a diff comes from. It is a toggle, so the same key puts it away.
    #[gpui_kit::test]
    fn the_source_control_key_shows_the_panel(cx: &mut TestAppContext) {
        let fixture = open(cx, "panel-key");
        change_three_files(&fixture);
        refresh(cx, &fixture);

        // The keyboard path resolves the window it acts on through the
        // platform's active one, and a test window starts inactive — the app's
        // own window is the active one whenever a key can reach it at all.
        activate(cx, &fixture);
        press(cx, &fixture, "cmd-shift-g");
        assert!(
            draw_until(cx, &fixture, |cx| cx
                .update(|cx| fixture.workspace.read(cx).panel)
                == Some(Panel::Git)),
            "cmd-shift-g did not show the source control panel"
        );
        assert!(found(cx, &fixture, "git-panel"));

        press(cx, &fixture, "cmd-shift-g");
        assert!(
            draw_until(cx, &fixture, |cx| cx
                .update(|cx| fixture.workspace.read(cx).panel)
                .is_none()),
            "the same key did not put the panel away"
        );
    }

    #[gpui_kit::test]
    fn the_jump_opens_the_file_and_leaves_the_caret_to_the_editor(cx: &mut TestAppContext) {
        let fixture = open(cx, "diff-jump");
        change_three_files(&fixture);
        refresh(cx, &fixture);
        open_diff(cx, &fixture);

        show_diff(cx, &fixture, "notes.md");
        click(cx, &fixture, "git-open-notes.md".to_string());

        // The file half of the jump: the file's own tab, focused.
        assert!(
            draw_until(cx, &fixture, |cx| file_tabs(&active_tabs(cx, &fixture))
                .contains(&fixture.root.join(NOTES))),
            "the jump did not open the file in an editor tab"
        );
        assert!(found(cx, &fixture, "editor-pane"));

        // The line half is not here, and cannot be: gpui-kit 0.7.1's editor state has no public cursor
        // API, so a caret cannot be placed from outside the editor — the tab opens on the file,
        // at wherever the reader last was in it.
        let text = cx.update(|cx| {
            fixture
                .workspace
                .read(cx)
                .active_tabs(cx)
                .into_iter()
                .flat_map(|tab| tab.specs().into_iter().cloned().collect::<Vec<_>>())
                .any(|spec| matches!(spec, TabSpec::File { .. }))
        });
        assert!(text, "the editor tab is not in the layout the app persists");
    }

    /// A tab opened by the tree (or by the source control panel) has to bring the worktree's pane
    /// layout with it: a tab and the layout that shows it are one thing, and the two places
    /// that pushed a tab by hand left the pane area saying "No repository yet" about a worktree
    /// whose files were in the tree right beside it.
    #[gpui_kit::test]
    fn opening_a_file_after_the_last_tab_closed_shows_it(cx: &mut TestAppContext) {
        let fixture = open(cx, "pane-after-close");
        draw(cx, &fixture);
        assert!(found(cx, &fixture, "pane"), "the fixture has no pane area");

        // Close the worktree's only tab: no tabs, and with them no layout.
        press(cx, &fixture, "cmd-w");
        assert!(
            draw_until(cx, &fixture, |cx| active_tabs(cx, &fixture).is_empty()),
            "the last tab did not close"
        );
        assert!(!found(cx, &fixture, "pane"), "the pane area is still drawn");

        click(cx, &fixture, tree_row(&fixture, NOTES));
        assert!(
            draw_until(cx, &fixture, |cx| file_tabs(&active_tabs(cx, &fixture))
                .len()
                == 1),
            "the file did not open"
        );
        assert!(
            found(cx, &fixture, "pane"),
            "the file's tab is drawn but the pane area says the worktree is gone"
        );
    }

    /// A menu item clicked in the real window: the file tree lives in the right
    /// sidebar here, not at the window's own origin, so this is the arrangement
    /// the panel is actually used in.
    #[gpui_kit::test]
    fn a_row_menu_item_does_what_it_says_in_the_real_window(cx: &mut TestAppContext) {
        let fixture = open(cx, "tree-menu-real");
        show_panel(cx, &fixture, "panel-files", Some(Panel::Files));
        let doomed = fixture.root.join(NOTES);
        cx.update_window(fixture.window.into(), |_, window, cx| {
            window.right_click(format!("tree-{}", doomed.display()), cx);
        })
        .expect("right-click the row");
        assert!(
            draw_until(cx, &fixture, |cx| found(cx, &fixture, "tree-menu")),
            "the row menu did not open"
        );

        click(cx, &fixture, "tree-menu-delete".to_string());
        assert!(
            draw_until(cx, &fixture, |cx| found(cx, &fixture, "tree-confirm")),
            "clicking Delete did not ask about anything"
        );
        let question = label(cx, &fixture, "tree-confirm").unwrap_or_default();
        assert!(
            question.contains(NOTES),
            "the question is about another file: {question:?}"
        );
        assert!(doomed.is_file(), "the file went without a confirmation");
        // And what the item was drawn *over* did not act: the click is the
        // item's, and a row under a menu that also opens a file is the menu
        // leaking clicks into the tree.
        let opened = cx.update(|cx| fixture.workspace.read(cx).active_tabs(cx));
        assert!(
            !saved_specs(&opened)
                .iter()
                .any(|spec| matches!(spec, TabSpec::File { .. })),
            "the row under the menu acted too: {opened:?}"
        );
    }

    /// Rename, in the real window: the field the row grows is typed into through
    /// the panel's own key handler, which is only reached if the panel still has
    /// the keyboard after the menu item's click.
    #[gpui_kit::test]
    fn renaming_a_file_from_the_row_menu_takes_typed_input(cx: &mut TestAppContext) {
        let fixture = open(cx, "tree-rename-real");
        show_panel(cx, &fixture, "panel-files", Some(Panel::Files));
        let from = fixture.root.join(NOTES);
        cx.update_window(fixture.window.into(), |_, window, cx| {
            window.right_click(format!("tree-{}", from.display()), cx);
        })
        .expect("right-click the row");
        assert!(
            draw_until(cx, &fixture, |cx| found(cx, &fixture, "tree-menu")),
            "the row menu did not open"
        );
        click(cx, &fixture, "tree-menu-rename".to_string());
        assert!(
            draw_until(cx, &fixture, |cx| found(cx, &fixture, "tree-edit")),
            "the rename field did not open"
        );

        // The name is in the field, selected: what is typed replaces it.
        type_text(cx, &fixture, "renamed.md");
        draw(cx, &fixture);
        let field = label(cx, &fixture, "tree-edit").unwrap_or_default();
        assert!(
            field.contains("renamed.md"),
            "the typed name never landed: {field:?}"
        );

        press(cx, &fixture, "enter");
        assert!(
            draw_until(cx, &fixture, |_cx| fixture
                .root
                .join("renamed.md")
                .is_file()),
            "the rename did not happen"
        );
        assert!(!from.exists(), "the old name is still there");
    }

    /// A click on a row the menu covers: the menu's layer is what hears it, so the row does not
    /// act.
    #[gpui_kit::test]
    fn a_click_on_a_row_under_the_menu_does_not_open_it(cx: &mut TestAppContext) {
        let fixture = open(cx, "tree-menu-covered");
        show_panel(cx, &fixture, "panel-files", Some(Panel::Files));
        let menu_row = fixture.root.join(NOTES);
        cx.update_window(fixture.window.into(), |_, window, cx| {
            window.right_click(format!("tree-{}", menu_row.display()), cx);
        })
        .expect("right-click the row");
        assert!(
            draw_until(cx, &fixture, |cx| found(cx, &fixture, "tree-menu")),
            "the row menu did not open"
        );

        // A row the menu is drawn over, found rather than named: which rows the
        // fixture has depends on the fixture.
        let menu = bounds_of(cx, &fixture, "tree-menu");
        let covered = [NOTES, "README.md", ".gitignore", IGNORED, NUL]
            .into_iter()
            .filter_map(|name| {
                let path = fixture.root.join(name);
                cx.update_window(fixture.window.into(), |_, window, _| {
                    window.try_find(format!("tree-{}", path.display()))
                })
                .ok()
                .flatten()
                .map(|element| (path, element.bounds().center()))
            })
            .filter(|(_, center)| menu.contains(center) && center.y > menu.origin.y + px(40.))
            .next_back()
            .expect("no row is under the menu");
        assert!(
            !covered.0.ends_with(NOTES),
            "the covered row is the menu's own row"
        );

        let at = covered.1;
        cx.update_window(fixture.window.into(), |_, window, cx| {
            window.dispatch_event(
                gpui_kit::MouseDownEvent {
                    button: gpui_kit::MouseButton::Left,
                    position: at,
                    modifiers: Default::default(),
                    click_count: 1,
                    first_mouse: false,
                }
                .to_platform_input(),
                cx,
            );
            window.dispatch_event(
                gpui_kit::MouseUpEvent {
                    button: gpui_kit::MouseButton::Left,
                    position: at,
                    modifiers: Default::default(),
                    click_count: 1,
                }
                .to_platform_input(),
                cx,
            );
            window.render_frame(cx);
        })
        .expect("click the covered row");

        let opened = cx.update(|cx| fixture.workspace.read(cx).active_tabs(cx));
        assert!(
            !saved_specs(&opened)
                .iter()
                .any(|spec| matches!(spec, TabSpec::File { .. })),
            "the row under the menu acted on the click: {opened:?}"
        );
    }

    /// The row menu, the name field and the delete question all name a path in the worktree that
    /// was on screen.
    #[gpui_kit::test]
    fn switching_worktrees_drops_the_row_menu(cx: &mut TestAppContext) {
        let fixture = open(cx, "tree-menu-switch");
        show_panel(cx, &fixture, "panel-files", Some(Panel::Files));
        let row = format!("tree-{}", fixture.root.join(NOTES).display());
        cx.update_window(fixture.window.into(), |_, window, cx| {
            window.right_click(row.clone(), cx);
        })
        .expect("right-click the row");
        assert!(
            draw_until(cx, &fixture, |cx| found(cx, &fixture, "tree-menu")),
            "the row menu did not open"
        );

        let sibling = fixture.root.parent().unwrap().join("wt-one");
        let sibling = sibling.canonicalize().unwrap_or(sibling);
        let id = sibling.display().to_string();
        cx.update_window(fixture.window.into(), |_, window, cx| {
            window.dispatch_action(
                Box::new(crate::state::worktree::Activate { worktree_id: id }),
                cx,
            );
        })
        .expect("activate the sibling worktree");
        assert!(
            draw_until(cx, &fixture, |cx| !found(cx, &fixture, "tree-menu")),
            "the row menu is still open on the worktree that was left"
        );
    }

    /// An empty pane says what is missing. "No repository yet" is for a window
    /// with no project at all; a worktree with nothing open in it is a different
    /// thing, and it says so.
    #[gpui_kit::test]
    fn an_empty_pane_says_what_is_missing(cx: &mut TestAppContext) {
        let fixture = open(cx, "empty-pane");
        assert!(
            draw_until(cx, &fixture, |cx| cx.update(|cx| fixture
                .workspace
                .read(cx)
                .active_terminal(cx)
                .is_some())),
            "no terminal to close"
        );

        // The worktree on screen with nothing open in it: its last tab is closed,
        // and the pane area has nothing left to draw. The repository and the
        // worktree are both still there, so the message must not be about them.
        click_visible(cx, &fixture, "tab-close-0");
        assert!(
            draw_until(cx, &fixture, |cx| found(cx, &fixture, "workspace-message")),
            "closing the last tab left no message"
        );
        assert_eq!(
            label(cx, &fixture, "workspace-message").as_deref(),
            Some("Nothing open here. Open a terminal to start."),
            "a worktree with nothing open was told there is no repository"
        );
    }

    #[gpui_kit::test]
    fn the_open_tabs_are_written_to_state_json(cx: &mut TestAppContext) {
        let fixture = open(cx, "tree-persist");
        click(cx, &fixture, tree_row(&fixture, NOTES));
        assert!(draw_until(cx, &fixture, |cx| file_tabs(&active_tabs(
            cx, &fixture
        ))
        .len()
            == 1));

        let saved = Store::new(fixture.state_path.clone()).load().unwrap();
        assert_eq!(
            file_tabs(&saved_specs(&saved.tabs)),
            vec![fixture.root.join(NOTES)],
            "the layout on disk does not name the open file"
        );
    }

    /// Make the test window the active one. App-level paths resolve their window
    /// through the platform's active one, and a test window starts inactive (see
    /// the `cmd-shift-d` test for the same line and the same reason).
    fn activate(cx: &mut TestAppContext, fixture: &Fixture) {
        cx.update_window(fixture.window.into(), |_, window, _| {
            window.activate_window();
        })
        .expect("activate the window");
    }

    /// Click a strip icon until the sidebar shows what it should.
    fn show_panel(cx: &mut TestAppContext, fixture: &Fixture, id: &str, want: Option<Panel>) {
        let workspace = fixture.workspace.clone();
        let selected = |cx: &mut TestAppContext| cx.update(|cx| workspace.read(cx).panel);
        // A wall clock, not a count of attempts: a click can be dropped and a frame can arrive before
        // the strip it draws has been laid out, and both of those are more likely on a machine
        // that is busy with the rest of this suite.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            draw(cx, fixture);
            if selected(cx) == want {
                return;
            }
            click_visible(cx, fixture, id);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!(
            "clicking {id} never left the sidebar showing {want:?} (it shows {:?})",
            selected(cx)
        );
    }

    /// The right sidebar is the window's trailing panel: an activity strip with one icon per panel,
    /// and the selected panel under it.
    #[gpui_kit::test]
    fn the_right_sidebar_is_the_trailing_panel_and_the_strip_switches_it(cx: &mut TestAppContext) {
        let fixture = open(cx, "layout-sidebar");
        let workspace = fixture.workspace.clone();
        let panel = |cx: &mut TestAppContext| cx.update(|cx| workspace.read(cx).panel);

        // Trailing: to the right of the sidebar and of the pane area (whose
        // strip is the tab bar). This is the explorer side.
        let sidebar = bounds_of(cx, &fixture, "sidebar");
        let strip = bounds_of(cx, &fixture, "tab-bar");
        let right = bounds_of(cx, &fixture, "right-sidebar");
        assert!(
            sidebar.origin.x < strip.origin.x && strip.origin.x < right.origin.x,
            "the sidebar is not the trailing panel: {sidebar:?}, {strip:?}, {right:?}"
        );

        // Files is the panel a window opens with, and the strip says so.
        assert_eq!(panel(cx), Some(Panel::Files));
        assert!(found(cx, &fixture, "file-tree"));
        assert_eq!(
            label(cx, &fixture, "panel-files").as_deref(),
            Some("Show Files")
        );
        assert_eq!(
            label(cx, &fixture, "panel-git").as_deref(),
            Some("Show Source Control")
        );

        // Switching panels: the strip stays, the panel under it changes.
        show_panel(cx, &fixture, "panel-sessions", Some(Panel::Sessions));
        assert!(found(cx, &fixture, "session-panel"));
        assert!(
            !found(cx, &fixture, "file-tree"),
            "the old panel is still drawn"
        );
        assert!(found(cx, &fixture, "panel-strip"), "the strip went away");

        show_panel(cx, &fixture, "panel-git", Some(Panel::Git));
        assert!(found(cx, &fixture, "git-panel"));
        assert!(!found(cx, &fixture, "session-panel"));

        // The icon of the panel on screen closes the sidebar, and the toggle
        // that floats in the pane area's corner brings it back — the panel that
        // was last looked at, not the first one.
        show_panel(cx, &fixture, "panel-git", None);
        assert!(!found(cx, &fixture, "right-sidebar"));
        assert_eq!(
            label(cx, &fixture, "panel-toggle").as_deref(),
            Some("Toggle right sidebar")
        );
        // It sits beside the tab bar's `+`, not on top of it.
        let add = bounds_of(cx, &fixture, "add-tab");
        let toggle = bounds_of(cx, &fixture, "panel-toggle");
        assert!(
            add.right() <= toggle.left(),
            "the toggle covers the +: {add:?} {toggle:?}"
        );
        show_panel(cx, &fixture, "panel-toggle", Some(Panel::Git));
        assert!(found(cx, &fixture, "git-panel"));
    }

    /// The panel's other half: the remote. The header's menu fetches, pulls and
    /// pushes — and a branch with no upstream is offered a publish instead, which
    /// is the operation that makes the upstream in the first place.
    #[gpui_kit::test]
    fn the_source_control_header_publishes_pulls_and_pushes(cx: &mut TestAppContext) {
        // A remote to talk to: a bare repository beside the fixture's checkout,
        // with the branch already on it — but not set as the local branch's
        // upstream, which is the state the first half of this test is about.
        let fixture = open_with(cx, "git-sync", Settings::default(), |root, _| {
            let origin = root.parent().expect("a sibling").join("origin.git");
            let origin = origin.to_string_lossy().into_owned();
            git(root, &["init", "-q", "--bare", "-b", "main", &origin]);
            git(root, &["remote", "add", "origin", &origin]);
            git(root, &["push", "-q", "origin", "main"]);
            Vec::new()
        });
        let panel = cx.update(|cx| fixture.workspace.read(cx).git.clone());
        show_panel(cx, &fixture, "panel-git", Some(Panel::Git));
        assert!(
            draw_until(cx, &fixture, |cx| cx
                .update(|cx| panel.read(cx).status().is_some())),
            "the panel never read the branch"
        );
        assert_eq!(
            cx.update(|cx| panel
                .read(cx)
                .status()
                .and_then(|status| status.upstream.clone())),
            None,
            "the fixture's branch already has an upstream"
        );

        // The menu, opened from the header: fetch and publish, and no pull or
        // push — there is no branch to pull from or push to yet.
        assert!(
            click_until(cx, &fixture, "git-sync".to_string(), |cx| found(
                cx,
                &fixture,
                "git-sync-menu"
            )),
            "the header's menu did not open"
        );
        assert!(found(cx, &fixture, "git-sync-fetch"));
        assert!(found(cx, &fixture, "git-sync-publish"));
        assert!(!found(cx, &fixture, "git-sync-pull"));
        assert!(!found(cx, &fixture, "git-sync-push"));

        // Publish is `git push -u origin HEAD`: the branch lands on the remote
        // and the branch now has the upstream it did not.
        click_visible(cx, &fixture, "git-sync-publish");
        assert!(
            draw_until(cx, &fixture, |cx| cx.update(|cx| panel
                .read(cx)
                .status()
                .is_some_and(
                    |status| status.upstream.as_deref() == Some("origin/main")
                ))),
            "publishing did not set the upstream: {:?}",
            cx.update(|cx| panel.read(cx).status().cloned())
        );
        // Running an entry closes the menu, and git's own words are the notice.
        assert!(!found(cx, &fixture, "git-sync-menu"));
        assert!(
            label(cx, &fixture, "git-notice").is_some_and(|said| !said.is_empty()),
            "the publish said nothing"
        );

        // And now the menu offers the other two. A commit goes up with Push, and
        // the remote's ref is the commit that was made here.
        git(&fixture.root, &["add", "-A"]);
        git(
            &fixture.root,
            &["commit", "-q", "--allow-empty", "-m", "from the panel"],
        );
        assert!(
            click_until(cx, &fixture, "git-sync".to_string(), |cx| found(
                cx,
                &fixture,
                "git-sync-push"
            )),
            "the menu does not offer a push now that there is an upstream"
        );
        assert!(!found(cx, &fixture, "git-sync-publish"));
        click_visible(cx, &fixture, "git-sync-push");
        assert!(
            draw_until(cx, &fixture, |cx| {
                let said = label(cx, &fixture, "git-notice").unwrap_or_default();
                said.contains("main")
            }),
            "the push said nothing about the branch: {:?}",
            label(cx, &fixture, "git-notice")
        );
        let remote = String::from_utf8_lossy(
            &std::process::Command::new("git")
                .arg("-C")
                .arg(&fixture.root)
                .args(["rev-parse", "origin/main"])
                .output()
                .unwrap()
                .stdout,
        )
        .trim()
        .to_string();
        let here = String::from_utf8_lossy(
            &std::process::Command::new("git")
                .arg("-C")
                .arg(&fixture.root)
                .args(["rev-parse", "HEAD"])
                .output()
                .unwrap()
                .stdout,
        )
        .trim()
        .to_string();
        assert_eq!(remote, here, "the remote did not receive the commit");

        // A commit from somewhere else, and the panel's Pull is what brings it
        // home: the file is here, and the branch is with the remote again.
        let origin = fixture.root.parent().expect("a sibling").join("origin.git");
        let origin = origin.to_string_lossy().into_owned();
        let other = fixture.root.parent().expect("a sibling").join("other");
        let other_arg = other.to_string_lossy().into_owned();
        git(&fixture.root, &["clone", "-q", &origin, &other_arg]);
        git(&other, &["config", "user.email", "test@syro.invalid"]);
        git(&other, &["config", "user.name", "Syro Test"]);
        std::fs::write(other.join("from-elsewhere.txt"), "x\n").unwrap();
        git(&other, &["add", "."]);
        git(&other, &["commit", "-q", "-m", "from elsewhere"]);
        git(&other, &["push", "-q", "origin", "main"]);

        click_visible(cx, &fixture, "git-sync");
        assert!(
            draw_until(cx, &fixture, |cx| found(cx, &fixture, "git-sync-pull")),
            "the menu does not offer a pull"
        );
        click_visible(cx, &fixture, "git-sync-pull");
        assert!(
            draw_until(cx, &fixture, |_| fixture
                .root
                .join("from-elsewhere.txt")
                .is_file()),
            "the pull brought nothing"
        );
        assert_eq!(
            cx.update(|cx| panel.read(cx).status().map(|status| status.ahead)),
            Some(0),
            "the branch is still ahead of the upstream it just pulled"
        );

        // Escape closes the menu, the way it closes every other overlay.
        click_visible(cx, &fixture, "git-sync");
        assert!(
            draw_until(cx, &fixture, |cx| found(cx, &fixture, "git-sync-menu")),
            "the menu did not open a second time"
        );
        press(cx, &fixture, "escape");
        assert!(
            draw_until(cx, &fixture, |cx| !found(cx, &fixture, "git-sync-menu")),
            "escape did not close the menu"
        );
    }

    /// The source control panel's branch line: a menu of every local branch, and
    /// picking another is a checkout — git's own words are the notice, and the
    /// panel reads the branch it moved to.
    #[gpui_kit::test]
    fn the_source_control_panel_switches_branches(cx: &mut TestAppContext) {
        // The second branch is made where the fixture is: one to switch to.
        let fixture = open_with(cx, "git-switch", Settings::default(), |root, _| {
            git(root, &["branch", "feature"]);
            Vec::new()
        });
        let panel = cx.update(|cx| fixture.workspace.read(cx).git.clone());
        show_panel(cx, &fixture, "panel-git", Some(Panel::Git));
        assert!(
            draw_until(cx, &fixture, |cx| cx
                .update(|cx| panel.read(cx).status().is_some())),
            "the panel never read the branch"
        );
        assert_eq!(
            cx.update(|cx| panel
                .read(cx)
                .status()
                .map(|status| status.name().to_string())),
            Some("main".to_string())
        );

        // The branch line is the way in, and the menu names both branches.
        assert!(
            click_until(cx, &fixture, "git-branch-name".to_string(), |cx| found(
                cx,
                &fixture,
                "git-branch-menu"
            )),
            "the branch menu did not open"
        );
        assert!(found(cx, &fixture, "git-switch-main"));
        assert!(found(cx, &fixture, "git-switch-feature"));

        // Escape closes it, the way it closes every other overlay.
        press(cx, &fixture, "escape");
        assert!(
            draw_until(cx, &fixture, |cx| !found(cx, &fixture, "git-branch-menu")),
            "escape did not close the branch menu"
        );

        // Picking the other one checks it out, and the panel reads the branch it
        // moved to.
        click_visible(cx, &fixture, "git-branch-name");
        assert!(
            draw_until(cx, &fixture, |cx| found(cx, &fixture, "git-switch-feature")),
            "the branch menu did not open a second time"
        );
        click_visible(cx, &fixture, "git-switch-feature");
        assert!(
            draw_until(cx, &fixture, |cx| cx.update(|cx| panel
                .read(cx)
                .status()
                .is_some_and(|status| status.name() == "feature"))),
            "the branch did not switch: {:?}",
            cx.update(|cx| panel.read(cx).status().cloned())
        );
        assert!(!found(cx, &fixture, "git-branch-menu"));
        assert!(
            label(cx, &fixture, "git-notice").is_some_and(|said| said.contains("feature")),
            "the switch said nothing: {:?}",
            label(cx, &fixture, "git-notice")
        );
    }

    /// A repository with nothing to switch to is a branch name, not a switcher: git
    /// reports `main` from HEAD before there is a ref for it, which is the state a
    /// plain folder added as a project is in, and a menu there is an empty box.
    #[gpui_kit::test]
    fn the_git_panel_offers_no_switch_with_no_branch_to_switch_to(cx: &mut TestAppContext) {
        let fixture = open_with(cx, "git-one-branch", Settings::default(), |root, _| {
            // Every local ref, and not only `main`: the fixture's own two worktree
            // branches are refs in the same repository, and either would be
            // something to switch to.
            for branch in ["main", "wt-one", "wt-two"] {
                let ref_name = format!("refs/heads/{branch}");
                git(root, &["update-ref", "-d", &ref_name]);
            }
            Vec::new()
        });
        let panel = cx.update(|cx| fixture.workspace.read(cx).git.clone());
        show_panel(cx, &fixture, "panel-git", Some(Panel::Git));
        assert!(
            draw_until(cx, &fixture, |cx| cx
                .update(|cx| panel.read(cx).status().is_some())),
            "the panel never read the branch"
        );

        // The branch is on screen, and the line is a label: it says nothing about
        // switching, and there is no menu behind it — no box with nothing in it.
        assert!(
            label(cx, &fixture, "git-branch-name").is_some_and(|name| name.contains("main")),
            "the branch name is not drawn: {:?}",
            label(cx, &fixture, "git-branch-name")
        );
        click_visible(cx, &fixture, "git-branch-name");
        draw(cx, &fixture);
        assert!(
            !found(cx, &fixture, "git-branch-menu"),
            "the branch menu opened with no branch in it"
        );
    }

    /// The source control panel: the branch on screen, what it has against its
    /// upstream, and the commits that got it there.
    #[gpui_kit::test]
    fn the_git_panel_shows_the_branch_and_the_commits(cx: &mut TestAppContext) {
        let fixture = open(cx, "git-panel");
        let panel = cx.update(|cx| fixture.workspace.read(cx).git.clone());
        show_panel(cx, &fixture, "panel-git", Some(Panel::Git));

        assert!(
            draw_until(cx, &fixture, |cx| cx
                .update(|cx| !panel.read(cx).commits().is_empty())),
            "the panel never read the repository"
        );
        let commits = cx.update(|cx| panel.read(cx).commits().to_vec());
        assert_eq!(commits[0].summary, "fixture files", "newest first");
        assert!(commits.iter().any(|commit| commit.summary == "initial"));
        assert_eq!(
            cx.update(|cx| panel
                .read(cx)
                .status()
                .map(|status| status.name().to_string())),
            Some("main".to_string())
        );
        // The fixture's repository has no remote, which is the normal case for a
        // worktree this app just made: the panel says so rather than inventing an
        // upstream.
        assert_eq!(
            cx.update(|cx| panel.read(cx).headline()),
            "No changes on this branch"
        );
        // Every commit is a row, labelled with its summary.
        assert_eq!(
            label(cx, &fixture, &format!("commit-{}", commits[0].short)).as_deref(),
            Some("fixture files")
        );
        // And the panel's blocks are drawn with a size: a panel that is there but zero-tall is the
        // "看不见的 0" bug class, and `found` alone cannot tell the two apart.
        for id in [
            "git-header",
            "git-branch",
            "git-summary",
            "git-history",
            &format!("commit-{}", commits[0].short),
        ] {
            let bounds = bounds_of(cx, &fixture, id);
            assert!(
                bounds.size.width > px(0.) && bounds.size.height > px(0.),
                "{id} is drawn at {bounds:?}"
            );
        }

        // A clean tree has no file section to draw.
        assert!(!found(cx, &fixture, "git-changes"));

        // A file written after the read is what the refresh is for: the panel
        // lists it, counts it, and offers the commit that would hold it.
        std::fs::write(fixture.root.join("later.txt"), "later\n").unwrap();
        let landed = panel.clone();
        assert!(
            click_until(cx, &fixture, "git-refresh".to_string(), move |cx| cx
                .update(|cx| landed.read(cx).changes().len() == 1)),
            "the refresh did not see the new file"
        );
        assert_eq!(cx.update(|cx| panel.read(cx).headline()), "1 changed file");
        assert_eq!(
            label(cx, &fixture, "git-file-later.txt").as_deref(),
            Some("? later.txt"),
            "the changed file is not the row the panel drew"
        );
        // The message field and the button are the panel's reason to exist.
        assert!(found(cx, &fixture, "git-message"));
        assert_eq!(
            label(cx, &fixture, "git-primary").as_deref(),
            Some("Commit")
        );

        // Type the message, then press the button: a real commit in the fixture's own repository, on
        // the background executor.
        let typed = panel.clone();
        let mut typed_ok = false;
        for _ in 0..3 {
            click_visible(cx, &fixture, "git-message");
            type_text(cx, &fixture, "add later");
            draw(cx, &fixture);
            let message = cx.update(|cx| typed.read(cx).message().to_string());
            if message == "add later" {
                typed_ok = true;
                break;
            }
        }
        assert!(typed_ok, "the message field never took the text");

        let committed = panel.clone();
        assert!(
            click_until(cx, &fixture, "git-primary".to_string(), move |cx| cx
                .update(|cx| committed
                    .read(cx)
                    .commits()
                    .first()
                    .is_some_and(|commit| commit.summary == "add later"))),
            "the commit never landed"
        );
        // The tree is clean again, so the section and the message box are gone
        // and the history has one more row.
        assert!(cx.update(|cx| panel.read(cx).changes().is_empty()));
        assert_eq!(
            cx.update(|cx| panel.read(cx).headline()),
            "No changes on this branch"
        );
        assert!(!found(cx, &fixture, "git-changes"));

        // And the way to the changed files is the diff tab this app already has.
        std::fs::write(fixture.root.join("more.txt"), "more\n").unwrap();
        click_visible(cx, &fixture, "git-refresh");
        assert!(
            draw_until(cx, &fixture, |cx| found(cx, &fixture, "git-view-all")),
            "the changes section did not come back"
        );
        let workspace = fixture.workspace.clone();
        assert!(
            click_until(cx, &fixture, "git-view-all".to_string(), move |cx| cx
                .update(|cx| workspace.read(cx).active_diff(cx).is_some())),
            "the panel's View all did not open the diff tab"
        );
    }

    /// The AI button: the command's stdout is what the message field holds, which
    /// is what the Commit button reads. `cat` stands in for the model — it prints
    /// the prompt back, and the prompt carries the diff.
    #[gpui_kit::test]
    fn the_ai_button_puts_the_commands_answer_in_the_message_field(cx: &mut TestAppContext) {
        let settings = Settings {
            commit_message_command: Some("cat".into()),
            ..Default::default()
        };
        let fixture = open_with_settings(cx, "git-ai-message", settings);
        let panel = cx.update(|cx| fixture.workspace.read(cx).git.clone());
        show_panel(cx, &fixture, "panel-git", Some(Panel::Git));
        assert!(
            draw_until(cx, &fixture, |cx| cx
                .update(|cx| !panel.read(cx).commits().is_empty())),
            "the panel never read the repository"
        );

        // Something to write a message for.
        std::fs::write(fixture.root.join("described.txt"), "described\n").unwrap();
        click_visible(cx, &fixture, "git-refresh");
        assert!(
            draw_until(cx, &fixture, |cx| found(
                cx,
                &fixture,
                "git-generate-message"
            )),
            "the AI button was never drawn"
        );
        // The field is a textarea, not a single line: a message with a body has
        // somewhere to go. And it takes the whole width — the AI button sits
        // inside it rather than beside it.
        let field = bounds_of(cx, &fixture, "git-message");
        assert!(
            field.size.height > px(40.),
            "the message field is one line tall: {field:?}"
        );
        let button = bounds_of(cx, &fixture, "git-generate-message");
        assert!(
            field.contains(&button.center()),
            "the AI button is outside the field: {button:?} in {field:?}"
        );

        let landed = panel.clone();
        assert!(
            click_until(
                cx,
                &fixture,
                "git-generate-message".to_string(),
                move |cx| cx.update(|cx| landed.read(cx).message().contains("commit message"))
            ),
            "the command's answer never reached the message field"
        );
    }

    /// A `git commit` run from anywhere but this panel — a terminal tab, an agent, a script — is
    /// invisible to every event the app emits, so the panel re-reads on a tick while it is the
    /// one on screen.
    #[gpui_kit::test]
    fn the_git_panel_notices_a_commit_made_outside_it(cx: &mut TestAppContext) {
        let fixture = open(cx, "git-panel-outside");
        let panel = cx.update(|cx| fixture.workspace.read(cx).git.clone());
        show_panel(cx, &fixture, "panel-git", Some(Panel::Git));
        assert!(
            draw_until(cx, &fixture, |cx| cx
                .update(|cx| !panel.read(cx).commits().is_empty())),
            "the panel never read the repository"
        );

        // Committed by git itself: no click, no event, nothing the app is told.
        std::fs::write(fixture.root.join("outside.txt"), "outside\n").unwrap();
        git(&fixture.root, &["add", "-A"]);
        git(
            &fixture.root,
            &["commit", "-q", "-m", "committed outside the panel"],
        );

        assert!(
            draw_until(cx, &fixture, |cx| cx.update(|cx| panel
                .read(cx)
                .commits()
                .iter()
                .any(|commit| commit.summary == "committed outside the panel"))),
            "the panel never noticed the commit made outside it"
        );
        // And the list above is not still calling that work uncommitted.
        assert!(
            cx.update(|cx| panel.read(cx).changes().is_empty()),
            "the panel still lists the files of a commit that already happened"
        );
    }

    /// git's own words are about the worktree they were said in. A commit in one
    /// worktree leaves a notice under the button, and switching to another must
    /// not carry it over: the reader who has moved on is looking at a different
    /// repository, and that repository's push is not this one's news.
    #[gpui_kit::test]
    fn a_git_notice_does_not_follow_the_reader_to_another_worktree(cx: &mut TestAppContext) {
        let fixture = open(cx, "git-notice-worktree");
        let panel = cx.update(|cx| fixture.workspace.read(cx).git.clone());
        show_panel(cx, &fixture, "panel-git", Some(Panel::Git));
        assert!(
            draw_until(cx, &fixture, |cx| cx
                .update(|cx| !panel.read(cx).commits().is_empty())),
            "the panel never read the repository"
        );

        // Something to say, said the way the reader says it: a message, the
        // button, and a wait for the commit to land. Its output is the notice.
        std::fs::write(fixture.root.join("here.txt"), "here\n").unwrap();
        click_visible(cx, &fixture, "git-refresh");
        let mut typed = false;
        for _ in 0..3 {
            click_visible(cx, &fixture, "git-message");
            type_text(cx, &fixture, "here");
            draw(cx, &fixture);
            if cx.update(|cx| panel.read(cx).message() == "here") {
                typed = true;
                break;
            }
        }
        assert!(typed, "the message field never took the text");
        let committed = panel.clone();
        assert!(
            click_until(cx, &fixture, "git-primary".to_string(), move |cx| cx
                .update(|cx| committed
                    .read(cx)
                    .commits()
                    .first()
                    .is_some_and(|commit| commit.summary == "here"))),
            "the commit never landed"
        );
        assert!(
            draw_until(cx, &fixture, |cx| label(cx, &fixture, "git-notice")
                .is_some_and(|said| !said.is_empty())),
            "the commit said nothing"
        );

        // Another worktree, so the panel is now reading another repository.
        let other = cx.update(|cx| {
            fixture
                .state
                .read(cx)
                .worktrees()
                .iter()
                .find(|worktree| worktree.path != fixture.root)
                .expect("the fixture has a worktree besides the main checkout")
                .id
                .clone()
        });
        cx.update(|cx| {
            fixture
                .state
                .update(cx, |state, cx| state.activate(&other, cx))
        });

        assert!(
            draw_until(cx, &fixture, |cx| label(cx, &fixture, "git-notice")
                .is_none()),
            "the notice of the worktree the reader left is still on screen: {:?}",
            label(cx, &fixture, "git-notice")
        );
    }

    /// A history row opens onto the files *that* commit changed — the question a
    /// history is read to answer, and one the changes section above cannot: it
    /// lists the whole branch's work, and the two commits here share none of it.
    #[gpui_kit::test]
    fn a_commit_row_opens_onto_the_files_it_changed(cx: &mut TestAppContext) {
        let fixture = open(cx, "git-commit-files");
        let panel = cx.update(|cx| fixture.workspace.read(cx).git.clone());
        show_panel(cx, &fixture, "panel-git", Some(Panel::Git));
        assert!(
            draw_until(cx, &fixture, |cx| cx
                .update(|cx| !panel.read(cx).commits().is_empty())),
            "the panel never read the repository"
        );
        let commits = cx.update(|cx| panel.read(cx).commits().to_vec());
        let newest = commits[0].clone();
        let oldest = commits[1].clone();
        assert_eq!(oldest.summary, "initial");
        let (row, author) = (
            format!("commit-{}", newest.short),
            format!("commit-author-{}", newest.short),
        );
        let notes = format!("commit-file-{NOTES}");

        // A closed commit is drawn as its one row, and holds nothing open under it.
        assert!(found(cx, &fixture, &row), "the newest commit has no row");
        assert!(!found(cx, &fixture, &notes));

        assert!(
            click_until(cx, &fixture, row.clone(), |cx| found(cx, &fixture, &notes)),
            "clicking a commit row did not list the files it changed"
        );
        // The whole block, not just the files: who made the commit and when.
        assert!(found(cx, &fixture, &author), "the author line is missing");
        // And the file is the one git reports for *this* commit: `fixture files`
        // added `notes.md` and never touched `README.md`, which is the commit
        // before it — and which the changes section is not listing either.
        let added = format!("A {NOTES}");
        assert_eq!(
            label(cx, &fixture, &notes).as_deref(),
            Some(added.as_str()),
            "the row under the commit is not the file it added"
        );
        let notes_bounds = bounds_of(cx, &fixture, &notes);
        assert!(
            notes_bounds.size.height > px(0.),
            "the file row under the commit is drawn at no height"
        );
        // And the history's line runs through the whole block: a spine whose
        // height is a percentage of a box its own children size is one that can
        // come out 0 px tall, and the history would break in two under the row.
        let row_bounds = bounds_of(cx, &fixture, &row);
        let block = bounds_of(cx, &fixture, &format!("commit-open-{}", newest.short));
        let spine = bounds_of(cx, &fixture, &format!("commit-spine-{}", newest.short));
        assert!(
            spine.size.height > px(0.) && spine.origin.y >= row_bounds.origin.y,
            "the history's line is not drawn past the open commit: {spine:?} in {row_bounds:?}"
        );
        assert!(
            spine.origin.y + spine.size.height <= block.origin.y + block.size.height,
            "the spine runs out of the block it belongs to: {spine:?} in {block:?}"
        );
        assert!(!found(cx, &fixture, &format!("commit-file-README.md")));

        // One commit at a time: opening the one below closes this one, because
        // the history scrolls inside its own 240 px and two open commits would
        // push the rows between them out of it.
        let older = format!("commit-{}", oldest.short);
        assert!(
            click_until(cx, &fixture, older.clone(), |cx| found(
                cx,
                &fixture,
                "commit-file-README.md"
            )),
            "opening a second commit did not list its files"
        );
        assert!(
            !found(cx, &fixture, &notes),
            "the first commit stayed open under the second"
        );
        // The oldest commit's row is the end of the history, and its files are
        // below it, so its spine has to run on past its dot.
        let below = bounds_of(cx, &fixture, "commit-file-README.md");
        let oldest_row = bounds_of(cx, &fixture, &older);
        assert!(
            below.origin.y > oldest_row.origin.y,
            "the open commit's files are not under the row they belong to: {below:?} in {oldest_row:?}"
        );

        // And clicking the same row again closes it.
        assert!(
            click_until(cx, &fixture, older, |cx| !found(
                cx,
                &fixture,
                "commit-file-README.md"
            )),
            "clicking an open commit did not close it"
        );
    }

    /// The way from the panel's own header to the graph, and the whole of what
    /// the graph is for: every branch, and the way to throw one away. `main` is
    /// the one `HEAD` is on, and is not offered.
    #[gpui_kit::test]
    fn the_graph_lists_the_branches_and_deletes_one(cx: &mut TestAppContext) {
        let fixture = open(cx, "git-graph");
        // A second local branch, on the same commit as `main`: the graph shows
        // both names on one row, and only one of them can go.
        git(&fixture.root, &["branch", "spike"]);
        show_panel(cx, &fixture, "panel-git", Some(Panel::Git));

        assert!(
            click_until(cx, &fixture, "git-graph-open".to_string(), |cx| found(
                cx,
                &fixture,
                "git-graph-rows"
            )),
            "the panel's fullscreen button did not open the graph"
        );
        // The commits are rows, with the column names over them, and both are
        // drawn at a height: a view that is there but zero-tall is the bug class
        // a `found` alone cannot tell apart.
        let head = bounds_of(cx, &fixture, "git-graph-head");
        assert!(
            head.size.height > px(0.),
            "the graph has no column names: {head:?}"
        );
        let rows = bounds_of(cx, &fixture, "git-graph-rows");
        assert!(
            rows.size.height > px(0.),
            "the graph has no room for its commits: {rows:?}"
        );
        let newest = String::from_utf8_lossy(
            &std::process::Command::new("git")
                .arg("-C")
                .arg(&fixture.root)
                .args(["rev-parse", "--short", "HEAD"])
                .output()
                .unwrap()
                .stdout,
        )
        .trim()
        .to_string();
        assert!(
            found(cx, &fixture, &format!("git-graph-commit-{newest}")),
            "the newest commit has no row in the graph"
        );
        assert!(found(cx, &fixture, "git-graph-branch-main"));
        assert!(found(cx, &fixture, "git-graph-branch-spike"));
        // The branch `HEAD` is on is marked, not deletable; the other one is.
        assert!(found(cx, &fixture, "git-graph-delete-spike"));
        assert!(
            !found(cx, &fixture, "git-graph-delete-main"),
            "the graph offers to delete the branch it is on"
        );

        // A right-click on the branch's name is the other way in, and the one
        // that holds the operations *about the branch*: its menu, with the
        // delete in it.
        right_click(cx, &fixture, "git-graph-branch-spike".to_string());
        assert!(
            draw_until(cx, &fixture, |cx| found(
                cx,
                &fixture,
                "git-graph-branch-menu-delete"
            )),
            "right-clicking a branch did not open its menu"
        );
        // Escape closes the menu and not the graph: the menu is the innermost
        // thing that is open.
        press(cx, &fixture, "escape");
        assert!(
            draw_until(cx, &fixture, |cx| !found(
                cx,
                &fixture,
                "git-graph-branch-menu"
            )),
            "escape did not close the branch menu"
        );
        assert!(
            found(cx, &fixture, "git-graph-rows"),
            "escape closed the whole graph instead of the menu"
        );

        // The branch `HEAD` is on has nothing to do here, so it gets no menu at
        // all rather than an empty one.
        right_click(cx, &fixture, "git-graph-branch-main".to_string());
        draw(cx, &fixture);
        assert!(
            !found(cx, &fixture, "git-graph-branch-menu"),
            "the branch HEAD is on offered a menu"
        );

        // The delete asks first: nothing goes until the answer is given.
        right_click(cx, &fixture, "git-graph-branch-spike".to_string());
        assert!(
            draw_until(cx, &fixture, |cx| found(
                cx,
                &fixture,
                "git-graph-branch-menu-delete"
            )),
            "the branch menu did not open a second time"
        );
        click_visible(cx, &fixture, "git-graph-branch-menu-delete");
        assert!(
            draw_until(cx, &fixture, |cx| found(cx, &fixture, "ok")),
            "the delete did not ask"
        );
        assert!(
            found(cx, &fixture, "git-graph-branch-spike"),
            "the branch went before the question was answered"
        );

        click_visible(cx, &fixture, "ok");
        assert!(
            draw_until(cx, &fixture, |cx| !found(
                cx,
                &fixture,
                "git-graph-branch-spike"
            )),
            "the deleted branch is still in the graph"
        );
        // And it is gone from the repository, not only from the graph: git is
        // the truth this view draws.
        let said = std::process::Command::new("git")
            .arg("-C")
            .arg(&fixture.root)
            .args(["branch", "--format=%(refname:short)"])
            .output()
            .unwrap();
        let names = String::from_utf8_lossy(&said.stdout);
        assert!(
            !names.contains("spike"),
            "git still has the branch: {names}"
        );
        assert!(
            names.contains("main"),
            "git lost the branch it is on: {names}"
        );

        // The `×` gives the window back — and the overlay goes with the view,
        // because the panes under it are what was there before it.
        assert!(
            click_until(cx, &fixture, "git-graph-close".to_string(), |cx| !found(
                cx,
                &fixture,
                "git-graph-rows"
            )),
            "the graph's own close button did not close it"
        );
        assert!(!found(cx, &fixture, "git-graph-overlay"));
        assert!(!found(cx, &fixture, "git-graph-tab"));
        assert!(found(cx, &fixture, "git-panel"), "the panel went with it");

        // Escape hides an open graph without destroying its tab.
        assert!(
            click_until(cx, &fixture, "git-graph-open".to_string(), |cx| found(
                cx,
                &fixture,
                "git-graph-rows"
            )),
            "the graph did not open again from the Git panel"
        );
        press(cx, &fixture, "escape");
        assert!(
            draw_until(cx, &fixture, |cx| !found(cx, &fixture, "git-graph-rows")),
            "escape did not hide the graph"
        );
        assert!(found(cx, &fixture, "git-graph-tab"));
    }

    #[gpui_kit::test]
    fn the_graph_respects_window_transparency(cx: &mut TestAppContext) {
        let fixture = open(cx, "git-graph-opacity");
        show_panel(cx, &fixture, "panel-git", Some(Panel::Git));
        assert!(click_until(
            cx,
            &fixture,
            "git-graph-open".to_string(),
            |cx| found(cx, &fixture, "git-graph-rows")
        ));

        for opacity in [0.4, 1., 0.2] {
            cx.update(|cx| crate::settings_view::apply_opacity(cx, opacity, 0));
            draw(cx, &fixture);
            let graph = bounds_of(cx, &fixture, "git-graph-overlay");
            let color = cx.update(|cx| gpui_kit::base::Theme::global(cx).tokens.colors.background);
            let expected = gpui_kit::solid_background(color);
            let opaque = gpui_kit::solid_background(gpui_kit::Hsla { a: 1., ..color });
            let fills = cx
                .update_window(fixture.window.into(), |_, window, _| {
                    let center = graph.center().scale(window.scale_factor());
                    window
                        .painted_quads()
                        .iter()
                        .filter(|quad| {
                            quad.bounds.contains(&center)
                                && (quad.background == expected || quad.background == opaque)
                        })
                        .map(|quad| quad.background)
                        .collect::<Vec<_>>()
                })
                .unwrap();
            assert_eq!(
                fills,
                vec![expected; 2],
                "the graph does not follow opacity {opacity}"
            );
            assert!(
                !found(cx, &fixture, "pane"),
                "the pane shows through the graph"
            );
            assert!(
                !found(cx, &fixture, "tab-bar"),
                "the tab bar shows through the graph"
            );
        }

        assert!(click_until(
            cx,
            &fixture,
            "git-graph-close".to_string(),
            |cx| !found(cx, &fixture, "git-graph-overlay")
        ));
        assert!(found(cx, &fixture, "pane"));
        assert!(found(cx, &fixture, "tab-bar"));
    }

    /// The branch a worktree has checked out is the one git will not delete — and
    /// in this window that is most of them, because a worktree is a branch. The
    /// question asked is the worktree's, because that is the way the branch goes,
    /// and answering it takes both.
    #[gpui_kit::test]
    fn the_graph_deletes_a_branch_by_removing_the_worktree_that_holds_it(cx: &mut TestAppContext) {
        let fixture = open(cx, "git-graph-held-branch");
        let held = fixture.root.parent().expect("a sibling").join("wt-one");
        assert!(held.is_dir(), "the fixture's worktree is not where it was");
        show_panel(cx, &fixture, "panel-git", Some(Panel::Git));
        assert!(
            click_until(cx, &fixture, "git-graph-open".to_string(), |cx| found(
                cx,
                &fixture,
                "git-graph-rows"
            )),
            "the graph did not open"
        );

        // `git branch -d` cannot take this one: the branch is checked out over
        // there, and so is `-D`. So the question is the one that does have an
        // answer.
        right_click(cx, &fixture, "git-graph-branch-wt-one".to_string());
        assert!(
            draw_until(cx, &fixture, |cx| found(
                cx,
                &fixture,
                "git-graph-branch-menu-delete"
            )),
            "right-clicking the branch did not open its menu"
        );
        assert!(
            click_until(
                cx,
                &fixture,
                "git-graph-branch-menu-delete".to_string(),
                |cx| { found(cx, &fixture, "remove-worktree") }
            ),
            "the worktree that holds the branch was not asked about"
        );
        assert!(
            found(cx, &fixture, "git-graph-branch-wt-one"),
            "the branch went before the question was answered"
        );

        // Confirm, and both go: the directory, and the name it was on.
        assert!(
            click_until(cx, &fixture, "remove-worktree-remove".to_string(), |cx| {
                !found(cx, &fixture, "git-graph-branch-wt-one")
            }),
            "the deleted branch is still in the graph: {:?}",
            label(cx, &fixture, "git-graph-notice")
        );
        assert!(!held.exists(), "the worktree is still on disk");
        let said = std::process::Command::new("git")
            .arg("-C")
            .arg(&fixture.root)
            .args(["branch", "--format=%(refname:short)"])
            .output()
            .unwrap();
        let names = String::from_utf8_lossy(&said.stdout);
        assert!(
            !names.contains("wt-one"),
            "git still has the branch: {names}"
        );
    }

    /// The dot beside a commit sits beside the *middle* of it — and the middle
    /// is the text's, because a row is as tall as the text in it. A dot placed
    /// by a fixed offset from the top drifts above the line it belongs to, which
    /// is what the history's dots did: 6 px into a row that is 28.
    #[gpui_kit::test]
    fn a_commit_dot_is_beside_the_middle_of_its_row(cx: &mut TestAppContext) {
        let fixture = open(cx, "git-dots-centered");
        let panel = cx.update(|cx| fixture.workspace.read(cx).git.clone());
        show_panel(cx, &fixture, "panel-git", Some(Panel::Git));
        assert!(
            draw_until(cx, &fixture, |cx| cx
                .update(|cx| !panel.read(cx).commits().is_empty())),
            "the panel never read the repository"
        );
        let newest = cx.update(|cx| panel.read(cx).commits()[0].clone());

        // One place the two centres are read the same way, because the two
        // cases below are the same question.
        let centred = |cx: &mut TestAppContext, dot: &str, row: &str| {
            let dot = bounds_of(cx, &fixture, dot);
            let row = bounds_of(cx, &fixture, row);
            let off = ((dot.origin.y + dot.size.height / 2.)
                - (row.origin.y + row.size.height / 2.))
                .as_f32();
            assert!(
                off.abs() <= 1.,
                "the dot is {off} px off the middle of its row: {dot:?} in {row:?}"
            );
        };
        centred(
            cx,
            &format!("commit-dot-{}", newest.short),
            &format!("commit-{}", newest.short),
        );

        // The row for work that is not committed yet is the same shape, and has
        // to line up the same way.
        std::fs::write(fixture.root.join("uncommitted.txt"), "x\n").unwrap();
        assert!(
            click_until(cx, &fixture, "git-refresh".to_string(), |cx| found(
                cx,
                &fixture,
                "git-outgoing"
            )),
            "the outgoing row never appeared"
        );
        centred(cx, "git-outgoing-dot", "git-outgoing");
    }

    /// A branch on the remote is a name other machines can be working from, so
    /// its delete is a push — asked in those words, and run as one.
    #[gpui_kit::test]
    fn the_graph_deletes_a_remote_branch(cx: &mut TestAppContext) {
        let fixture = open_with(
            cx,
            "git-graph-remote-branch",
            Settings::default(),
            |root, _| {
                let origin = root.parent().expect("a sibling").join("origin.git");
                let origin = origin.to_string_lossy().into_owned();
                git(root, &["init", "-q", "--bare", "-b", "main", &origin]);
                git(root, &["remote", "add", "origin", &origin]);
                git(root, &["push", "-q", "-u", "origin", "main"]);
                // A branch up there nobody is sitting on: the remote refuses to
                // lose the branch its own `HEAD` is on, which is a different case.
                git(root, &["push", "-q", "origin", "main:spike"]);
                Vec::new()
            },
        );
        show_panel(cx, &fixture, "panel-git", Some(Panel::Git));
        assert!(
            click_until(cx, &fixture, "git-graph-open".to_string(), |cx| found(
                cx,
                &fixture,
                "git-graph-rows"
            )),
            "the graph did not open"
        );
        assert!(
            draw_until(cx, &fixture, |cx| found(
                cx,
                &fixture,
                "git-graph-branch-origin/spike"
            )),
            "the remote branch is not in the graph"
        );

        // The menu opens on a remote branch too, and the question it asks is the
        // remote's.
        right_click(cx, &fixture, "git-graph-branch-origin/spike".to_string());
        assert!(
            draw_until(cx, &fixture, |cx| found(
                cx,
                &fixture,
                "git-graph-branch-menu-delete"
            )),
            "right-clicking a remote branch did not open its menu"
        );
        click_visible(cx, &fixture, "git-graph-branch-menu-delete");
        assert!(
            draw_until(cx, &fixture, |cx| found(cx, &fixture, "ok")),
            "deleting a remote branch did not ask"
        );
        assert!(
            found(cx, &fixture, "git-graph-branch-origin/spike"),
            "the branch went before the question was answered"
        );

        click_visible(cx, &fixture, "ok");
        assert!(
            draw_until(cx, &fixture, |cx| !found(
                cx,
                &fixture,
                "git-graph-branch-origin/spike"
            )),
            "the deleted remote branch is still in the graph: {:?}",
            label(cx, &fixture, "git-graph-notice")
        );
        // The remote is the truth, and nothing local moved with it.
        let origin = fixture.root.parent().expect("a sibling").join("origin.git");
        let said = std::process::Command::new("git")
            .args(["--git-dir", &origin.to_string_lossy()])
            .args(["rev-parse", "--verify", "refs/heads/spike"])
            .output()
            .unwrap();
        assert!(
            !said.status.success(),
            "the branch is still on the remote: {}",
            String::from_utf8_lossy(&said.stdout)
        );
        let here = std::process::Command::new("git")
            .arg("-C")
            .arg(&fixture.root)
            .args(["branch", "--format=%(refname:short)"])
            .output()
            .unwrap();
        let names = String::from_utf8_lossy(&here.stdout);
        assert!(
            names.contains("main"),
            "the local branch went with it: {names}"
        );
        // And the local branch is still drawn: the delete was the remote's.
        assert!(found(cx, &fixture, "git-graph-branch-main"));
    }

    /// The graph is a window's worth of history, and a history is longer than
    /// the window: the rows scroll inside the view while the toolbar and the
    /// column names stay where they are.
    #[gpui_kit::test]
    fn the_graph_scrolls_its_history(cx: &mut TestAppContext) {
        let fixture = open(cx, "git-graph-scroll");
        // More commits than the view can hold: forty rows is around three
        // windows' worth at 22 px each.
        for index in 0..40 {
            std::fs::write(fixture.root.join("many.txt"), format!("{index}\n")).unwrap();
            git(&fixture.root, &["add", "many.txt"]);
            git(
                &fixture.root,
                &["commit", "-q", "-m", &format!("commit {index}")],
            );
        }
        show_panel(cx, &fixture, "panel-git", Some(Panel::Git));
        assert!(
            click_until(cx, &fixture, "git-graph-open".to_string(), |cx| found(
                cx,
                &fixture,
                "git-graph-rows"
            )),
            "the panel's fullscreen button did not open the graph"
        );

        // The rows are bounded by the view rather than as tall as the history:
        // a container that grew with its content has nothing left to scroll.
        let rows = bounds_of(cx, &fixture, "git-graph-rows");
        assert!(
            rows.size.height < px(40. * 22.),
            "the history is not clipped to the view: {rows:?}"
        );
        assert!(
            rows.size.height > px(0.),
            "the graph has no room to scroll in"
        );

        let newest = String::from_utf8_lossy(
            &std::process::Command::new("git")
                .arg("-C")
                .arg(&fixture.root)
                .args(["rev-parse", "--short", "HEAD"])
                .output()
                .unwrap()
                .stdout,
        )
        .trim()
        .to_string();
        let first = format!("git-graph-commit-{newest}");
        let before = bounds_of(cx, &fixture, &first);
        assert!(
            before.origin.y >= rows.origin.y && before.origin.y < rows.origin.y + rows.size.height,
            "the newest commit is not at the top of the view: {before:?} in {rows:?}"
        );

        // The wheel over the rows, which is what a reader does to see the rest.
        let at = rows.center();
        cx.update_window(fixture.window.into(), |_, window, cx| {
            window.dispatch_event(
                gpui_kit::ScrollWheelEvent {
                    position: at,
                    delta: gpui_kit::ScrollDelta::Lines(gpui_kit::point(0., -3.)),
                    ..Default::default()
                }
                .to_platform_input(),
                cx,
            );
            window.render_frame(cx);
        })
        .expect("wheel over the graph");
        draw(cx, &fixture);

        assert!(
            !found(cx, &fixture, &first),
            "an offscreen commit remained mounted after scrolling"
        );
        // A visible row after the scroll still receives clicks and expands inline.
        let target = String::from_utf8_lossy(
            &std::process::Command::new("git")
                .arg("-C")
                .arg(&fixture.root)
                .args(["rev-parse", "--short", "HEAD~5"])
                .output()
                .unwrap()
                .stdout,
        )
        .trim()
        .to_string();
        assert!(click_until(
            cx,
            &fixture,
            format!("git-graph-commit-{target}"),
            |cx| found(cx, &fixture, "git-graph-details")
        ));
        // And the column names stay where they are while it does.
        let head = bounds_of(cx, &fixture, "git-graph-head");
        assert!(
            head.origin.y < rows.origin.y,
            "the column names scrolled away with the history: {head:?}"
        );
    }

    /// The files under a commit in the panel are a way into *that commit's*
    /// diff — a different comparison from the worktree's, which the panel's other
    /// rows show.
    #[gpui_kit::test]
    fn a_file_under_a_commit_shows_that_commits_change(cx: &mut TestAppContext) {
        let fixture = open(cx, "git-commit-diff");
        let panel = cx.update(|cx| fixture.workspace.read(cx).git.clone());
        show_panel(cx, &fixture, "panel-git", Some(Panel::Git));
        assert!(
            draw_until(cx, &fixture, |cx| cx
                .update(|cx| !panel.read(cx).commits().is_empty())),
            "the panel never read the repository"
        );
        // A change in the worktree, on top of the one the commit made: the two
        // diffs of this file are different, which is what the test is about.
        std::fs::write(
            fixture.root.join(NOTES),
            format!("{NOTES_TEXT}a later edit\n"),
        )
        .unwrap();

        let newest = cx.update(|cx| panel.read(cx).commits()[0].clone());
        let row = format!("commit-{}", newest.short);
        let notes = format!("commit-file-{NOTES}");
        assert!(
            click_until(cx, &fixture, row, |cx| found(cx, &fixture, &notes)),
            "clicking a commit row did not list the files it changed"
        );

        let workspace = fixture.workspace.clone();
        let wanted = NOTES.to_string();
        assert!(
            click_until(cx, &fixture, notes, move |cx| cx.update(|cx| workspace
                .read(cx)
                .active_diff(cx)
                .is_some_and(
                    |view| view.read(cx).selected() == Some(wanted.as_str())
                ))),
            "clicking a file under a commit did not show its diff"
        );
        let text = diff_text(cx, &fixture);
        assert!(
            text.contains("+the tree put this here"),
            "the commit's own change is not in the diff: {text:?}"
        );
        assert!(
            !text.contains("a later edit"),
            "the diff is the worktree's, not the commit's: {text:?}"
        );
        // The tab is the one diff tab, named for the file: it is another
        // comparison in the same place, not another place.
        assert_eq!(
            active_tabs(cx, &fixture)
                .iter()
                .filter(|spec| matches!(spec, TabSpec::Diff { .. }))
                .count(),
            1,
            "the commit's file opened a second diff tab"
        );

        // And the way back to the worktree's comparison is `diff::Open`.
        cx.update_window(fixture.window.into(), |_, window, cx| {
            window.dispatch_action(Box::new(crate::state::diff::Open), cx);
        })
        .expect("dispatch the diff action");
        draw(cx, &fixture);
        assert!(
            diff_text(cx, &fixture).contains("a later edit"),
            "`diff::Open` did not go back to the worktree's changes"
        );
    }

    /// The graph's own file rows are the other way into a commit's diff, and the
    /// one the reader is looking at when the graph is open: a click has to show
    /// the diff and *not* leave the graph drawn over it, which is the editor area
    /// the diff lands in.
    #[gpui_kit::test]
    fn a_file_under_a_commit_in_the_graph_shows_its_diff(cx: &mut TestAppContext) {
        let fixture = open(cx, "git-graph-commit-diff");
        show_panel(cx, &fixture, "panel-git", Some(Panel::Git));
        // A change in the worktree, on top of the one the commit made: the two
        // diffs of this file are different, which is what the test is about.
        std::fs::write(
            fixture.root.join(NOTES),
            format!("{NOTES_TEXT}a later edit\n"),
        )
        .unwrap();
        assert!(
            click_until(cx, &fixture, "git-graph-open".to_string(), |cx| found(
                cx,
                &fixture,
                "git-graph-rows"
            )),
            "the panel's fullscreen button did not open the graph"
        );

        let newest = String::from_utf8_lossy(
            &std::process::Command::new("git")
                .arg("-C")
                .arg(&fixture.root)
                .args(["rev-parse", "--short", "HEAD"])
                .output()
                .unwrap()
                .stdout,
        )
        .trim()
        .to_string();
        let notes = format!("commit-file-{NOTES}");
        assert!(
            click_until(cx, &fixture, format!("git-graph-commit-{newest}"), |cx| {
                found(cx, &fixture, &notes)
            }),
            "clicking a commit in the graph did not list the files it changed"
        );
        assert!(found(cx, &fixture, "git-graph-details"));
        let selected = bounds_of(cx, &fixture, &format!("git-graph-commit-{newest}"));
        let details = bounds_of(cx, &fixture, "git-graph-details");
        assert!(
            details.origin.y >= selected.origin.y + selected.size.height - px(1.),
            "commit details are not expanded under the selected row: {details:?} vs {selected:?}"
        );
        assert!(details.size.height > px(0.));
        assert!(click_until(
            cx,
            &fixture,
            format!("git-graph-commit-{newest}"),
            |cx| found(cx, &fixture, "git-graph-details")
        ));
        press(cx, &fixture, "escape");
        assert!(
            draw_until(cx, &fixture, |cx| !found(cx, &fixture, "git-graph-details")),
            "Escape did not close the commit details"
        );
        assert!(found(cx, &fixture, "git-graph-rows"));
        assert!(click_until(
            cx,
            &fixture,
            format!("git-graph-commit-{newest}"),
            |cx| found(cx, &fixture, "git-graph-details")
        ));

        let tabs_before_diff = active_tabs(cx, &fixture).len();
        let workspace = fixture.workspace.clone();
        let wanted = NOTES.to_string();
        assert!(
            click_until(cx, &fixture, notes.clone(), move |cx| cx.update(|cx| {
                workspace
                    .read(cx)
                    .active_diff(cx)
                    .is_some_and(|view| view.read(cx).selected() == Some(wanted.as_str()))
            })),
            "clicking a file under a commit in the graph did not show its diff"
        );
        assert!(!found(cx, &fixture, "git-graph-overlay"));
        assert!(found(cx, &fixture, "git-graph-tab"));
        assert_eq!(
            active_tabs(cx, &fixture).len(),
            tabs_before_diff + 1,
            "the graph file did not open a new tab"
        );
        let text = diff_text(cx, &fixture);
        assert!(
            text.contains("+the tree put this here"),
            "the commit's own change is not in the diff: {text:?}"
        );
        assert!(
            !text.contains("a later edit"),
            "the diff is the worktree's, not the commit's: {text:?}"
        );
        assert!(click_until(
            cx,
            &fixture,
            "git-graph-tab".to_string(),
            |cx| found(cx, &fixture, "git-graph-overlay")
                && found(cx, &fixture, &format!("git-graph-commit-{newest}"))
        ));
        assert!(found(cx, &fixture, &notes));
    }

    #[gpui_kit::test]
    fn scrolling_commit_files_does_not_scroll_the_graph_history(cx: &mut TestAppContext) {
        let fixture = open(cx, "git-graph-inner-scroll");
        for index in 0..30 {
            std::fs::write(
                fixture.root.join(format!("detail-{index:02}.md")),
                "change\n",
            )
            .unwrap();
        }
        git(&fixture.root, &["add", "-A"]);
        git(&fixture.root, &["commit", "-q", "-m", "many changed files"]);
        show_panel(cx, &fixture, "panel-git", Some(Panel::Git));
        assert!(click_until(
            cx,
            &fixture,
            "git-graph-open".to_string(),
            |cx| found(cx, &fixture, "git-graph-rows")
        ));
        let commit = String::from_utf8_lossy(
            &std::process::Command::new("git")
                .arg("-C")
                .arg(&fixture.root)
                .args(["rev-parse", "--short", "HEAD"])
                .output()
                .unwrap()
                .stdout,
        )
        .trim()
        .to_string();
        assert!(click_until(
            cx,
            &fixture,
            format!("git-graph-commit-{commit}"),
            |cx| found(cx, &fixture, "git-graph-detail-files")
        ));
        let selected = format!("git-graph-commit-{commit}");
        let selected_before = bounds_of(cx, &fixture, &selected);
        let file_id = "commit-file-detail-00.md";
        let file_before = bounds_of(cx, &fixture, file_id);
        let at = bounds_of(cx, &fixture, "git-graph-detail-files").center();
        cx.update_window(fixture.window.into(), |_, window, cx| {
            window.dispatch_event(
                gpui_kit::ScrollWheelEvent {
                    position: at,
                    delta: gpui_kit::ScrollDelta::Lines(gpui_kit::point(0., -3.)),
                    ..Default::default()
                }
                .to_platform_input(),
                cx,
            );
            window.render_frame(cx);
        })
        .expect("wheel over commit files");
        draw(cx, &fixture);
        let selected_after = bounds_of(cx, &fixture, &selected);
        let file_after = bounds_of(cx, &fixture, file_id);
        assert_eq!(
            selected_after.origin.y, selected_before.origin.y,
            "scrolling commit files moved the graph row: {selected_before:?} -> {selected_after:?}"
        );
        assert!(
            file_after.origin.y < file_before.origin.y,
            "the detail file list did not scroll: {file_before:?} -> {file_after:?}"
        );
    }

    /// A status letter is a column a reader scans down, in both lists that draw
    /// one — the panel's changes, and the files under a commit. A row for a file
    /// in the repository's root has no directory to fill the middle of it, and
    /// its letter used to end up against the name while every row around it had
    /// its own at the right edge.
    #[gpui_kit::test]
    fn a_status_letter_keeps_to_one_column(cx: &mut TestAppContext) {
        let fixture = open(cx, "git-status-column");
        let panel = cx.update(|cx| fixture.workspace.read(cx).git.clone());
        // One change in the root and one under `src`: the two rows that differ
        // only in whether there is a directory to put between the name and the
        // letter.
        std::fs::write(fixture.root.join(NOTES), "rewritten\n").unwrap();
        std::fs::write(
            fixture.root.join("src").join("main.rs"),
            "fn main() {}\n// a change\n",
        )
        .unwrap();
        refresh(cx, &fixture);
        show_panel(cx, &fixture, "panel-git", Some(Panel::Git));
        assert!(
            draw_until(cx, &fixture, |cx| cx.update(|cx| panel
                .read(cx)
                .changes()
                .len()
                == 2)),
            "the panel never read the changes"
        );

        // The changes section's rows, and the same question of them.
        let edge = |cx: &mut TestAppContext, id: &str| bounds_of(cx, &fixture, id).right().as_f32();
        let root = edge(cx, &format!("git-file-status-{NOTES}"));
        let nested = edge(cx, "git-file-status-src/main.rs");
        assert!(
            (root - nested).abs() <= 1.,
            "the changes section's letters do not line up: {root} vs {nested}"
        );

        // And the rows the graph draws under a commit, which is the list the
        // letter was reported off: the fixture's newest commit holds both files.
        assert!(
            click_until(cx, &fixture, "git-graph-open".to_string(), |cx| found(
                cx,
                &fixture,
                "git-graph-rows"
            )),
            "the panel's fullscreen button did not open the graph"
        );
        let newest = String::from_utf8_lossy(
            &std::process::Command::new("git")
                .arg("-C")
                .arg(&fixture.root)
                .args(["rev-parse", "--short", "HEAD"])
                .output()
                .unwrap()
                .stdout,
        )
        .trim()
        .to_string();
        assert!(
            click_until(cx, &fixture, format!("git-graph-commit-{newest}"), |cx| {
                found(cx, &fixture, &format!("commit-file-{NOTES}"))
            }),
            "clicking a commit in the graph did not list the files it changed"
        );
        let root = edge(cx, &format!("commit-file-status-{NOTES}"));
        let nested = edge(cx, "commit-file-status-src/main.rs");
        assert!(
            (root - nested).abs() <= 1.,
            "the graph's commit rows do not line up: {root} vs {nested}"
        );
    }

    /// The source control panel's two lists fold, and each one is capped and
    /// scrolls inside itself: neither of them may push the branch, the message
    /// box and the button — which are what the panel is *for* — off the top.
    #[gpui_kit::test]
    fn the_source_control_sections_fold_and_cap(cx: &mut TestAppContext) {
        let fixture = open(cx, "git-sections");
        let panel = cx.update(|cx| fixture.workspace.read(cx).git.clone());
        // More changed files than the section will draw: forty rows are roughly
        // three panels' worth, in a pane that is 640 px tall.
        for index in 0..40 {
            std::fs::write(fixture.root.join(format!("change-{index}.txt")), "x\n").unwrap();
        }
        show_panel(cx, &fixture, "panel-git", Some(Panel::Git));
        assert!(
            draw_until(cx, &fixture, |cx| cx.update(|cx| panel
                .read(cx)
                .changes()
                .len()
                == 40)),
            "the panel never read the changes"
        );

        // The history is the footer: it sits against the bottom edge of the panel, and the changes take
        // what is left between it and the message box.
        let panel_bounds = bounds_of(cx, &fixture, "git-panel");
        let footer = bounds_of(cx, &fixture, "git-history");
        assert!(
            footer.origin.y + footer.size.height
                >= panel_bounds.origin.y + panel_bounds.size.height - px(2.),
            "the history is not against the bottom of the panel: {footer:?} in {panel_bounds:?}"
        );
        // The rows scroll inside the section rather than pushing the footer down:
        // forty files are three panels' worth.
        let rows = bounds_of(cx, &fixture, "git-change-rows");
        assert!(
            rows.origin.y + rows.size.height <= footer.origin.y,
            "the file section ran into the footer: {rows:?} vs {footer:?}"
        );
        // And the fixed part is still on screen, above it.
        for id in ["git-branch", "git-summary", "git-message", "git-primary"] {
            let bounds = bounds_of(cx, &fixture, id);
            assert!(
                bounds.origin.y >= px(0.) && bounds.origin.y < rows.origin.y,
                "{id} is not above the section that is supposed to scroll: {bounds:?}"
            );
        }
        // Past the cap the list says where the rest is.
        assert!(found(cx, &fixture, "git-view-all"));

        // Folding it away puts the rows away and leaves the header as the way
        // back — the chevron in the section header.
        click_visible(cx, &fixture, "git-changes-header");
        assert!(
            draw_until(cx, &fixture, |cx| !found(cx, &fixture, "git-change-rows")),
            "folding the file section left its rows drawn"
        );
        assert!(found(cx, &fixture, "git-changes-header"));
        click_visible(cx, &fixture, "git-changes-header");
        assert!(
            draw_until(cx, &fixture, |cx| found(cx, &fixture, "git-change-rows")),
            "unfolding the file section did not bring its rows back"
        );

        // The commit list is the same: a header, rows that scroll inside it, and
        // a chevron. The fixture's repository has commits, so this is the list
        // the panel opens with.
        assert!(found(cx, &fixture, "git-commit-rows"));
        click_visible(cx, &fixture, "git-commits-header");
        assert!(
            draw_until(cx, &fixture, |cx| !found(cx, &fixture, "git-commit-rows")),
            "folding the commits left them drawn"
        );
        // Folded, it is one header line against the bottom edge — the space it
        // would have taken is left empty.
        let folded = bounds_of(cx, &fixture, "git-commits-header");
        assert!(
            folded.size.height < px(30.),
            "the folded section is not just its header: {folded:?}"
        );
        assert!(
            folded.origin.y + folded.size.height
                >= panel_bounds.origin.y + panel_bounds.size.height - px(2.),
            "the folded section did not stay at the bottom: {folded:?}"
        );
        // And the file list, which is the flexible middle, keeps the space it
        // was given rather than collapsing with it.
        let rows = bounds_of(cx, &fixture, "git-change-rows");
        assert!(
            rows.origin.y + rows.size.height <= folded.origin.y,
            "the file list is not above the folded footer: {rows:?} vs {folded:?}"
        );
    }

    /// The sidebar's leading edge drags, in the direction its side implies, and
    /// where the drag stops is what the next run opens with.
    #[gpui_kit::test]
    fn the_sidebar_edge_drag_resizes_it_and_is_remembered(cx: &mut TestAppContext) {
        let fixture = open(cx, "sidebar-drag");
        let workspace = fixture.workspace.clone();
        assert_eq!(cx.update(|cx| workspace.read(cx).right_width()), 260.);

        let handle = bounds_of(cx, &fixture, "panel-resize");
        let from = Point::new(handle.origin.x + px(3.), handle.origin.y + px(100.));
        // Left, because this panel's leading edge is the one the handle drags:
        // the panel grows towards the pane area, not away from the window.
        let to = Point::new(from.x - px(40.), from.y);
        cx.update_window(fixture.window.into(), |_, window, cx| {
            window.drag(from, to, cx)
        })
        .expect("drag the edge");
        draw(cx, &fixture);

        assert_eq!(cx.update(|cx| workspace.read(cx).right_width()), 300.);
        assert_eq!(
            cx.update(|cx| fixture.state.read(cx).right_width()),
            Some(300.),
            "the width the drag ended on was not written to state"
        );
    }

    /// The same drag, on a horizontal divider: a stacked split is dragged along `y`, so an upwards
    /// drag takes the *first* pane's height with it.
    #[gpui_kit::test]
    fn a_stacked_divider_follows_the_pointer_up(cx: &mut TestAppContext) {
        let fixture = open(cx, "split-drag-stacked");
        click(cx, &fixture, tree_row(&fixture, NOTES));
        assert!(draw_until(cx, &fixture, |cx| file_tabs(&active_tabs(
            cx, &fixture
        ))
        .len()
            == 1));
        press(cx, &fixture, "cmd-shift-d");
        draw(cx, &fixture);

        let leading = bounds_of(cx, &fixture, "pane-0");
        let divider = bounds_of(cx, &fixture, "pane-divider");
        let height_before = leading.size.height.as_f32();

        // Upwards: the divider is at the bottom of the room the split has for
        // its first pane, so the direction it can still move in is up.
        let from = Point::new(divider.origin.x + px(60.), divider.origin.y + px(3.));
        let to = Point::new(from.x, from.y - px(60.));
        cx.update_window(fixture.window.into(), |_, window, cx| {
            window.drag(from, to, cx)
        })
        .expect("drag the divider");
        draw(cx, &fixture);

        let after = bounds_of(cx, &fixture, "pane-0");
        // One for one: the pane loses exactly what the pointer travelled.
        let moved = height_before - after.size.height.as_f32();
        assert!(
            (moved - 60.).abs() <= 1.5,
            "the divider did not follow the pointer: {height_before} -> {} for a 60px drag",
            after.size.height.as_f32()
        );
        // And it moved only along that axis: a pane that changed width during a
        // vertical drag is the same bug with the sign the other way round.
        assert_eq!(
            after.size.width.as_f32(),
            leading.size.width.as_f32(),
            "a vertical drag changed the pane's width"
        );
    }

    /// The tab bar's `+`: it opens a menu of *what kind of tab*
    /// — a terminal, or one of the configured agents — and the tab it makes is
    /// the one you look at. A worktree holds as many terminals as the user opens.
    #[gpui_kit::test]
    fn the_add_tab_menu_opens_a_terminal_in_a_new_tab(cx: &mut TestAppContext) {
        let settings = Settings {
            agents: vec![AgentSpec {
                id: "sleeper".into(),
                name: "Sleeper".into(),
                command: "/bin/sh".into(),
                args: vec!["-c".into(), "sleep 30".into()],
                resume_args: Vec::new(),
                new_args: Vec::new(),
                env: Default::default(),
                status: Vec::new(),
            }],
            ..Settings::default()
        };
        let fixture = open_with_settings(cx, "add-tab", settings);
        let shell = cx
            .update(|cx| fixture.workspace.read(cx).terminal_tab(cx))
            .expect("the worktree's shell");
        assert_eq!(terminal_tabs(cx, &fixture), 1);

        click_visible(cx, &fixture, "add-tab");
        assert!(
            draw_until(cx, &fixture, |cx| found(cx, &fixture, "add-menu")),
            "the + did not open its menu"
        );
        assert_eq!(
            label(cx, &fixture, "add-menu-terminal").as_deref(),
            Some("New terminal")
        );
        // And one row per configured agent.
        assert_eq!(
            label(cx, &fixture, "add-menu-agent-sleeper").as_deref(),
            Some("Sleeper")
        );

        click_visible(cx, &fixture, "add-menu-agent-sleeper");
        assert!(
            draw_until(cx, &fixture, |cx| terminal_tabs(cx, &fixture) == 2),
            "the menu did not open a second tab"
        );
        assert!(!found(cx, &fixture, "add-menu"), "the menu stayed open");
        let started = cx
            .update(|cx| fixture.workspace.read(cx).active_terminal(cx))
            .expect("the tab the menu just opened");
        assert_ne!(
            started.entity_id(),
            shell.entity_id(),
            "the menu reused the shell's tab instead of opening one"
        );
        assert_eq!(
            cx.update(|cx| started.read(cx).agent_id().cloned()),
            Some("sleeper".to_string())
        );
        // The shell is still there beside it: a worktree holds both.
        assert_eq!(
            cx.update(|cx| fixture.workspace.read(cx).terminal_tab(cx))
                .expect("the worktree's shell")
                .entity_id(),
            shell.entity_id()
        );

        // Closing one tab leaves the other: the worktree is not a
        // single-terminal box any more. The agent is still running, so the
        // close asks first and nothing goes until it is confirmed.
        click_visible(cx, &fixture, "tab-close-1");
        assert!(
            draw_until(cx, &fixture, |cx| found(cx, &fixture, "ok")),
            "closing a running agent's tab did not ask"
        );
        assert_eq!(
            terminal_tabs(cx, &fixture),
            2,
            "the tab went before the answer"
        );
        click_visible(cx, &fixture, "ok");
        assert!(
            draw_until(cx, &fixture, |cx| !found(cx, &fixture, "ok")),
            "the confirmation stayed open"
        );
        assert!(
            draw_until(cx, &fixture, |cx| terminal_tabs(cx, &fixture) == 1),
            "closing the new tab did not leave the shell"
        );
        assert_eq!(
            cx.update(|cx| fixture.workspace.read(cx).active_terminal(cx))
                .expect("the shell")
                .entity_id(),
            shell.entity_id()
        );
    }

    /// An agent tab with no task known yet is named for the worktree it is in,
    /// the same as its sidebar row: the branch, or the worktree's own name when
    /// git has no branch on it. Not the agent's own title, which names the directory the worktree
    /// sits in — the repository.
    #[gpui_kit::test]
    fn an_agent_tab_is_named_for_its_worktree(cx: &mut TestAppContext) {
        let settings = Settings {
            agents: vec![AgentSpec {
                id: "sleeper".into(),
                name: "Sleeper".into(),
                command: "/bin/sh".into(),
                args: vec!["-c".into(), "sleep 30".into()],
                resume_args: Vec::new(),
                new_args: Vec::new(),
                env: Default::default(),
                status: Vec::new(),
            }],
            ..Settings::default()
        };
        let fixture = open_with_settings(cx, "agent-tab-name", settings);
        // Read from the state rather than spelled out: which branch the
        // fixture's worktree is on is the fixture's business.
        let called = cx.update(|cx| {
            let state = fixture.state.read(cx);
            let id = state.active_id().cloned().expect("an active worktree");
            branch_or_name(state.worktree(&id).expect("the active worktree")).to_string()
        });

        click_visible(cx, &fixture, "add-tab");
        assert!(
            draw_until(cx, &fixture, |cx| found(cx, &fixture, "add-menu")),
            "the + did not open its menu"
        );
        click_visible(cx, &fixture, "add-menu-agent-sleeper");
        assert!(
            draw_until(cx, &fixture, |cx| terminal_tabs(cx, &fixture) == 2),
            "the menu did not open a second tab"
        );
        assert_eq!(
            label(cx, &fixture, "tab-1"),
            Some(format!("Sleeper - {called}")),
            "the agent tab is not named for its worktree"
        );
        // And the same name is on the row it belongs to: a tab and a row that
        // read the same are how one is found from the other.
        let worktree_id = cx.update(|cx| {
            fixture
                .state
                .read(cx)
                .active_id()
                .cloned()
                .expect("an active worktree")
        });
        assert!(
            draw_until(cx, &fixture, |cx| readout_rows(cx, &fixture, &worktree_id)
                .iter()
                .any(|title| *title == format!("Sleeper - {called}"))),
            "no row named like the tab: {:?}",
            readout_rows(cx, &fixture, &worktree_id)
        );
    }

    /// A terminal whose child is gone keeps its row and says how it went: the
    /// word is the exit — `Failed` for a non-zero one — and the length stops where
    /// the child did.
    #[gpui_kit::test]
    fn a_terminal_that_exited_says_so_and_how_long_it_ran(cx: &mut TestAppContext) {
        let settings = Settings {
            agents: vec![AgentSpec {
                id: "quitter".into(),
                name: "Quitter".into(),
                command: "/bin/sh".into(),
                args: vec!["-c".into(), "exit 3".into()],
                resume_args: Vec::new(),
                new_args: Vec::new(),
                env: Default::default(),
                status: Vec::new(),
            }],
            ..Settings::default()
        };
        let fixture = open_with_settings(cx, "exited-terminal", settings);
        let worktree_id = cx.update(|cx| {
            fixture
                .state
                .read(cx)
                .active_id()
                .cloned()
                .expect("an active worktree")
        });
        let called = cx.update(|cx| {
            branch_or_name(
                fixture
                    .state
                    .read(cx)
                    .worktree(&worktree_id)
                    .expect("the worktree"),
            )
            .to_string()
        });
        cx.dispatch_action(
            fixture.window.into(),
            crate::state::terminal::New {
                worktree_id: worktree_id.clone(),
                agent: Some("quitter".to_string()),
                resume: None,
                seed: None,
                model: None,
            },
        );

        // The row is named for the agent and its worktree, and it is not a
        // disappearing act: a finished terminal is a row with news.
        assert!(
            draw_until(cx, &fixture, |cx| found(
                cx,
                &fixture,
                &format!("terminal-{worktree_id}-Quitter - {called}")
            )),
            "the terminal never got a row"
        );
        let status_id = format!("terminal-status-{worktree_id}-Quitter - {called}");
        assert!(
            draw_until(cx, &fixture, |cx| label(cx, &fixture, &status_id)
                .as_deref()
                == Some("Failed")),
            "the exited terminal does not say how it went: {:?}",
            label(cx, &fixture, &status_id)
        );
        let uptime_id = format!("terminal-uptime-{worktree_id}-Quitter - {called}");
        let uptime = label(cx, &fixture, &uptime_id).expect("a length for a finished terminal");
        assert!(
            uptime.ends_with('s') || uptime.ends_with('m'),
            "the length is not one: {uptime}"
        );
    }

    /// `cmd-t`, the key for the same thing the `+` does.
    #[gpui_kit::test]
    fn cmd_t_opens_a_new_terminal_tab(cx: &mut TestAppContext) {
        let fixture = open(cx, "add-tab-key");
        assert_eq!(terminal_tabs(cx, &fixture), 1);
        activate(cx, &fixture);
        press(cx, &fixture, "cmd-t");
        assert!(
            draw_until(cx, &fixture, |cx| terminal_tabs(cx, &fixture) == 2),
            "cmd-t did not open a tab"
        );
        assert!(
            found(cx, &fixture, "tab-1"),
            "the new tab is not in the bar"
        );
    }

    /// The right sidebar stays inside the window whatever the pane area does.
    #[gpui_kit::test]
    fn a_wide_pane_area_does_not_push_the_sidebar_out(cx: &mut TestAppContext) {
        let fixture = open(cx, "layout-wide-panes");
        // Two terminals side by side, which is the widest the pane area gets on
        // its own: each one measures its grid, and two grids are two minima.
        press(cx, &fixture, "cmd-d");
        draw(cx, &fixture);

        let window = cx
            .update_window(fixture.window.into(), |_, window, _| {
                window.viewport_size().width
            })
            .expect("read the window");
        for id in ["sidebar", "tab-bar", "right-sidebar"] {
            let bounds = bounds_of(cx, &fixture, id);
            assert!(
                bounds.origin.x >= px(0.) && bounds.origin.x + bounds.size.width <= window,
                "{id} is outside the {window:?} window: {bounds:?}"
            );
        }
        // And the strip is still there to be clicked, which is what going out of
        // the window takes away.
        assert!(found(cx, &fixture, "panel-git"));
    }

    /// A width a previous run was dragged to is what this one opens with, and
    /// one from a window wide enough to have written more than the sidebar can
    /// be is clamped rather than used as it came in.
    #[gpui_kit::test]
    fn a_remembered_width_is_what_the_sidebar_opens_with(cx: &mut TestAppContext) {
        let remembered = open_seeded(cx, "sidebar-width-remembered", |state| {
            state.right_width = Some(300.);
        });
        assert_eq!(
            cx.update(|cx| remembered.workspace.read(cx).right_width()),
            300.,
            "a remembered width was not what the sidebar opened with"
        );

        let clamped = open_seeded(cx, "sidebar-width-clamped", |state| {
            state.right_width = Some(9_000.);
        });
        assert_eq!(
            cx.update(|cx| clamped.workspace.read(cx).right_width()),
            crate::right_sidebar::MAX_WIDTH,
            "an out-of-range width was used as it came in"
        );
    }

    /// A scratch home holding one Pi transcript for `cwd`, and the roots to scan it with.
    fn pi_history(home: &Path, cwd: &Path, id: &str, title: &str) -> SessionRoots {
        let cwd = cwd.display().to_string();
        let bucket = home.join(".pi/agent/sessions").join(format!(
            "--{}--",
            cwd.trim_start_matches('/').replace('/', "-")
        ));
        std::fs::create_dir_all(&bucket).unwrap();
        let lines = [
            format!(
                "{{\"type\":\"session\",\"version\":3,\"id\":\"{id}\",\"timestamp\":\"2026-10-12T01:00:01.191Z\",\"cwd\":\"{cwd}\"}}"
            ),
            "{\"type\":\"model_change\",\"id\":\"m\",\"provider\":\"anthropic\",\"modelId\":\"claude-opus-5\"}".to_string(),
            format!(
                "{{\"type\":\"message\",\"id\":\"a\",\"message\":{{\"role\":\"user\",\"content\":[{{\"type\":\"text\",\"text\":\"{title}\"}}]}}}}"
            ),
            "{\"type\":\"message\",\"id\":\"b\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"On it.\"}]}}".to_string(),
        ];
        std::fs::write(
            bucket.join(format!("2026-10-12T01-00-01-191Z_{id}.jsonl")),
            format!("{}\n", lines.join("\n")),
        )
        .unwrap();
        SessionRoots::from_home(home)
    }

    /// Open the session history panel on a scratch home, and wait for the scan
    /// to land: the panel reads the transcripts on the first frame it is drawn
    /// in, and that read is a background task.
    fn open_sessions(
        cx: &mut TestAppContext,
        fixture: &Fixture,
        roots: SessionRoots,
    ) -> Entity<crate::session_panel::SessionPanel> {
        let panel = cx.update(|cx| fixture.workspace.read(cx).sessions.clone());
        // Before the panel is drawn, so the first scan reads the fixture rather
        // than this machine's own history.
        cx.update(|cx| panel.update(cx, |panel, _cx| panel.set_roots(roots)));
        click(cx, fixture, "panel-sessions".to_string());
        let landed = panel.clone();
        assert!(
            draw_until(cx, fixture, |cx| cx
                .update(|cx| !landed.read(cx).sessions().is_empty())),
            "the scan never landed"
        );
        panel
    }

    /// The session history panel lists a row per past session, with the agent,
    /// the turns, the age and the model.
    #[gpui_kit::test]
    fn the_session_panel_lists_past_sessions(cx: &mut TestAppContext) {
        let fixture = open(cx, "sessions-panel");
        let roots = pi_history(
            &fixture._dir.join("home"),
            &fixture.root,
            "aaaa1111",
            "fix the flaky test",
        );
        let panel = open_sessions(cx, &fixture, roots);

        assert!(found(cx, &fixture, "session-panel"));
        // It took the sidebar from the file tree, which is the panel a window
        // opens with.
        assert!(!found(cx, &fixture, "file-tree"), "the tree is still drawn");
        // And the way back is the strip, which is still there.
        assert!(found(cx, &fixture, "panel-files"));

        let session = cx.update(|cx| panel.read(cx).sessions()[0].clone());
        assert_eq!(session.title, "fix the flaky test");
        assert_eq!(session.preview, "On it.");
        assert_eq!(session.messages, 2);
        assert_eq!(session.model.as_deref(), Some("claude-opus-5"));
        // Grouped by the directory the agent worked in, which is this repo's own
        // checkout — the worktree the fixture activates.
        assert_eq!(
            session.cwd,
            fixture
                .root
                .canonicalize()
                .unwrap_or_else(|_| fixture.root.clone())
        );
        assert_eq!(
            label(cx, &fixture, "session-aaaa1111").as_deref(),
            Some("fix the flaky test"),
            "the row lost its accessible name"
        );

        // And the strip takes the sidebar back to the files.
        click(cx, &fixture, "panel-files".to_string());
        assert!(
            draw_until(cx, &fixture, |cx| found(cx, &fixture, "file-tree")),
            "the strip did not switch back to the files"
        );
        assert!(!found(cx, &fixture, "session-panel"));
    }

    /// The list is the repository on screen: every worktree of it, and nothing from elsewhere.
    #[gpui_kit::test]
    fn the_session_list_is_the_repository_on_screen(cx: &mut TestAppContext) {
        let fixture = open(cx, "sessions-scope");
        let home = fixture._dir.join("home");
        let roots = pi_history(&home, &fixture.root, "aaaa1111", "in this worktree");
        // A second project, outside every worktree this window knows about.
        let elsewhere = home.join("somewhere-else");
        std::fs::create_dir_all(&elsewhere).unwrap();
        pi_history(&home, &elsewhere, "bbbb2222", "somewhere else");
        // A sibling worktree of the same repository.
        let sibling = fixture.root.parent().unwrap().join("wt-one");
        let sibling = sibling.canonicalize().unwrap_or(sibling);
        pi_history(&home, &sibling, "cccc3333", "in the sibling worktree");

        let panel = open_sessions(cx, &fixture, roots);
        assert_eq!(cx.update(|cx| panel.read(cx).sessions().len()), 3);
        draw(cx, &fixture);
        assert!(
            found(cx, &fixture, "session-aaaa1111")
                && found(cx, &fixture, "session-cccc3333")
                && !found(cx, &fixture, "session-bbbb2222"),
            "the list is not the repository's worktrees"
        );
    }

    /// A row's click resumes the session: the agent that wrote the transcript,
    /// in the worktree the transcript came from, with the session's own id and
    /// path for the resume flags.
    #[gpui_kit::test]
    fn resuming_a_session_starts_its_agent_in_the_worktree_it_came_from(cx: &mut TestAppContext) {
        // A preset that can actually run in a test: `/bin/sh`, with the resume
        // flags appended where `term_spec_for_agent` puts them.
        let settings = Settings {
            agents: vec![AgentSpec {
                id: "pi".into(),
                name: "Pi (test)".into(),
                command: "/bin/sh".into(),
                args: vec!["-c".into(), "sleep 30".into()],
                resume_args: vec!["--session".into(), "{path}".into()],
                new_args: Vec::new(),
                env: Default::default(),
                status: Vec::new(),
            }],
            ..Settings::default()
        };
        let fixture = open_with_settings(cx, "sessions-resume", settings);
        let roots = pi_history(
            &fixture._dir.join("home"),
            &fixture.root,
            "aaaa1111",
            "carry on",
        );
        let panel = open_sessions(cx, &fixture, roots);
        let _ = panel;

        // The row opens first: restarting a conversation is a deliberate click
        // on the word, not the side effect of looking at the row.
        click(cx, &fixture, "session-aaaa1111".to_string());
        assert!(
            draw_until(cx, &fixture, |cx| found(
                cx,
                &fixture,
                "session-aaaa1111-resume"
            )),
            "the row did not open"
        );
        click_visible(cx, &fixture, "session-aaaa1111-resume");
        let workspace = fixture.workspace.clone();
        assert!(
            draw_until(cx, &fixture, |cx| cx.update(|cx| workspace
                .read(cx)
                .active_terminal(cx)
                .is_some_and(|terminal| terminal.read(cx).resume().is_some()))),
            "the click did not start a resumed terminal"
        );

        let terminal = cx
            .update(|cx| fixture.workspace.read(cx).active_terminal(cx))
            .expect("the resumed terminal");
        let (id, path, agent) = cx.update(|cx| {
            let view = terminal.read(cx);
            (
                view.resume().map(|resume| resume.id.clone()),
                view.resume().map(|resume| resume.path.clone()),
                view.agent_id().cloned(),
            )
        });
        assert_eq!(id.as_deref(), Some("aaaa1111"));
        assert_eq!(
            agent.as_deref(),
            Some("pi"),
            "the preset is the session's agent"
        );
        assert_eq!(
            path,
            Some(
                fixture
                    ._dir
                    .join("home/.pi/agent/sessions")
                    .join(format!(
                        "--{}--",
                        fixture
                            .root
                            .display()
                            .to_string()
                            .trim_start_matches('/')
                            .replace('/', "-")
                    ))
                    .join("2026-10-12T01-00-01-191Z_aaaa1111.jsonl")
            )
        );
    }

    /// A row opens onto more of itself — where it worked and which transcript
    /// it is — and three words: continue, start over, delete.
    #[gpui_kit::test]
    fn a_session_row_opens_onto_its_details_and_its_buttons(cx: &mut TestAppContext) {
        let fixture = open(cx, "sessions-expand");
        let home = fixture._dir.join("home");
        let roots = pi_history(&home, &fixture.root, "aaaa1111", "carry on");
        pi_history(&home, &fixture.root, "bbbb2222", "another session");
        let _ = open_sessions(cx, &fixture, roots);

        // Shut until asked: a list where every row drew three buttons would be a
        // list of buttons.
        draw(cx, &fixture);
        assert!(!found(cx, &fixture, "session-aaaa1111-resume"));
        assert!(!found(cx, &fixture, "session-aaaa1111-cwd"));

        click(cx, &fixture, "session-aaaa1111".to_string());
        assert!(
            draw_until(cx, &fixture, |cx| found(
                cx,
                &fixture,
                "session-aaaa1111-resume"
            )),
            "the row did not open"
        );
        assert!(found(cx, &fixture, "session-aaaa1111-new"));
        assert!(found(cx, &fixture, "session-aaaa1111-delete"));
        assert!(found(cx, &fixture, "session-aaaa1111-cwd"));
        assert!(found(cx, &fixture, "session-aaaa1111-id"));

        // Reading another session closes the previous one.
        click_visible(cx, &fixture, "session-bbbb2222");
        draw(cx, &fixture);
        assert!(found(cx, &fixture, "session-bbbb2222-resume"));
        assert!(!found(cx, &fixture, "session-aaaa1111-resume"));
        click_visible(cx, &fixture, "session-aaaa1111");
        draw(cx, &fixture);
        assert!(found(cx, &fixture, "session-aaaa1111-resume"));
        assert!(!found(cx, &fixture, "session-bbbb2222-resume"));

        // And the same click shuts it.
        click(cx, &fixture, "session-aaaa1111".to_string());
        assert!(
            draw_until(cx, &fixture, |cx| !found(
                cx,
                &fixture,
                "session-aaaa1111-resume"
            )),
            "the row did not close"
        );
    }

    /// Leaving the session list drops the open row, including project changes
    /// dispatched without a mouse click.
    #[gpui_kit::test]
    fn a_session_row_closes_when_leaving_the_list(cx: &mut TestAppContext) {
        let fixture = open(cx, "sessions-reset");
        let second = add_second_repo(cx, &fixture);
        cx.update(|cx| {
            fixture.state.update(cx, |state, cx| {
                state.activate(fixture.root.to_str().unwrap(), cx);
            });
        });
        let roots = pi_history(
            &fixture._dir.join("home"),
            &fixture.root,
            "aaaa1111",
            "carry on",
        );
        let _ = open_sessions(cx, &fixture, roots);

        for destination in ["project", "tab-0", "panel-files"] {
            click_visible(cx, &fixture, "session-aaaa1111");
            draw(cx, &fixture);
            assert!(found(cx, &fixture, "session-aaaa1111-resume"));

            if destination == "project" {
                cx.update(|cx| {
                    fixture.state.update(cx, |state, cx| {
                        state.activate(second.to_str().unwrap(), cx);
                    });
                });
                draw(cx, &fixture);
                assert!(!found(cx, &fixture, "session-aaaa1111"));
                cx.update(|cx| {
                    fixture.state.update(cx, |state, cx| {
                        state.activate(fixture.root.to_str().unwrap(), cx);
                    });
                });
            } else {
                click_visible(cx, &fixture, destination);
                draw(cx, &fixture);
                if destination == "panel-files" {
                    assert!(!found(cx, &fixture, "session-panel"));
                    click_visible(cx, &fixture, "panel-sessions");
                }
            }
            draw(cx, &fixture);
            assert!(found(cx, &fixture, "session-aaaa1111"));
            assert!(
                !found(cx, &fixture, "session-aaaa1111-resume"),
                "the row stayed open after leaving for {destination}"
            );
        }
    }

    /// "Start a new session" opens a fresh agent — the same worktree, the same
    /// preset, no resume flags — seeded with a handoff from the transcript it came
    /// from.
    #[gpui_kit::test]
    fn a_new_session_button_starts_a_fresh_agent_in_the_worktree(cx: &mut TestAppContext) {
        let settings = Settings {
            agents: vec![AgentSpec {
                id: "pi".into(),
                name: "Pi (test)".into(),
                command: "/bin/sh".into(),
                args: vec!["-c".into(), "echo carried=\"$*\"; sleep 30".into()],
                resume_args: vec!["--session".into(), "{path}".into()],
                new_args: vec!["{id}".into(), "{prompt}".into()],
                env: Default::default(),
                status: Vec::new(),
            }],
            ..Settings::default()
        };
        let fixture = open_with_settings(cx, "sessions-new", settings);
        let roots = pi_history(
            &fixture._dir.join("home"),
            &fixture.root,
            "aaaa1111",
            "carry on",
        );
        let _ = open_sessions(cx, &fixture, roots);

        click(cx, &fixture, "session-aaaa1111".to_string());
        assert!(draw_until(cx, &fixture, |cx| found(
            cx,
            &fixture,
            "session-aaaa1111-new"
        )));
        click_visible(cx, &fixture, "session-aaaa1111-new");

        let workspace = fixture.workspace.clone();
        assert!(
            draw_until(cx, &fixture, |cx| cx.update(|cx| workspace
                .read(cx)
                .active_terminal(cx)
                .is_some_and(|terminal| {
                    let view = terminal.read(cx);
                    view.agent_id().map(String::as_str) == Some("pi") && view.resume().is_none()
                }))),
            "the button did not start a fresh agent"
        );
        let terminal = cx
            .update(|cx| fixture.workspace.read(cx).active_terminal(cx))
            .expect("the new terminal");
        let (resume, agent) = cx.update(|cx| {
            let view = terminal.read(cx);
            (view.resume().cloned(), view.agent_id().cloned())
        });
        assert!(resume.is_none(), "a fresh session is not a resume");
        assert_eq!(agent.as_deref(), Some("pi"));
        // And the fresh session was seeded with what the transcript carried: the
        // original request, handed to the child as its first message.
        assert!(
            draw_until(cx, &fixture, |cx| cx.update(|cx| fixture
                .workspace
                .read(cx)
                .active_terminal(cx)
                .is_some_and(|terminal| terminal
                    .read(cx)
                    .screen()
                    .text()
                    .contains("Original request: carry on")))),
            "the handoff never reached the new session"
        );
        assert!(
            cx.update(
                |cx| fixture
                    .workspace
                    .read(cx)
                    .active_terminal(cx)
                    .is_some_and(|terminal| terminal
                        .read(cx)
                        .screen()
                        .text()
                        .contains("anthropic/claude-opus-5"))
            ),
            "the original model was not passed to the new session"
        );
    }

    /// "Delete" asks first, then removes the transcript from disk and from the
    /// list.
    #[gpui_kit::test]
    fn a_delete_button_removes_the_transcript_after_a_confirmation(cx: &mut TestAppContext) {
        let fixture = open(cx, "sessions-delete");
        let roots = pi_history(
            &fixture._dir.join("home"),
            &fixture.root,
            "aaaa1111",
            "throw me away",
        );
        let panel = open_sessions(cx, &fixture, roots);
        let path = cx.update(|cx| panel.read(cx).sessions()[0].path.clone());
        assert!(path.is_file());

        click(cx, &fixture, "session-aaaa1111".to_string());
        assert!(draw_until(cx, &fixture, |cx| found(
            cx,
            &fixture,
            "session-aaaa1111-delete"
        )));
        click_visible(cx, &fixture, "session-aaaa1111-delete");
        assert!(
            draw_until(cx, &fixture, |cx| found(cx, &fixture, "ok")),
            "the delete did not ask"
        );
        assert!(path.is_file(), "the transcript went before the answer");

        click_visible(cx, &fixture, "ok");
        assert!(
            draw_until(cx, &fixture, |_cx| !path.exists()),
            "the transcript was not removed"
        );
        assert!(
            draw_until(cx, &fixture, |cx| !found(cx, &fixture, "session-aaaa1111")),
            "the row stayed after its transcript went"
        );
    }

    /// Picking gathers a batch: the row's checkmark takes it without opening it,
    /// and one confirmation removes every chosen transcript at once.
    #[gpui_kit::test]
    fn chosen_sessions_are_deleted_in_one_confirmation(cx: &mut TestAppContext) {
        let fixture = open(cx, "sessions-delete-many");
        let home = fixture._dir.join("home");
        let roots = pi_history(&home, &fixture.root, "aaaa1111", "throw me away");
        pi_history(&home, &fixture.root, "bbbb2222", "and me too");
        pi_history(&home, &fixture.root, "cccc3333", "keep me");
        let panel = open_sessions(cx, &fixture, roots);
        // By id, not by position: three transcripts written a millisecond apart
        // sort newest first, and only the id says which one is which.
        let path_of = |cx: &mut TestAppContext, id: &str| -> PathBuf {
            cx.update(|cx| {
                panel
                    .read(cx)
                    .sessions()
                    .iter()
                    .find(|session| session.id == id)
                    .unwrap_or_else(|| panic!("no session {id}"))
                    .path
                    .clone()
            })
        };
        let (aa, bb, cc) = (
            path_of(cx, "aaaa1111"),
            path_of(cx, "bbbb2222"),
            path_of(cx, "cccc3333"),
        );

        // Off by default: the list is a list, not a form.
        draw(cx, &fixture);
        assert!(!found(cx, &fixture, "session-aaaa1111-choose"));

        click(cx, &fixture, "sessions-pick".to_string());
        assert!(
            draw_until(cx, &fixture, |cx| found(
                cx,
                &fixture,
                "session-aaaa1111-choose"
            )),
            "picking did not put a checkmark on the row"
        );
        assert!(found(cx, &fixture, "sessions-delete-chosen"));

        // The row's own click takes it, and picking does not open it.
        click(cx, &fixture, "session-aaaa1111".to_string());
        click(cx, &fixture, "session-bbbb2222".to_string());
        draw(cx, &fixture);
        assert_eq!(cx.update(|cx| panel.read(cx).chosen()), 2);
        assert_eq!(checked(cx, &fixture, "session-aaaa1111-choose"), Some(true));
        assert_eq!(
            checked(cx, &fixture, "session-cccc3333-choose"),
            Some(false)
        );
        assert!(
            !found(cx, &fixture, "session-aaaa1111-resume"),
            "picking opened a row"
        );

        click_visible(cx, &fixture, "sessions-delete-chosen");
        assert!(
            draw_until(cx, &fixture, |cx| found(cx, &fixture, "ok")),
            "the batch delete did not ask"
        );
        assert!(
            aa.is_file() && bb.is_file() && cc.is_file(),
            "transcripts went before the answer"
        );

        click_visible(cx, &fixture, "ok");
        assert!(
            draw_until(cx, &fixture, |_cx| !aa.exists() && !bb.exists()),
            "the chosen transcripts were not removed"
        );
        assert!(cc.is_file(), "a session nobody picked went with the batch");
        assert!(
            draw_until(cx, &fixture, |cx| !found(cx, &fixture, "session-aaaa1111")
                && !found(cx, &fixture, "session-bbbb2222")
                && found(cx, &fixture, "session-cccc3333")),
            "the list did not keep the session nobody picked"
        );
        // The batch is over: the panel is back to reading rows.
        assert!(
            !found(cx, &fixture, "session-cccc3333-choose"),
            "the checkmarks stayed after the delete"
        );
    }

    /// A restart re-enters the conversation: the id the tab saved, resolved back to
    /// the transcript the resume flags need.
    #[test]
    fn a_saved_session_resolves_to_the_transcript_it_named() {
        let dir = std::env::temp_dir().join(format!(
            "syro-saved-session-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let cwd = dir.join("main");
        std::fs::create_dir_all(&cwd).unwrap();
        let roots = pi_history(&dir, &cwd, "aaaa1111", "carry on");
        let agent = AgentSpec {
            id: "pi".into(),
            name: "Pi".into(),
            command: "pi".into(),
            args: Vec::new(),
            resume_args: vec!["--session".into(), "{path}".into()],
            new_args: vec!["--session-id".into(), "{id}".into()],
            env: Default::default(),
            status: Vec::new(),
        };

        let resume = saved_session(&roots, &agent, &cwd, "aaaa1111")
            .expect("the transcript the session wrote");
        assert_eq!(resume.id, "aaaa1111");
        assert_eq!(
            resume.path.file_name().unwrap().to_string_lossy(),
            "2026-10-12T01-00-01-191Z_aaaa1111.jsonl"
        );

        // An id no transcript answers to is not a resume: there is nothing to re-enter.
        assert!(saved_session(&roots, &agent, &cwd, "nosuchid").is_none());
        // An agent whose transcripts this app cannot lay out is left alone too.
        let unknown = AgentSpec {
            id: "opencode".into(),
            ..agent
        };
        assert!(saved_session(&roots, &unknown, &cwd, "aaaa1111").is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ------------------------------------------------------------ the lists

    /// The check that stands in front of the four `expect`s: a spec is only
    /// started once both halves have been looked at, so a worktree deleted
    /// while its tab was opening is a message rather than a panic.
    #[test]
    fn an_unstartable_spec_says_why() {
        let dir = std::env::temp_dir();
        let mut spec = TermSpec::shell(dir.clone(), TermSize::new(80, 24));
        assert_eq!(unrunnable(&spec), None);

        spec.command = "definitely-not-a-program-4f2a".to_string();
        assert_eq!(
            unrunnable(&spec).as_deref(),
            Some("definitely-not-a-program-4f2a is not runnable")
        );

        spec.command = "/bin/sh".to_string();
        spec.cwd = dir.join("definitely-not-a-directory-4f2a");
        let reason = unrunnable(&spec).expect("a missing directory is not startable");
        assert!(reason.ends_with("is gone"), "{reason}");
    }

    /// Keys reach the app only when the window is the active one; `activate` is
    /// the module's existing helper for that (and for the same reason).
    fn press(cx: &mut TestAppContext, fixture: &Fixture, key: &str) {
        cx.update_window(fixture.window.into(), |_, window, cx| {
            window.press(key, cx);
        })
        .unwrap_or_else(|error| panic!("press {key}: {error}"));
    }

    fn type_text(cx: &mut TestAppContext, fixture: &Fixture, text: &str) {
        cx.update_window(fixture.window.into(), |_, window, cx| {
            window.input(text, cx);
        })
        .expect("type into the palette");
    }

    /// The aria label of the drawn row, which is its title. `None` when the row
    /// is not there.
    fn palette_row(cx: &mut TestAppContext, fixture: &Fixture, position: usize) -> Option<String> {
        label(cx, fixture, &format!("palette-item-{position}"))
    }

    /// "drawn of matched", from the footer: how a test reads the list without
    /// reaching into the view the dialog owns.
    fn palette_count(cx: &mut TestAppContext, fixture: &Fixture) -> Option<String> {
        label(cx, fixture, "palette-footer")
    }

    /// The palette floats over the window instead of dimming it: no scrim, like
    /// the settings form. A click outside is still a way out — the palette
    /// measures its own box and answers at the window level — and it does not
    /// reach the control behind, which the dialog still occludes.
    #[gpui_kit::test]
    fn a_click_outside_the_palette_closes_it_without_a_scrim(cx: &mut TestAppContext) {
        let fixture = open(cx, "palette-no-scrim");
        activate(cx, &fixture);
        press(cx, &fixture, "cmd-p");
        assert!(
            draw_until(cx, &fixture, |cx| found(cx, &fixture, "palette-input")),
            "cmd-p did not open the palette"
        );

        // A click outside it: the palette closes, and the control behind it —
        // the usage chip, which opens its own panel — does not see the click.
        let palette = bounds_of(cx, &fixture, "palette");
        let chip = bounds_of(cx, &fixture, "usage-open");
        assert!(
            !palette.contains(&chip.center()),
            "the probe is under the palette: {chip:?}, {palette:?}"
        );
        click(cx, &fixture, "usage-open".to_string());
        draw(cx, &fixture);
        assert!(
            !found(cx, &fixture, "palette-input"),
            "the palette stayed up on a click outside it"
        );
        assert!(
            !found(cx, &fixture, "usage-panel"),
            "the click went past the palette"
        );
    }

    #[gpui_kit::test]
    fn escape_closes_the_palette_and_leaves_nothing_behind(cx: &mut TestAppContext) {
        let fixture = open(cx, "palette-escape");
        activate(cx, &fixture);
        press(cx, &fixture, "cmd-p");
        assert!(draw_until(cx, &fixture, |cx| palette_row(cx, &fixture, 0).is_some()));

        press(cx, &fixture, "escape");
        assert!(
            draw_until(cx, &fixture, |cx| !found(cx, &fixture, "palette-input")),
            "escape did not close the palette"
        );
        // And nothing ran: the tabs are the one terminal the fixture opened.
        assert_eq!(file_tabs(&active_tabs(cx, &fixture)).len(), 0);
        assert!(cx.update(|cx| fixture.workspace.read(cx).active_diff(cx).is_none()));
    }

    /// The arrows move the selection and leave the rows where they are: Enter
    /// runs the row that is under it, not the first one.
    #[gpui_kit::test]
    fn the_arrow_keys_move_the_selection(cx: &mut TestAppContext) {
        let fixture = open(cx, "palette-arrows");
        activate(cx, &fixture);
        // The global search, because it lists the fixture's three worktrees
        // (and the project above them) and running a row is a change of its own.
        press(cx, &fixture, "cmd-j");
        assert!(draw_until(cx, &fixture, |cx| palette_row(cx, &fixture, 0).is_some()));
        // The project, and the three worktrees under it.
        let rows: Vec<String> = (0..4)
            .filter_map(|position| palette_row(cx, &fixture, position))
            .collect();
        assert_eq!(
            rows.len(),
            4,
            "the search did not list the project and its worktrees: {rows:?}"
        );
        let before = cx.update(|cx| fixture.state.read(cx).active_id().cloned());
        assert_ne!(
            rows[2], rows[0],
            "the third row is the first one: there is nothing to move to"
        );

        // Down twice, then Enter: the third row is the one the arrows moved to.
        press(cx, &fixture, "down");
        press(cx, &fixture, "down");
        draw(cx, &fixture);
        assert_eq!(
            palette_row(cx, &fixture, 0).as_deref(),
            Some(rows[0].as_str()),
            "the rows moved, not the selection"
        );
        press(cx, &fixture, "enter");
        assert!(
            draw_until(cx, &fixture, |cx| cx.update(|cx| fixture
                .state
                .read(cx)
                .active_id()
                .cloned()
                != before)),
            "Enter after two downs did not activate the third row"
        );
    }

    #[gpui_kit::test]
    fn quick_open_lists_tracked_files_and_opens_one(cx: &mut TestAppContext) {
        let fixture = open(cx, "palette-files");
        activate(cx, &fixture);

        press(cx, &fixture, "cmd-p");
        assert!(
            draw_until(cx, &fixture, |cx| found(cx, &fixture, "palette-input")),
            "cmd-p did not open quick open"
        );
        // Nothing typed: every tracked file matches, so the two numbers agree.
        let listed = palette_count(cx, &fixture).expect("the footer is drawn");
        let numbers: Vec<&str> = listed.split(" of ").collect();
        assert_eq!(numbers.len(), 2, "unexpected footer: {listed}");
        assert_eq!(numbers[0], numbers[1], "unexpected footer: {listed}");

        // The ignored file is not in the list, because it is not tracked: a
        // query that would match it narrows to nothing.
        type_text(cx, &fixture, IGNORED);
        draw(cx, &fixture);
        assert_eq!(
            palette_count(cx, &fixture).as_deref(),
            Some("0 of 0"),
            "quick open listed a file git does not track"
        );
        assert!(found(cx, &fixture, "palette-empty"));

        // And the tracked one is, by name. Reopened rather than edited: the
        // field is the palette's, and a second open is what a user does.
        press(cx, &fixture, "escape");
        assert!(draw_until(cx, &fixture, |cx| !found(
            cx,
            &fixture,
            "palette-input"
        )));
        press(cx, &fixture, "cmd-p");
        assert!(draw_until(cx, &fixture, |cx| found(
            cx,
            &fixture,
            "palette-input"
        )));
        type_text(cx, &fixture, "notes");
        draw(cx, &fixture);
        assert_eq!(palette_count(cx, &fixture).as_deref(), Some("1 of 1"));
        assert_eq!(palette_row(cx, &fixture, 0).as_deref(), Some(NOTES));

        press(cx, &fixture, "enter");
        assert!(
            draw_until(cx, &fixture, |cx| file_tabs(&active_tabs(cx, &fixture))
                .len()
                == 1),
            "Enter did not open the file"
        );
        assert_eq!(
            file_tabs(&active_tabs(cx, &fixture))[0],
            fixture.root.join(NOTES)
        );
    }

    /// The keyboard can walk past the last row on screen: the list scrolls with
    /// the selection, so the row the arrows reached is the row in view.
    #[gpui_kit::test]
    fn the_list_scrolls_with_the_selection(cx: &mut TestAppContext) {
        let fixture = open(cx, "palette-scroll");
        // More tracked files than the list can show at once, so walking to the
        // end has to move the view, not just the highlight.
        for index in 0..60 {
            std::fs::write(
                fixture.root.join("src").join(format!("many-{index:02}.rs")),
                "fn main() {}\n",
            )
            .unwrap();
        }
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(&fixture.root)
            .args(["add", "-A"])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );

        activate(cx, &fixture);
        press(cx, &fixture, "cmd-p");
        assert!(draw_until(cx, &fixture, |cx| palette_row(cx, &fixture, 0).is_some()));

        // The list is capped at fifty rows, so fifty downs land on the last one.
        for _ in 0..50 {
            press(cx, &fixture, "down");
        }
        let viewport = bounds_of(cx, &fixture, "palette-rows");
        let last = bounds_of(cx, &fixture, "palette-item-49");
        // `Bounds::contains` is half-open, and the row that was scrolled to sits
        // flush against the bottom edge, so the check is the two edges themselves.
        assert!(
            last.origin.y >= viewport.origin.y
                && last.origin.y + last.size.height <= viewport.origin.y + viewport.size.height,
            "the last row is not in view: {last:?} in {viewport:?}"
        );
    }

    /// A second repository, so the jump has two to draw from. Built here rather
    /// than in the shared fixture because only this test needs one.
    fn add_second_repo(cx: &mut TestAppContext, fixture: &Fixture) -> PathBuf {
        let path = fixture.root.parent().unwrap().join("second-repo");
        std::fs::create_dir_all(&path).unwrap();
        for args in [
            ["init", "-q", "-b", "main", path.to_str().unwrap()].as_slice(),
            ["config", "user.email", "test@example.com"].as_slice(),
            ["config", "user.name", "syro test"].as_slice(),
        ] {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(&path)
                .args(args)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(&path)
            .args(["commit", "-q", "--allow-empty", "-m", "init"])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let path = path.canonicalize().unwrap_or(path);
        cx.update(|cx| {
            fixture
                .state
                .update(cx, |state, cx| state.add_repo(path.clone(), cx))
        });
        draw(cx, &fixture);
        path
    }

    #[gpui_kit::test]
    fn the_search_lists_every_project_and_activates_a_worktree(cx: &mut TestAppContext) {
        let fixture = open(cx, "palette-worktrees");
        let second = add_second_repo(cx, &fixture);
        activate(cx, &fixture);

        press(cx, &fixture, "cmd-j");
        assert!(
            draw_until(cx, &fixture, |cx| palette_row(cx, &fixture, 0).is_some()),
            "cmd-j did not open the search"
        );
        // Two projects and four worktrees: both repositories are listed, and so
        // are the worktrees under them.
        assert_eq!(
            palette_count(cx, &fixture).as_deref(),
            Some("6 of 6"),
            "the search did not list both repositories"
        );

        type_text(cx, &fixture, "wt-two");
        draw(cx, &fixture);
        assert_eq!(palette_count(cx, &fixture).as_deref(), Some("1 of 1"));
        press(cx, &fixture, "enter");
        assert!(
            draw_until(cx, &fixture, |cx| cx.update(|cx| fixture
                .state
                .read(cx)
                .active()
                .is_some_and(|worktree| worktree.name == "wt-two"))),
            "Enter did not activate the chosen worktree"
        );
        // The second repository is still there: the jump moved the selection
        // rather than replacing the list.
        assert!(second.exists());
    }

    /// The global search is the old worktree jump with the projects above the
    /// worktrees and the past sessions beside them: a session row resumes, and
    /// `cmd-shift-p` is one of the ways in.
    #[gpui_kit::test]
    fn the_global_search_lists_projects_worktrees_and_sessions(cx: &mut TestAppContext) {
        // A preset that can actually run in a test, so the resume is visible.
        let settings = Settings {
            agents: vec![AgentSpec {
                id: "pi".into(),
                name: "Pi (test)".into(),
                command: "/bin/sh".into(),
                args: vec!["-c".into(), "sleep 30".into()],
                resume_args: vec!["--session".into(), "{path}".into()],
                new_args: Vec::new(),
                env: Default::default(),
                status: Vec::new(),
            }],
            ..Settings::default()
        };
        let fixture = open_with_settings(cx, "palette-global", settings);
        add_second_repo(cx, &fixture);
        // One past session, in the worktree on screen.
        let roots = pi_history(
            &fixture._dir.join("home"),
            &fixture.root,
            "aaaa1111",
            "carry on",
        );
        let panel = open_sessions(cx, &fixture, roots);
        // The search lists what the readout's sweep found, so the test hands it
        // the same sessions the panel's scan just read.
        let sessions = cx.update(|cx| panel.read(cx).sessions().to_vec());
        let bar = cx.update(|cx| fixture.workspace.read(cx).resource_bar().clone());
        cx.update(|cx| bar.update(cx, |bar, _cx| bar.set_sessions(sessions)));

        activate(cx, &fixture);
        press(cx, &fixture, "cmd-shift-p");
        assert!(
            draw_until(cx, &fixture, |cx| palette_row(cx, &fixture, 0).is_some()),
            "cmd-shift-p did not open the global search"
        );
        // Two projects, four worktrees and the one session.
        assert_eq!(
            palette_count(cx, &fixture).as_deref(),
            Some("7 of 7"),
            "the search did not list everything"
        );

        let first = bounds_of(cx, &fixture, "palette-item-0");
        let second = bounds_of(cx, &fixture, "palette-item-1");
        let input = bounds_of(cx, &fixture, "palette-input");
        assert!(
            first.size.height >= px(48.),
            "the two-line row is cramped: {first:?}"
        );
        assert!(
            second.origin.y >= first.bottom() + px(3.),
            "search rows touch"
        );
        assert!(
            first.origin.y >= input.bottom() + px(8.),
            "the input crowds the results"
        );

        // The session's title finds it, and Enter resumes it.
        type_text(cx, &fixture, "carry on");
        draw(cx, &fixture);
        assert_eq!(palette_count(cx, &fixture).as_deref(), Some("1 of 1"));
        press(cx, &fixture, "enter");
        let workspace = fixture.workspace.clone();
        assert!(
            draw_until(cx, &fixture, |cx| cx.update(|cx| workspace
                .read(cx)
                .active_terminal(cx)
                .is_some_and(|terminal| terminal.read(cx).resume().is_some()))),
            "the search did not resume the session"
        );
    }

    /// The sidebar's search button opens the same global search the keys do.
    #[gpui_kit::test]
    fn the_sidebar_search_button_opens_the_global_search(cx: &mut TestAppContext) {
        let fixture = open(cx, "sidebar-search-button");
        activate(cx, &fixture);
        click(cx, &fixture, "sidebar-search".to_string());
        assert!(
            draw_until(cx, &fixture, |cx| found(cx, &fixture, "palette-input")),
            "the search button did not open the search"
        );
    }

    #[gpui_kit::test]
    fn a_keymap_override_rebinds_an_action_and_a_typo_is_reported(cx: &mut TestAppContext) {
        let mut settings = Settings::default();
        settings
            .keymap
            .insert("cmd-alt-d".to_string(), "diff::Open".to_string());
        settings.keymap.insert(
            "cmd-alt-z".to_string(),
            "workspace::NoSuchThing".to_string(),
        );
        let fixture = open_with_settings(cx, "palette-keymap", settings.clone());

        // What `main` does with the same map at startup: bind what it names, and
        // hand back what it could not.
        let unknown = cx.update(|cx| crate::palette::apply_keymap(cx, &settings.keymap));
        assert_eq!(unknown, vec!["workspace::NoSuchThing".to_string()]);

        activate(cx, &fixture);
        press(cx, &fixture, "cmd-alt-d");
        assert!(
            draw_until(cx, &fixture, |cx| cx.update(|cx| fixture
                .workspace
                .read(cx)
                .active_diff(cx)
                .is_some())),
            "the overridden key did not run the action"
        );
    }

    // --- What a restart gets back (the last item) ------------------------- The tabs are restored
    // on the first frame of a worktree, so every test here opens the fixture with a saved list
    // and asserts what the frame made of it.

    /// A split is a *tab's shape*, and the shape is what `state.json` keeps: a tab
    /// that was split comes back split — two panes in one row — not as two tabs.
    #[gpui_kit::test]
    fn a_split_tab_comes_back_split(cx: &mut TestAppContext) {
        let fixture = open_with_seeded(
            cx,
            "restore-split",
            Settings::default(),
            |_, _| Vec::new(),
            |state| {
                // The worktrees are read from git when the state opens, so the
                // path to seed against is the repository's own checkout — which is
                // the worktree the window activates.
                let root = state.repos.first().expect("the repo").path.clone();
                state.tabs = vec![TabLayout::Split {
                    axis: SplitAxis::Stacked,
                    first: Box::new(TabLayout::Pane(TabSpec::Terminal {
                        cwd: root.clone(),
                        agent: None,
                        session: None,
                    })),
                    second: Box::new(TabLayout::Pane(TabSpec::File {
                        path: root.join(NOTES),
                    })),
                }];
            },
        );

        assert_eq!(
            open_tabs(cx, &fixture),
            1,
            "a split tab came back as two tabs"
        );
        assert!(
            draw_until(cx, &fixture, |cx| on_screen(cx, &fixture).len() == 2),
            "the split did not come back: {:?}",
            on_screen(cx, &fixture)
        );
        assert!(found(cx, &fixture, "pane-divider"), "no divider came back");
        assert!(
            cx.update(|cx| fixture.workspace.read(cx).active_terminal(cx).is_some()),
            "the pane that had the keyboard is not the terminal"
        );
        // And the shape is what gets saved again: a restart of a restart.
        assert_eq!(
            cx.update(|cx| fixture.workspace.read(cx).active_tabs(cx))
                .len(),
            1
        );
    }

    #[gpui_kit::test]
    fn restored_documents_load_only_when_shown(cx: &mut TestAppContext) {
        let fixture = open_restored(cx, "restore-lazy", |root, _| {
            vec![
                TabSpec::Terminal {
                    cwd: root.to_path_buf(),
                    agent: None,
                    session: None,
                },
                TabSpec::File {
                    path: root.join(NOTES),
                },
                TabSpec::Diff {
                    worktree_id: root.display().to_string(),
                },
            ]
        });
        let loaded = |cx: &mut TestAppContext| {
            cx.update(|cx| {
                fixture
                    .workspace
                    .read(cx)
                    .panes
                    .values()
                    .flat_map(|panes| panes.values())
                    .filter(|pane| {
                        matches!(
                            pane,
                            super::PaneView::Editor { .. } | super::PaneView::Diff { .. }
                        )
                    })
                    .count()
            })
        };
        assert_eq!(loaded(cx), 0, "hidden documents allocated editors");
        assert_eq!(label(cx, &fixture, "tab-1").as_deref(), Some(NOTES));
        assert_eq!(
            saved_specs(&Store::new(fixture.state_path.clone()).load().unwrap().tabs).len(),
            3
        );

        // Reading at activation must see changes made since the window opened.
        std::fs::write(fixture.root.join(NOTES), "# Changed before activation\n").unwrap();
        click_visible(cx, &fixture, "tab-1");
        draw(cx, &fixture);
        let editor = cx
            .update(|cx| fixture.workspace.read(cx).active_editor(cx))
            .unwrap();
        assert_eq!(loaded(cx), 1);
        assert_eq!(
            cx.update(|cx| editor.read(cx).text(cx)).as_ref(),
            "# Changed before activation\n"
        );
        cx.update_window(fixture.window.into(), |_, window, cx| {
            let focus = editor.read(cx).focus_handle(cx);
            window.focus(&focus, cx);
            window.input("unsaved", cx);
        })
        .unwrap();
        draw(cx, &fixture);
        let edited = cx.update(|cx| editor.read(cx).text(cx));
        assert!(cx.update(|cx| editor.read(cx).is_dirty(cx)));

        click_visible(cx, &fixture, "tab-0");
        draw(cx, &fixture);
        // Opening through the file tree must reuse the loaded editor and its edits.
        click_visible(cx, &fixture, &tree_row(&fixture, NOTES));
        draw(cx, &fixture);
        assert_eq!(open_tabs(cx, &fixture), 3);
        assert_eq!(
            cx.update(|cx| fixture.workspace.read(cx).active_editor(cx))
                .unwrap(),
            editor
        );
        assert_eq!(cx.update(|cx| editor.read(cx).text(cx)), edited);

        show_diff(cx, &fixture, NOTES);
        draw(cx, &fixture);
        assert_eq!(loaded(cx), 2);
        assert_eq!(
            open_tabs(cx, &fixture),
            3,
            "opening a deferred Diff duplicated its tab"
        );
        assert!(
            cx.update(|cx| fixture.workspace.read(cx).active_diff(cx))
                .is_some()
        );
    }

    #[gpui_kit::test]
    fn a_hidden_document_split_loads_both_visible_panes(cx: &mut TestAppContext) {
        let fixture = open_with_seeded(
            cx,
            "restore-lazy-split",
            Settings::default(),
            |_, _| Vec::new(),
            |state| {
                let root = state.repos[0].path.clone();
                state.tabs = vec![
                    TabLayout::Pane(TabSpec::Terminal {
                        cwd: root.clone(),
                        agent: None,
                        session: None,
                    }),
                    TabLayout::Split {
                        axis: SplitAxis::SideBySide,
                        first: Box::new(TabLayout::Pane(TabSpec::File {
                            path: root.join(NOTES),
                        })),
                        second: Box::new(TabLayout::Pane(TabSpec::Diff {
                            worktree_id: root.display().to_string(),
                        })),
                    },
                ];
            },
        );
        assert_eq!(
            cx.update(|cx| fixture
                .workspace
                .read(cx)
                .panes
                .values()
                .flat_map(|panes| panes.values())
                .filter(|pane| matches!(
                    pane,
                    super::PaneView::Editor { .. } | super::PaneView::Diff { .. }
                ))
                .count()),
            0
        );
        // An explicit file open must reuse the deferred pane and load its visible sibling.
        click_visible(cx, &fixture, &tree_row(&fixture, NOTES));
        draw(cx, &fixture);
        assert_eq!(open_tabs(cx, &fixture), 2);
        assert_eq!(on_screen(cx, &fixture).len(), 2);
        assert!(found(cx, &fixture, "editor-pane"));
        assert!(found(cx, &fixture, "diff-pane"));
        assert!(found(cx, &fixture, "pane-divider"));
        let editor = cx
            .update(|cx| fixture.workspace.read(cx).active_editor(cx))
            .unwrap();
        assert_eq!(
            cx.update(|cx| editor.read(cx).text(cx)).as_ref(),
            NOTES_TEXT
        );
    }

    #[gpui_kit::test]
    fn closing_a_deferred_tab_loads_only_the_tab_it_reveals(cx: &mut TestAppContext) {
        let fixture = open_restored(cx, "restore-lazy-close", |root, _| {
            vec![
                TabSpec::Terminal {
                    cwd: root.to_path_buf(),
                    agent: None,
                    session: None,
                },
                TabSpec::File {
                    path: root.join(NOTES),
                },
                TabSpec::Diff {
                    worktree_id: root.display().to_string(),
                },
            ]
        });
        click_visible(cx, &fixture, "tab-close-2");
        draw(cx, &fixture);
        assert_eq!(open_tabs(cx, &fixture), 2);
        assert_eq!(
            cx.update(|cx| fixture
                .workspace
                .read(cx)
                .panes
                .values()
                .flat_map(|panes| panes.values())
                .filter(|pane| matches!(
                    pane,
                    super::PaneView::Editor { .. } | super::PaneView::Diff { .. }
                ))
                .count()),
            0
        );
        click_visible(cx, &fixture, "tab-close-0");
        draw(cx, &fixture);
        let editor = cx
            .update(|cx| fixture.workspace.read(cx).active_editor(cx))
            .unwrap();
        assert_eq!(
            cx.update(|cx| editor.read(cx).text(cx)).as_ref(),
            NOTES_TEXT
        );
        assert_eq!(open_tabs(cx, &fixture), 1);
    }

    #[gpui_kit::test]
    fn deferred_files_are_validated_when_activated(cx: &mut TestAppContext) {
        for (tag, name) in [("deleted", NOTES), ("binary", NUL), ("large", BIG)] {
            let fixture = open_restored(cx, &format!("restore-lazy-{tag}"), |root, _| {
                vec![
                    TabSpec::Terminal {
                        cwd: root.to_path_buf(),
                        agent: None,
                        session: None,
                    },
                    TabSpec::File {
                        path: root.join(name),
                    },
                ]
            });
            assert_eq!(open_tabs(cx, &fixture), 2);
            if tag == "deleted" {
                std::fs::remove_file(fixture.root.join(name)).unwrap();
            }
            click_visible(cx, &fixture, "tab-1");
            draw(cx, &fixture);
            assert_eq!(open_tabs(cx, &fixture), 1);
            assert!(
                cx.update(|cx| fixture.workspace.read(cx).active_terminal(cx))
                    .is_some()
            );
            assert!(
                label(cx, &fixture, "open-error")
                    .unwrap_or_default()
                    .contains(name)
            );
            assert!(
                file_tabs(&saved_specs(
                    &Store::new(fixture.state_path.clone()).load().unwrap().tabs
                ))
                .is_empty()
            );
        }
    }

    /// The three kinds come back, in the order they were saved, and the file tab
    /// holds the file.
    #[gpui_kit::test]
    fn the_saved_tabs_come_back_in_their_order(cx: &mut TestAppContext) {
        let fixture = open_restored(cx, "restore-order", |root, _| {
            vec![
                TabSpec::File {
                    path: root.join(NOTES),
                },
                TabSpec::Diff {
                    worktree_id: root.display().to_string(),
                },
                TabSpec::Terminal {
                    cwd: root.to_path_buf(),
                    agent: None,
                    session: None,
                },
            ]
        });

        let tabs = active_tabs(cx, &fixture);
        assert_eq!(tabs.len(), 3, "three were saved, three came back: {tabs:?}");
        assert!(
            matches!(&tabs[0], TabSpec::File { path } if path.ends_with(NOTES)),
            "{tabs:?}"
        );
        assert!(matches!(&tabs[1], TabSpec::Diff { .. }), "{tabs:?}");
        assert!(matches!(&tabs[2], TabSpec::Terminal { .. }), "{tabs:?}");

        // The file tab is the visible one (a restored list shows its first tab),
        // and the editor read the file: the language is the one the opener named
        // for `notes.md`, which it can only answer after reading it.
        let editor = cx
            .update(|cx| fixture.workspace.read(cx).active_editor(cx))
            .expect("the file tab is the visible one");
        assert_eq!(
            cx.update(|cx| editor.read(cx).language(cx)).as_ref(),
            "markdown",
            "the restored editor did not read the file"
        );
        // And the buffer holds it: `save` writes the whole buffer over the file,
        // so a file that survives the save is a buffer that had its text.
        cx.update_window(fixture.window.into(), |_, window, cx| {
            window.press("cmd-s", cx);
        })
        .expect("save the restored tab");
        assert_eq!(
            std::fs::read_to_string(fixture.root.join(NOTES)).unwrap(),
            NOTES_TEXT,
            "the restored buffer did not hold the file's text"
        );
    }

    /// A saved file that is gone is dropped, said out loud, and the rest of the
    /// list still comes back.
    #[gpui_kit::test]
    fn a_saved_file_that_is_gone_is_dropped_with_a_reason(cx: &mut TestAppContext) {
        let fixture = open_restored(cx, "restore-dropped", |root, _| {
            vec![
                TabSpec::File {
                    path: root.join("never-written.md"),
                },
                TabSpec::Diff {
                    worktree_id: root.display().to_string(),
                },
                TabSpec::Terminal {
                    cwd: root.to_path_buf(),
                    agent: None,
                    session: None,
                },
            ]
        });

        let tabs = active_tabs(cx, &fixture);
        assert_eq!(tabs.len(), 2, "two of the three could come back: {tabs:?}");
        assert!(matches!(&tabs[0], TabSpec::Diff { .. }), "{tabs:?}");
        assert!(matches!(&tabs[1], TabSpec::Terminal { .. }), "{tabs:?}");
        // The line above the panes is where the app says what it lost.
        let said = label(cx, &fixture, "open-error").unwrap_or_default();
        assert!(
            said.contains("never-written.md"),
            "the dropped tab was not reported: {said:?}"
        );
        // And it is out of `state.json`: a spec that cannot come back must not
        // be saved to fail again next time.
        let saved = cx.update(|cx| fixture.state.read(cx).tabs().to_vec());
        assert!(
            !saved_specs(&saved)
                .iter()
                .any(|spec| matches!(spec, TabSpec::File { .. })),
            "the dropped tab is still saved: {saved:?}"
        );
    }

    /// Another worktree's tabs are not dragged into this one, and are still
    /// saved for the worktree they belong to.
    #[gpui_kit::test]
    fn another_worktrees_tabs_stay_where_they_were(cx: &mut TestAppContext) {
        let fixture = open_restored(cx, "restore-other", |root, worktrees| {
            let elsewhere = worktrees
                .first()
                .expect("the fixture made two worktrees")
                .clone();
            vec![
                TabSpec::File {
                    path: root.join(NOTES),
                },
                TabSpec::Terminal {
                    cwd: elsewhere,
                    agent: None,
                    session: None,
                },
            ]
        });

        let tabs = active_tabs(cx, &fixture);
        assert_eq!(tabs.len(), 1, "only this worktree's tab: {tabs:?}");
        assert!(matches!(&tabs[0], TabSpec::File { .. }), "{tabs:?}");

        let saved = cx.update(|cx| fixture.state.read(cx).tabs().to_vec());
        assert!(
            saved_specs(&saved)
                .iter()
                .any(|spec| matches!(spec, TabSpec::Terminal { .. })),
            "the other worktree's tab was forgotten: {saved:?}"
        );
    }

    /// A restored terminal is a live child in the directory the spec named. The
    /// content is not restored, so what is asserted is the child
    /// itself: it is there, and it is in that directory.
    #[gpui_kit::test]
    fn a_restored_terminal_is_a_live_child_in_the_saved_directory(cx: &mut TestAppContext) {
        let fixture = open_restored(cx, "restore-terminal", |root, _| {
            vec![TabSpec::Terminal {
                cwd: root.to_path_buf(),
                agent: None,
                session: None,
            }]
        });

        // The view reports the directory it was built with, and that is the one
        // the spec named.
        let tabs = active_tabs(cx, &fixture);
        assert!(
            matches!(&tabs[0], TabSpec::Terminal { cwd, .. } if cwd == &fixture.root),
            "{tabs:?}"
        );

        // And the child agrees: `pwd` is the shell talking about itself. The
        // grid's newlines are dropped first because a long path wraps, and a
        // wrapped path is still the same path.
        let terminal = cx
            .update(|cx| fixture.workspace.read(cx).active_terminal(cx))
            .expect("the restored terminal is the visible tab");
        let focus = cx.update(|cx| terminal.read(cx).focus_handle(cx));
        cx.update_window(fixture.window.into(), |_, window, cx| {
            window.focus(&focus, cx);
            window.input("pwd", cx);
            window.press("enter", cx);
        })
        .expect("ask the restored shell where it is");

        let root = fixture.root.display().to_string();
        assert!(
            draw_until(cx, &fixture, |cx| {
                let screen = cx.update(|cx| terminal.read(cx).screen());
                screen.text().replace('\n', "").contains(&root)
            }),
            "the restored child never said where it was: {:?}",
            cx.update(|cx| terminal.read(cx).screen()).text()
        );
    }

    /// Tab in a terminal is the child's. The window's own Tab — gpui-kit's Root
    /// moves the keyboard to the next tab stop — must not fire while a terminal
    /// holds it, or a shell completion never happens and the keyboard lands on
    /// the sidebar's filter box instead.
    #[gpui_kit::test]
    fn tab_in_the_terminal_stays_in_the_terminal(cx: &mut TestAppContext) {
        let fixture = open(cx, "tab-terminal");
        let terminal = cx
            .update(|cx| fixture.workspace.read(cx).active_terminal(cx))
            .expect("the active tab is a terminal");
        let focus = cx.update(|cx| terminal.read(cx).focus_handle(cx));
        cx.update_window(fixture.window.into(), |_, window, cx| {
            window.focus(&focus, cx);
            window.press("tab", cx);
            assert_eq!(
                window.focused(cx),
                Some(focus.clone()),
                "Tab moved the keyboard out of the terminal"
            );
        })
        .expect("press tab in the terminal");
    }

    /// The wheel over a terminal **in the workspace**, not in a bare window: the panes, the tab bar
    /// and the trackers all sit between the pointer and the grid, and one of them swallowing
    /// the event is a terminal nobody can scroll.
    #[gpui_kit::test]
    fn the_wheel_scrolls_the_terminal_in_the_workspace(cx: &mut TestAppContext) {
        let settings = Settings {
            agents: vec![AgentSpec {
                id: "chatty".into(),
                name: "Chatty".into(),
                command: "/bin/sh".into(),
                args: vec!["-c".into(), "seq 1 400; sleep 30".into()],
                resume_args: Vec::new(),
                new_args: Vec::new(),
                env: Default::default(),
                status: Vec::new(),
            }],
            ..Settings::default()
        };
        let fixture = open_with_settings(cx, "wheel-workspace", settings);
        let worktree_id = cx.update(|cx| {
            fixture
                .state
                .read(cx)
                .active_id()
                .cloned()
                .expect("an active worktree")
        });
        // The menu's entry, as an action: the action is the one command channel.
        cx.update_window(fixture.window.into(), |_, window, cx| {
            window.dispatch_action(
                Box::new(crate::state::terminal::New {
                    worktree_id,
                    agent: Some("chatty".into()),
                    resume: None,
                    seed: None,
                    model: None,
                }),
                cx,
            );
        })
        .expect("start the chatty agent");
        let terminal = cx
            .update(|cx| fixture.workspace.read(cx).active_terminal(cx))
            .expect("the chatty terminal");
        assert!(
            draw_until(cx, &fixture, |cx| {
                cx.update(|cx| terminal.read(cx).screen())
                    .text()
                    .contains("400")
            }),
            "the terminal never printed its 400 lines: {:?}",
            cx.update(|cx| terminal.read(cx).screen()).text()
        );
        assert_eq!(cx.update(|cx| terminal.read(cx).screen().display_offset), 0);

        let at = bounds_of(cx, &fixture, "pane").center();
        cx.update_window(fixture.window.into(), |_, window, cx| {
            window.dispatch_event(
                gpui_kit::ScrollWheelEvent {
                    position: at,
                    delta: gpui_kit::ScrollDelta::Lines(gpui_kit::point(0., 3.)),
                    ..Default::default()
                }
                .to_platform_input(),
                cx,
            );
            window.render_frame(cx);
        })
        .expect("wheel over the terminal");
        draw(cx, &fixture);

        let offset = cx.update(|cx| terminal.read(cx).screen().display_offset);
        assert!(
            offset > 0,
            "the wheel did not scroll the workspace's terminal: {offset}"
        );
    }

    // --- The chrome's icons -------------------------------------------

    /// Every icon the chrome draws is in the set the app installs.
    #[test]
    fn every_icon_the_chrome_draws_is_in_the_installed_set() {
        use gpui_kit::AssetSource as _;
        use gpui_kit::assets::IconNamed as _;

        let installed = crate::agent_logo::AppAssets;
        let drawn = [
            ("Add repository", IconName::Plus),
            ("New worktree", IconName::Plus),
            ("Close", IconName::Close),
            ("Restart", IconName::RotateCw),
            ("Toggle right sidebar", IconName::PanelRight),
            ("New tab", IconName::Plus),
            ("Files panel", IconName::Folder),
            ("Session history panel", IconName::LayoutDashboard),
            ("Source control panel", IconName::Network),
            ("Git graph", IconName::Maximize),
            ("Refresh source control", IconName::RefreshCw),
            ("Settings", IconName::Settings),
        ];
        for (what, icon) in drawn {
            let path = icon.path();
            assert!(
                installed.load(&path).is_ok(),
                "{what} draws {path}, which the app's asset source does not have"
            );
        }
    }

    /// An icon-bearing control is still a control: the id a test finds it by and
    /// the label a screen reader reads do not depend on what is drawn inside it.
    #[gpui_kit::test]
    fn an_icon_button_keeps_its_id_and_its_label(cx: &mut TestAppContext) {
        let fixture = open(cx, "icons-chrome");
        for (id, expected) in [
            ("repo-add", "Add repository"),
            ("panel-toggle", "Toggle right sidebar"),
            ("add-tab", "New tab"),
            ("panel-files", "Show Files"),
            ("panel-sessions", "Show Sessions"),
            ("panel-git", "Show Source Control"),
            ("tab-close-0", "Close tab"),
            ("settings-open", "Settings"),
        ] {
            assert_eq!(
                label(cx, &fixture, id).as_deref(),
                Some(expected),
                "the control behind {id} lost its accessible name"
            );
        }
    }

    // ------------------------------------------------------- the split panes

    /// The two panes, the divider between them — and **one** tab in the bar: a
    /// split is content under the tab it was opened in, not a tab of its own.
    #[gpui_kit::test]
    fn splitting_puts_two_panes_on_screen_under_the_same_tab(cx: &mut TestAppContext) {
        let fixture = open(cx, "split-open");
        click(cx, &fixture, tree_row(&fixture, NOTES));
        assert!(
            draw_until(cx, &fixture, |cx| file_tabs(&active_tabs(cx, &fixture))
                .len()
                == 1),
            "no editor tab opened"
        );
        assert!(
            !found(cx, &fixture, "pane-divider"),
            "one pane before the split"
        );
        let tabs_before = open_tabs(cx, &fixture);

        press(cx, &fixture, "cmd-d");
        draw(cx, &fixture);

        assert!(found(cx, &fixture, "pane-0"), "no first pane");
        assert!(found(cx, &fixture, "pane-1"), "no second pane");
        assert!(found(cx, &fixture, "pane-divider"), "no divider");
        // The editor you were looking at stays where it is and the new terminal
        // appears beside it, with the keyboard.
        assert_eq!(
            on_screen(cx, &fixture).len(),
            2,
            "the split did not put two panes on screen"
        );
        assert!(
            cx.update(|cx| fixture.workspace.read(cx).active_terminal(cx).is_some()),
            "the new half did not take the keyboard"
        );

        // The bar is where the change shows: a split adds a *pane*, so the tab
        // count is what it was — and the row is marked as split.
        assert_eq!(
            open_tabs(cx, &fixture),
            tabs_before,
            "the split opened a tab of its own"
        );
        assert!(
            found(cx, &fixture, "tab-onscreen-1"),
            "the split tab is not marked"
        );
    }

    /// The divider drags, and the two panes still add up to the pane area: a
    /// split whose halves grew with the drag would be a pane area that resized
    /// itself.
    #[gpui_kit::test]
    fn the_divider_moves_the_panes_without_changing_their_sum(cx: &mut TestAppContext) {
        let fixture = open(cx, "split-drag");
        click(cx, &fixture, tree_row(&fixture, NOTES));
        assert!(draw_until(cx, &fixture, |cx| file_tabs(&active_tabs(
            cx, &fixture
        ))
        .len()
            == 1));
        press(cx, &fixture, "cmd-d");
        draw(cx, &fixture);

        let leading = bounds_of(cx, &fixture, "pane-0");
        let trailing = bounds_of(cx, &fixture, "pane-1");
        let divider = bounds_of(cx, &fixture, "pane-divider");
        // Both halves fill the pane area before the drag: a trailing pane with
        // no height is the failure this assertion exists for.
        assert_eq!(
            trailing.size.height.as_f32(),
            leading.size.height.as_f32(),
            "the trailing pane does not fill the row's height: {trailing:?} beside {leading:?}"
        );
        let before = leading.size.width.as_f32() + trailing.size.width.as_f32();
        let leading_before = leading.size.width.as_f32();

        // Leftwards: the two panes each have a minimum, so in a pane area this
        // narrow the divider already sits as far right as it may — what a drag
        // can still do is give the second pane more room.
        let from = Point::new(divider.origin.x + px(3.), divider.origin.y + px(50.));
        let to = Point::new(from.x - px(80.), from.y);
        cx.update_window(fixture.window.into(), |_, window, cx| {
            window.drag(from, to, cx)
        })
        .expect("drag the divider");
        draw(cx, &fixture);

        let leading_after = bounds_of(cx, &fixture, "pane-0").size.width.as_f32();
        let after = leading_after + bounds_of(cx, &fixture, "pane-1").size.width.as_f32();
        assert!(
            leading_after < leading_before,
            "the drag did not narrow the first pane: {leading_before} -> {leading_after}"
        );
        assert!(
            (after - before).abs() < 1.,
            "the two panes no longer fill the pane area: {before} -> {after}"
        );
    }

    /// `cmd-w` closes the pane the keyboard is in — its terminal with it — and
    /// its sibling takes the whole area. Closing the pane is why nothing is left
    /// running off screen.
    #[gpui_kit::test]
    fn closing_the_pane_closes_its_terminal(cx: &mut TestAppContext) {
        let fixture = open(cx, "split-close");
        click(cx, &fixture, tree_row(&fixture, NOTES));
        assert!(draw_until(cx, &fixture, |cx| file_tabs(&active_tabs(
            cx, &fixture
        ))
        .len()
            == 1));
        press(cx, &fixture, "cmd-d");
        draw(cx, &fixture);
        assert_eq!(
            on_screen(cx, &fixture).len(),
            2,
            "the split did not put two panes on screen"
        );
        // The split put the keyboard in the new pane, so the terminal is what is
        // "active" until the pane goes away.
        assert!(cx.update(|cx| fixture.workspace.read(cx).active_terminal(cx).is_some()));

        press(cx, &fixture, "cmd-w");
        draw(cx, &fixture);

        assert!(
            !found(cx, &fixture, "pane-divider"),
            "the split is still there"
        );
        // The terminal the split opened is gone — pane and child both — and the
        // editor it was beside has the whole area. The worktree's own shell is
        // in its own tab and was never part of this.
        assert_eq!(
            terminal_tabs(cx, &fixture),
            1,
            "the pane's terminal outlived it"
        );
        assert_eq!(
            on_screen(cx, &fixture).len(),
            1,
            "the tab did not come back to one pane"
        );
        assert!(
            cx.update(|cx| fixture.workspace.read(cx).active_editor(cx).is_some()),
            "the editor did not come back to the whole area"
        );
    }

    /// The `×` in a split pane's corner closes *that* pane, whichever half has
    /// the keyboard — a pane you cannot point at and close is a pane people get
    /// stuck in.
    #[gpui_kit::test]
    fn the_pane_close_button_closes_that_pane(cx: &mut TestAppContext) {
        let fixture = open(cx, "split-close-button");
        press(cx, &fixture, "cmd-d");
        assert!(
            draw_until(cx, &fixture, |cx| terminal_tabs(cx, &fixture) == 2),
            "the split did not open a second terminal"
        );
        assert_eq!(on_screen(cx, &fixture).len(), 2);
        assert!(
            found(cx, &fixture, "pane-close-0") && found(cx, &fixture, "pane-close-1"),
            "a pane has no close button"
        );

        // The first pane, closed from the pointer while the *second* pane is
        // what has the keyboard: the click says which pane, not the focus.
        click_visible(cx, &fixture, "pane-close-0");
        assert!(
            draw_until(cx, &fixture, |cx| !found(cx, &fixture, "pane-divider")),
            "the button did not close the pane"
        );
        assert_eq!(
            terminal_tabs(cx, &fixture),
            1,
            "the pane's tab is still there"
        );
        assert_eq!(on_screen(cx, &fixture).len(), 1, "the sibling has the area");
    }

    /// Splitting a single tab opens a *second* terminal, and the two panes are two views: what is
    /// typed goes into the pane with the keyboard and nowhere else.
    #[gpui_kit::test]
    fn splitting_a_single_tab_opens_a_second_terminal(cx: &mut TestAppContext) {
        let fixture = open(cx, "split-second-terminal");
        assert_eq!(terminal_tabs(cx, &fixture), 1);

        press(cx, &fixture, "cmd-d");
        assert!(
            draw_until(cx, &fixture, |cx| terminal_tabs(cx, &fixture) == 2),
            "the split did not open a second terminal"
        );
        // The pane that was there stays where it was; the new one is beside it,
        // with the keyboard, and it is a terminal of its own.
        assert_eq!(on_screen(cx, &fixture).len(), 2);
        let first = cx
            .update(|cx| fixture.workspace.read(cx).terminal_at(0, cx))
            .expect("the terminal that was already there");
        let second = cx
            .update(|cx| fixture.workspace.read(cx).terminal_at(1, cx))
            .expect("the terminal the split opened");
        assert_ne!(
            first.entity_id(),
            second.entity_id(),
            "both halves are the same terminal, which is the bug"
        );

        // Through the *text input* path (`window.input`, which is what an IME
        // commits through) and then the shell's own echo: only the pane with the
        // keyboard may have it.
        type_text(
            cx,
            &fixture,
            "echo SPLIT_ONLY
",
        );
        assert!(
            draw_until(cx, &fixture, |cx| cx.update(|cx| second
                .read(cx)
                .screen()
                .text()
                .contains("SPLIT_ONLY"))),
            "the pane with the keyboard never got the text"
        );
        assert!(
            !cx.update(|cx| first.read(cx).screen().text().contains("SPLIT_ONLY")),
            "the other pane got the same input"
        );

        // And the keyboard follows the pointer: a click in the other pane takes
        // it there, and what is typed next goes there and nowhere else.
        click_visible(cx, &fixture, "pane-0");
        assert!(
            draw_until(cx, &fixture, |cx| cx.update(|cx| fixture
                .workspace
                .read(cx)
                .active_terminal(cx)
                .is_some_and(|now| now.entity_id() == first.entity_id()))),
            "the click did not move the keyboard to the pane it landed in"
        );
        type_text(cx, &fixture, "echo LEADING_ONLY\n");
        assert!(
            draw_until(cx, &fixture, |cx| cx.update(|cx| first
                .read(cx)
                .screen()
                .text()
                .contains("LEADING_ONLY"))),
            "the pane the pointer went into never got the text"
        );
        assert!(
            !cx.update(|cx| second.read(cx).screen().text().contains("LEADING_ONLY")),
            "the text went into both panes again"
        );
    }

    /// A tab opened while split lands in the pane with the keyboard, and the
    /// panes beside it keep what they had.
    #[gpui_kit::test]
    fn a_file_opened_while_split_is_a_tab_of_its_own(cx: &mut TestAppContext) {
        let fixture = open(cx, "split-focus");
        click(cx, &fixture, tree_row(&fixture, NOTES));
        assert!(draw_until(cx, &fixture, |cx| file_tabs(&active_tabs(
            cx, &fixture
        ))
        .len()
            == 1));
        // Split: a new terminal opens beside the focused pane, with the keyboard.
        press(cx, &fixture, "cmd-d");
        draw(cx, &fixture);
        assert_eq!(on_screen(cx, &fixture).len(), 2);

        // A second file, opened while the split is on screen: a file is a tab of
        // its own — a split divides the pane it is in, and a file from the tree
        // is not that — so the new tab is what comes up.
        click(cx, &fixture, tree_row(&fixture, "src"));
        draw(cx, &fixture);
        click(cx, &fixture, row(&fixture.root.join("src").join("main.rs")));
        assert!(
            draw_until(cx, &fixture, |cx| file_tabs(&active_tabs(cx, &fixture))
                .len()
                == 2),
            "the second file did not open"
        );
        assert_eq!(
            on_screen(cx, &fixture).len(),
            1,
            "the file's tab is one pane"
        );
        assert_eq!(
            open_tabs(cx, &fixture),
            3,
            "the file did not open a tab of its own"
        );

        // And the split is still where it was: back to the tab it is in — the
        // editor's, which is the one `cmd-d` divided — and both panes are there,
        // which is what a tab owning its panes means.
        click_visible(cx, &fixture, "tab-1");
        assert!(
            draw_until(cx, &fixture, |cx| on_screen(cx, &fixture).len() == 2),
            "the split did not come back with its tab"
        );
        assert!(
            found(cx, &fixture, "pane-0") && found(cx, &fixture, "pane-1"),
            "a pane went away"
        );
    }

    /// Stacked is the other axis, and a second split divides the pane that has
    /// the keyboard instead of turning the first one — which is how a worktree
    /// gets a third pane.
    #[gpui_kit::test]
    fn splitting_again_divides_the_pane_with_the_keyboard(cx: &mut TestAppContext) {
        let fixture = open(cx, "split-third");
        click(cx, &fixture, tree_row(&fixture, NOTES));
        assert!(draw_until(cx, &fixture, |cx| file_tabs(&active_tabs(
            cx, &fixture
        ))
        .len()
            == 1));

        press(cx, &fixture, "cmd-d");
        draw(cx, &fixture);
        assert_eq!(on_screen(cx, &fixture).len(), 2);
        let whole = bounds_of(cx, &fixture, "pane-0");
        assert!(
            whole.size.height.as_f32() > whole.size.width.as_f32(),
            "side by side should leave a pane taller than it is wide"
        );

        // The keyboard is in the pane the split just made — the terminal — so
        // `cmd-shift-d` divides *that* one: two panes stacked in the right half,
        // and the editor on the left still filling its half alone.
        press(cx, &fixture, "cmd-shift-d");
        assert!(
            draw_until(cx, &fixture, |cx| found(cx, &fixture, "pane-1-1")),
            "the second split did not make a third pane"
        );
        assert_eq!(
            on_screen(cx, &fixture).len(),
            3,
            "the second split did not make a third pane"
        );
        assert!(
            !found(cx, &fixture, "pane-0-0"),
            "the split divided the wrong pane"
        );
        let top = bounds_of(cx, &fixture, "pane-1-0");
        let bottom = bounds_of(cx, &fixture, "pane-1-1");
        assert_eq!(
            top.origin.x, bottom.origin.x,
            "the stacked panes are not aligned"
        );
        assert_eq!(top.size.width, bottom.size.width);
        assert_eq!(
            top.size.height + bottom.size.height + px(split::DIVIDER),
            whole.size.height,
            "the two stacked panes do not fill the half they are in"
        );
    }

    // --------------------------------------------------------- the settings

    /// The readout is one icon and the app's own cost on the strip, and what a
    /// worktree's terminals cost is on the sidebar's rows: the worktree's own
    /// line, and one line per terminal under it. From a real sweep of the host
    /// process table: the numbers come from the kernel, so this test says what
    /// the wiring does, not what a fixture said.
    #[gpui_kit::test]
    fn the_resource_bar_counts_the_app_alone(cx: &mut TestAppContext) {
        let fixture = open(cx, "resources");
        assert!(
            draw_until(cx, &fixture, |cx| cx.update(|cx| fixture
                .workspace
                .read(cx)
                .active_terminal(cx)
                .is_some())),
            "no terminal to measure"
        );

        let summary = label(cx, &fixture, "resource-summary").expect("the readout");
        assert!(summary.starts_with("Resources: "), "{summary}");
        assert!(
            !summary.contains("measuring") && !summary.contains("unavailable"),
            "the sweep did not answer: {summary}"
        );
        // The app's own number: one process, and not its terminals' trees, which
        // are counted on the worktree each terminal belongs to.
        assert!(
            summary.contains("MB") || summary.contains("GB"),
            "{summary}"
        );
        // Memory alone: the CPU share is not on the strip.
        assert!(!summary.contains('%'), "{summary}");

        // And a click on it opens nothing: what it used to open is on the
        // sidebar's rows now.
        click_visible(cx, &fixture, "resource-summary");
        assert!(
            !found(cx, &fixture, "resource-panel"),
            "the strip still opens a list"
        );

        // The worktree on screen carries what its terminals cost, on the rows it
        // is: the numbers are the readout's, and this is where they are read.
        let (worktree_id, branch) = cx.update(|cx| {
            let worktree = fixture
                .state
                .read(cx)
                .active()
                .expect("something is active");
            (worktree.id.clone(), branch_or_name(worktree).to_string())
        });
        assert!(
            found(cx, &fixture, &format!("worktree-{worktree_id}")),
            "no row for {branch}"
        );
        // The worktree's line is the branch alone: each terminal says what it
        // costs on its own line, and the worktree never repeats or totals it.
        let title = {
            assert!(
                draw_until(cx, &fixture, |cx| !readout_rows(cx, &fixture, &worktree_id)
                    .is_empty()),
                "the worktree never got a terminal row"
            );
            readout_rows(cx, &fixture, &worktree_id)
                .first()
                .cloned()
                .expect("a terminal row")
        };
        let row_id = format!("terminal-{worktree_id}-{title}");
        assert!(
            draw_until(cx, &fixture, |cx| found(cx, &fixture, &row_id)),
            "the row never got the sweep's numbers"
        );
        let metrics = label(
            cx,
            &fixture,
            &format!("terminal-metrics-{worktree_id}-{title}"),
        )
        .expect("the terminal's numbers");
        assert!(
            metrics.contains('%') && (metrics.contains("MB") || metrics.contains("GB")),
            "the terminal's numbers are not a cost: {metrics}"
        );
        assert!(
            !found(cx, &fixture, &format!("worktree-metrics-{worktree_id}")),
            "one terminal's numbers are on the worktree's line as well"
        );

        // The columns those numbers sit in are named once, on the worktree's own
        // line — the header its rows hang under.
        let columns = format!("terminal-columns-{worktree_id}");
        assert!(found(cx, &fixture, &columns), "the columns are not named");
        let header = bounds_of(cx, &fixture, &columns);
        let line = bounds_of(cx, &fixture, &row_id);
        assert!(
            header.origin.y + header.size.height <= line.origin.y,
            "the names are not above the row they head: {header:?}, {line:?}"
        );
        // A label sits over the column it names: the two numbers are the last of
        // the header's columns, so the RSS label's right edge is where the row's
        // is and the whole group closes both. The CPU label is the first of the
        // numbers, so its left edge is the row's first number's too, and the
        // length sits beside the word, ahead of them.
        let numbers = bounds_of(
            cx,
            &fixture,
            &format!("terminal-metrics-{worktree_id}-{title}"),
        );
        let cpu = bounds_of(cx, &fixture, &format!("terminal-columns-cpu-{worktree_id}"));
        let mut end = |id: &str| {
            let at = bounds_of(cx, &fixture, id);
            at.origin.x + at.size.width
        };
        assert_eq!(
            cpu.origin.x, numbers.origin.x,
            "the CPU label is not over the CPU column"
        );
        assert_eq!(
            end(&format!("terminal-columns-mem-{worktree_id}")),
            numbers.origin.x + numbers.size.width,
            "the RSS label is not over the RSS column"
        );
        assert_eq!(
            end(&format!("terminal-columns-uptime-{worktree_id}")),
            end(&format!("terminal-uptime-{worktree_id}-{title}")),
            "the length label is not over the length column"
        );
        assert_eq!(
            end(&columns),
            numbers.origin.x + numbers.size.width,
            "the row's numbers do not close the columns"
        );

        // A second terminal still draws no total: each has its own line.
        click_visible(cx, &fixture, "add-tab");
        assert!(
            draw_until(cx, &fixture, |cx| found(cx, &fixture, "add-menu")),
            "the + did not open its menu"
        );
        click_visible(cx, &fixture, "add-menu-terminal");
        assert!(
            draw_until(cx, &fixture, |cx| terminal_tabs(cx, &fixture) == 2),
            "the menu did not open a second terminal"
        );
        assert!(
            draw_until(cx, &fixture, |cx| readout_rows(cx, &fixture, &worktree_id)
                .len()
                == 2),
            "the second terminal never got a row"
        );
        assert!(
            !found(cx, &fixture, &format!("worktree-metrics-{worktree_id}")),
            "the worktree's line carries a total"
        );
        for title in readout_rows(cx, &fixture, &worktree_id) {
            assert!(
                found(cx, &fixture, &format!("terminal-{worktree_id}-{title}")),
                "no row for {title}"
            );
        }
    }

    /// The status bar's leading end holds the way into the settings form: one
    /// icon, one dialog, and every setting is in it.
    #[gpui_kit::test]
    fn the_status_bar_settings_button_opens_the_settings_form(cx: &mut TestAppContext) {
        let fixture = open(cx, "settings-open-button");
        assert!(!found(cx, &fixture, "settings"), "the form was already up");

        click_visible(cx, &fixture, "settings-open");
        assert!(
            draw_until(cx, &fixture, |cx| found(cx, &fixture, "settings")),
            "the status bar's settings button opened nothing"
        );

        // In the bottom bar's leading end, which is where it was asked for: the
        // bar is the window's last row, and the button is at its left.
        let bar = bounds_of(cx, &fixture, "resource-bar");
        let button = bounds_of(cx, &fixture, "settings-open");
        assert!(
            button.origin.y >= bar.origin.y && button.origin.y < bar.origin.y + bar.size.height,
            "the settings button is not on the status bar: {button:?}, {bar:?}"
        );
        assert!(
            button.origin.x < bar.origin.x + px(32.),
            "the settings button is not at the status bar's leading end: {button:?}"
        );
    }

    /// The strip's usage chip opens the panel: what the agents have spent lately, as
    /// charts over the window rather than as rows squeezed into a popover.
    #[gpui_kit::test]
    fn the_status_bar_usage_chip_opens_the_panel(cx: &mut TestAppContext) {
        let fixture = open(cx, "usage-chip");
        assert!(
            !found(cx, &fixture, "usage-panel"),
            "the panel was up already"
        );

        // A sweep's answer without a sweep: a test's home has no transcripts to
        // fold, and what the panel draws is what the strip holds.
        let slice = |model: &str, tokens: u64, cost: f64| UsageSlice {
            model: model.to_string(),
            tokens,
            cost,
            estimated: false,
            unpriced: false,
        };
        let model = |label: &str, tokens: u64, cost: f64| UsageGroup {
            label: label.to_string(),
            tokens,
            cost,
            estimated: false,
            unpriced: false,
        };
        let usage = UsageSummary {
            total: Usage {
                tokens: 2_500_000_000,
                cost: 1824.49,
                ..Usage::default()
            },
            today: Usage {
                tokens: 40_000_000,
                cost: 4.46,
                ..Usage::default()
            },
            days: vec![
                UsageDay {
                    label: "10-13".to_string(),
                    tokens: 731_000_000,
                    cost: 677.44,
                    // Two models in one column: the stack the panel draws.
                    models: vec![
                        slice("claude-opus-5-5", 600_000_000, 600.),
                        slice("gpt-6.1-sol", 131_000_000, 77.44),
                    ],
                },
                UsageDay {
                    label: "10-14".to_string(),
                    tokens: 423_000_000,
                    cost: 343.57,
                    models: vec![slice("claude-opus-5-5", 423_000_000, 343.57)],
                },
            ],
            providers: vec![UsageGroup {
                label: "iotex".to_string(),
                tokens: 1_564_000_000,
                cost: 12.64,
                estimated: false,
                unpriced: false,
            }],
            // Ten of them, which is what the panel has to hold: the list runs past the
            // bottom of the strip it is drawn in, and the body scrolls to it.
            models: vec![
                model("claude-opus-5-5", 1_564_000_000, 12.64),
                model("deepseek-v4.1-flash", 900_000_000, 8.20),
                model("gpt-6.1-sol", 400_000_000, 3.40),
                model("deepseek-v4-pro", 200_000_000, 1.60),
                model("kimi-k3", 90_000_000, 0.61),
                // A long name, which is the widest thing the legend has to hold.
                model("gpt-6-sol", 40_000_000, 0.30),
                model("gpt-5.6-sol", 30_000_000, 0.20),
                model("grok-4.6", 20_000_000, 0.10),
                model("glm-5.3", 10_000_000, 0.05),
                model("claude-sonnet-5-5", 5_000_000, 0.02),
            ],
        };
        let bar = cx.update(|cx| fixture.workspace.read(cx).resource_bar().clone());
        bar.update(cx, |bar, cx| {
            bar.set_usage(usage.clone());
            cx.notify();
        });
        // The chip's label is the sentence a screen reader reads; the compact
        // numbers on it are the unit test's.
        assert_eq!(
            label(cx, &fixture, "usage-open").as_deref(),
            Some("Usage: 2.5B tokens and $1824.49 over the last 14 days"),
            "the chip does not read the numbers the sweep left"
        );

        click_visible(cx, &fixture, "usage-open");
        assert!(
            draw_until(cx, &fixture, |cx| found(cx, &fixture, "usage-panel")),
            "the usage chip opened nothing"
        );
        // Both cuts of it: the days as bars, and the providers as a ring with the
        // legend that names them.
        assert!(found(cx, &fixture, "usage-days-box"), "no day chart");
        assert!(found(cx, &fixture, "usage-share-box"), "no share ring");

        // The panel is a strip down the window's trailing side: narrow, clear of the strip
        // it reports on — and its body scrolls, because the list of models can be longer
        // than any window. It is only as tall as the list makes it.
        let panel = bounds_of(cx, &fixture, "usage-panel");
        let strip = bounds_of(cx, &fixture, "resource-bar");
        // The chip that opened it sits at the bar's trailing end, over the panel.
        let chip = bounds_of(cx, &fixture, "usage-open");
        assert!(
            strip.origin.x + strip.size.width - (chip.origin.x + chip.size.width) < px(32.),
            "the usage chip is not at the status bar's trailing end: {chip:?}, {strip:?}"
        );
        let window_bounds = bounds_of(cx, &fixture, "usage-scrim");
        let trailing =
            window_bounds.origin.x + window_bounds.size.width - (panel.origin.x + panel.size.width);
        // Off the trailing edge, but only just: the panel is placed where the
        // red line was drawn, not against the window's own corner.
        assert!(
            trailing > px(8.) && trailing < px(32.),
            "the panel is not just off the trailing edge: {panel:?}, {window_bounds:?}"
        );
        assert!(
            panel.size.width <= px(560.),
            "the panel is not a narrow strip: {panel:?}"
        );
        // Its foot hangs just over the strip — near it, clear of it.
        let clearance = strip.origin.y - (panel.origin.y + panel.size.height);
        assert!(
            clearance >= px(40.) && clearance <= px(80.),
            "the panel's clearance over the strip is {clearance:?}: {panel:?}, {strip:?}"
        );
        assert!(
            panel.origin.y + panel.size.height <= strip.origin.y,
            "the panel runs into the strip: {panel:?}, {strip:?}"
        );
        assert!(
            !found(cx, &fixture, "usage-tail"),
            "the panel still has a tail"
        );
        // No header row: the card is charts and the list, and the scrim behind it and
        // escape are what close it.
        assert!(
            !found(cx, &fixture, "usage-title") && !found(cx, &fixture, "usage-close"),
            "the card still has a header"
        );
        // The box the scroller was given, and the content inside it: the content being
        // taller is what the scrollbar is for, and a wheel over it has to move the list
        // rather than stop after a few pixels — which is what an un-sized scroller does.
        let viewport = bounds_of(cx, &fixture, "usage-viewport");
        let body = bounds_of(cx, &fixture, "usage-body");
        assert!(
            panel.right() - viewport.right() <= px(2.),
            "the scrollbar viewport is inset from the right edge: {viewport:?}, {panel:?}"
        );
        assert!(
            viewport.origin.y + viewport.size.height <= panel.origin.y + panel.size.height,
            "the scroller is not inside the panel: {viewport:?}, {panel:?}"
        );
        assert!(
            body.size.height > viewport.size.height,
            "the model list fits the viewport: nothing to scroll — {body:?}, {viewport:?}"
        );
        let legend_before = bounds_of(cx, &fixture, "usage-legend").origin.y;
        cx.update_window(fixture.window.into(), |_, window, cx| {
            window.scroll(
                "usage-body",
                ScrollDelta::Lines(Point { x: 0., y: -400. }),
                cx,
            );
        })
        .unwrap();
        draw(cx, &fixture);
        let legend_after = bounds_of(cx, &fixture, "usage-legend").origin.y;
        assert!(
            legend_before - legend_after > px(100.),
            "the wheel barely moved the list: {legend_before:?} to {legend_after:?}"
        );
        // Back to the top, so the rest of this test reads the list as it opens.
        cx.update_window(fixture.window.into(), |_, window, cx| {
            window.scroll(
                "usage-body",
                ScrollDelta::Lines(Point { x: 0., y: 400. }),
                cx,
            );
        })
        .unwrap();
        draw(cx, &fixture);

        // And the legend keeps to its own row: a list longer than the room it was
        // given scrolls inside it rather than being drawn over the days chart, which
        // is what happened when it outgrew the row it was in. Its first row — and the
        // heading above it — start below the chart.
        let days = bounds_of(cx, &fixture, "usage-days-box");
        let share = bounds_of(cx, &fixture, "usage-share-box");
        let first = bounds_of(cx, &fixture, "usage-legend-claude-opus-5-5");
        for (what, top) in [
            ("the share row", share.origin.y),
            ("the legend", first.origin.y),
        ] {
            assert!(
                top >= days.origin.y + days.size.height,
                "{what} runs into the days chart: {top:?}, {days:?}"
            );
        }
        assert!(
            share.origin.y >= days.bottom() + px(12.),
            "the charts are cramped"
        );
        // Names shrink before the fixed numeric columns; the full value stays accessible.
        let row = bounds_of(cx, &fixture, "usage-legend-deepseek-v4.1-flash");
        assert!(
            (row.size.width - days.size.width).as_f32().abs() < 1.,
            "the legend does not align with the chart: {row:?}, {days:?}"
        );
        let name: gpui_kit::SharedString = "deepseek-v4.1-flash".into();
        let whole = cx
            .update_window(fixture.window.into(), move |_, window, _| {
                gpui_kit::base::plot::label::measure_text_width(
                    &name,
                    window.rem_size() * 0.75,
                    window,
                )
            })
            .expect("measure the name");
        assert!(
            row.size.width >= px(whole),
            "the legend cut its longest name: {row:?}, which needs {whole:?}"
        );
        assert!(row.size.height < px(24.), "the legend row wrapped: {row:?}");
        let model = bounds_of(cx, &fixture, "usage-model-deepseek-v4.1-flash");
        assert!(
            model.size.width > px(0.) && model.right() < row.right() - px(140.),
            "the model name crowds the numbers: {model:?}, {row:?}"
        );
        assert!(
            label(cx, &fixture, "usage-legend-deepseek-v4.1-flash")
                .is_some_and(|label| label.starts_with("deepseek-v4.1-flash:"))
        );

        // Escape closes it, and the chip opens it again: the strip's own flag has to
        // agree with the panel, or the chip stops answering.
        press(cx, &fixture, "escape");
        assert!(
            draw_until(cx, &fixture, |cx| !found(cx, &fixture, "usage-panel")),
            "escape left the panel up"
        );
        click_visible(cx, &fixture, "usage-open");
        assert!(
            draw_until(cx, &fixture, |cx| found(cx, &fixture, "usage-panel")),
            "the chip did not open the panel a second time"
        );

        // A chart defers its tooltip so the box can overflow the plot, and this panel
        // is drawn deferred too: a panel painted at a raised priority would come after
        // the tooltip and hide it, which is exactly what a hover that shows nothing
        // looks like. Nothing else asserts on it, so: hover, and check that something
        // painted over the panel's own surface.
        let quads = |cx: &mut TestAppContext| {
            cx.update_window(fixture.window.into(), |_, window, _| window.painted_quads())
                .unwrap()
        };
        let hovered_at = cx
            .update_window(fixture.window.into(), |_, window, _| window.scale_factor())
            .unwrap();
        // Past the chart's draw-in: a chart withholds its tooltip while its data is
        // still appearing.
        cx.background_executor
            .advance_clock(std::time::Duration::from_millis(1500));
        draw(cx, &fixture);
        let before = quads(cx);
        cx.update_window(fixture.window.into(), |_, window, cx| {
            window.hover("usage-days-box", cx);
        })
        .unwrap();
        // Twice: the hover state the first frame records is what the second paints.
        draw(cx, &fixture);
        draw(cx, &fixture);
        let after = quads(cx);
        let panel = bounds_of(cx, &fixture, "usage-panel");
        let (panel_x, panel_y) = (
            panel.origin.x.as_f32() * hovered_at,
            panel.origin.y.as_f32() * hovered_at,
        );
        let last_panel = after
            .iter()
            .enumerate()
            .filter(|(_, quad)| {
                quad.bounds.origin.x.as_f32() == panel_x && quad.bounds.origin.y.as_f32() == panel_y
            })
            .map(|(index, _)| index)
            .next_back()
            .expect("the panel is painted");
        // The band, the dot and the tooltip box: whatever the hover draws, it has to be
        // painted after the panel's own surface.
        assert!(
            after
                .iter()
                .enumerate()
                .any(|(index, quad)| index > last_panel
                    && !before.iter().any(|old| old.bounds == quad.bounds)),
            "the hover painted nothing over the panel: the tooltip is behind the card"
        );

        // A click on the bar behind it dismisses it — without reaching the bar,
        // so the settings form does not open as well. (Not the window's top edge,
        // which is the title bar's drag zone and ignores the press.)
        click(cx, &fixture, "resource-bar".to_string());
        assert!(
            draw_until(cx, &fixture, |cx| !found(cx, &fixture, "usage-panel")),
            "a click on the window behind left the panel up"
        );
        assert!(
            !found(cx, &fixture, "settings"),
            "the click went past the panel"
        );
    }

    #[gpui_kit::test]
    fn the_usage_panel_marks_official_estimates_and_unknown_prices(cx: &mut TestAppContext) {
        let fixture = open(cx, "usage-estimate");
        let usage = UsageSummary {
            total: Usage {
                tokens: 1_000,
                cost: 1.25,
                estimated: true,
                unpriced: true,
            },
            days: vec![UsageDay {
                label: "10-16".into(),
                tokens: 1_000,
                cost: 1.25,
                models: vec![],
            }],
            models: vec![
                UsageGroup {
                    label: "gpt-6.1-sol".into(),
                    tokens: 900,
                    cost: 1.25,
                    estimated: true,
                    unpriced: false,
                },
                UsageGroup {
                    label: "gpt-unknown".into(),
                    tokens: 100,
                    cost: 0.,
                    estimated: false,
                    unpriced: true,
                },
            ],
            ..UsageSummary::default()
        };
        let bar = cx.update(|cx| fixture.workspace.read(cx).resource_bar().clone());
        bar.update(cx, |bar, cx| {
            bar.set_usage(usage);
            cx.notify();
        });
        assert_eq!(
            label(cx, &fixture, "usage-open").unwrap(),
            "Usage: 1.1k tokens and ~$1.25+ over the last 14 days"
        );
        click_visible(cx, &fixture, "usage-open");
        assert!(draw_until(cx, &fixture, |cx| found(
            cx,
            &fixture,
            "usage-legend-gpt-6.1-sol"
        )));
        assert!(
            label(cx, &fixture, "usage-legend-gpt-6.1-sol")
                .unwrap()
                .contains("~$1.25")
        );
        assert!(
            label(cx, &fixture, "usage-legend-gpt-unknown")
                .unwrap()
                .contains("Unknown")
        );
    }

    /// The panel opens before the first sweep has answered, too: a window that has
    /// not read anything yet says so rather than drawing an empty chart.
    #[gpui_kit::test]
    fn the_usage_panel_says_it_is_measuring_before_the_first_sweep(cx: &mut TestAppContext) {
        let fixture = open(cx, "usage-measuring");
        assert_eq!(
            label(cx, &fixture, "usage-open").as_deref(),
            Some("Usage: —"),
            "the chip invented numbers before the first sweep"
        );

        click_visible(cx, &fixture, "usage-open");
        assert!(
            draw_until(cx, &fixture, |cx| found(cx, &fixture, "usage-panel")),
            "the usage chip opened nothing"
        );
        assert!(
            !found(cx, &fixture, "usage-days-box"),
            "the panel charted usage it had not read"
        );
    }

    /// The form's font size reaches the theme, and the theme is what the
    /// terminal measures its cells from.
    #[gpui_kit::test]
    fn the_font_size_setting_reaches_the_terminal_grid(cx: &mut TestAppContext) {
        let fixture = open(cx, "settings-font");
        assert!(
            draw_until(cx, &fixture, |cx| cx.update(|cx| fixture
                .workspace
                .read(cx)
                .active_terminal(cx)
                .is_some())),
            "no terminal to measure"
        );
        let rows_before = terminal_rows(cx, &fixture);

        open_settings_tab(cx, &fixture, "settings-tab-terminal");
        // Clicked until it moves rather than a fixed number of times: the harness dispatches a
        // synthetic pointer event, and one of those is occasionally dropped — which an exact
        // count turns into a failure about arithmetic instead of about the wiring.
        assert!(
            click_until(cx, &fixture, "settings-font-plus".to_string(), |cx| cx
                .update(
                    |cx| fixture.state.read(cx).settings().terminal.font_size > 13.
                )),
            "the form's change did not reach the state"
        );
        let chosen = cx.update(|cx| fixture.state.read(cx).settings().terminal.font_size);
        assert!(chosen > 13., "{chosen}");

        // The theme the terminal measures from, size and line height together:
        // a size without its line height is rows that overlap.
        let (size, line_height) = cx.update(|cx| {
            let mono = gpui_kit::base::Theme::global(cx).tokens.typography.mono_md;
            (mono.size, mono.line_height)
        });
        assert_eq!(size, px(chosen));
        assert!(
            line_height.as_f32() > 20.,
            "the line height stayed at the old size's: {line_height:?}"
        );

        // A terminal measures its cells when it is built, which is what the form
        // says it does, so the grid that follows the new type is the next one.
        restart_terminal(cx, &fixture);
        assert!(
            draw_until(cx, &fixture, |cx| terminal_rows(cx, &fixture) < rows_before),
            "the terminal's grid did not follow the font size: {rows_before} rows before, {} after",
            terminal_rows(cx, &fixture)
        );
    }

    /// The appearance is a dropdown, and picking a palette lands as the theme.
    #[gpui_kit::test]
    fn the_appearance_setting_changes_the_theme(cx: &mut TestAppContext) {
        let fixture = open(cx, "settings-appearance");
        let before = cx.update(|cx| gpui_kit::base::Theme::global(cx).appearance);
        open_settings_tab(cx, &fixture, "settings-tab-appearance");

        // Pick the one it is not, so the assertion is about the pick rather than
        // about the start-up value. The picker opens on "follow the system", so
        // Light is the next row and Dark the one after it.
        let (wanted, rows) = if before == gpui_kit::base::ThemeAppearance::Dark {
            (gpui_kit::base::ThemeAppearance::Light, 1)
        } else {
            (gpui_kit::base::ThemeAppearance::Dark, 2)
        };

        click_visible(cx, &fixture, "settings-appearance-select");
        draw(cx, &fixture);
        for _ in 0..rows {
            press(cx, &fixture, "down");
        }
        press(cx, &fixture, "enter");

        assert!(
            draw_until(cx, &fixture, move |cx| cx.update(|cx| {
                gpui_kit::base::Theme::global(cx).appearance == wanted
            })),
            "the appearance did not change"
        );
        let after = cx.update(|cx| gpui_kit::base::Theme::global(cx).appearance);
        assert_ne!(before, after, "the appearance did not change");
        assert_eq!(after, wanted);
    }

    /// The blur is a slider on the same tab, and it lands in the state and in the
    /// file the state is written to — the radius the window server is given is not
    /// readable from a test, so the pure function that picks the background is
    /// tested where it is defined.
    #[gpui_kit::test]
    fn the_blur_setting_reaches_the_state(cx: &mut TestAppContext) {
        let fixture = open(cx, "settings-blur");
        open_settings_tab(cx, &fixture, "settings-tab-appearance");

        assert!(
            found(cx, &fixture, "settings-blur"),
            "the appearance tab has no blur row"
        );
        // The blur only does anything through a see-through window, and its
        // slider is off until there is one.
        let state = fixture.state.clone();
        let see_through = (0..3).any(|_| {
            click_at_fraction(cx, &fixture, "settings-opacity-slider", 0.5);
            draw(cx, &fixture);
            draw_until(cx, &fixture, |cx| {
                cx.update(|cx| state.read(cx).settings().background_opacity < 1.0)
            })
        });
        assert!(see_through, "the window could not be made see-through");

        let slide_to = |cx: &mut TestAppContext, fraction: f32, wanted: u32| {
            let state = fixture.state.clone();
            (0..3).any(|_| {
                click_at_fraction(cx, &fixture, "settings-blur-slider", fraction);
                draw(cx, &fixture);
                draw_until(cx, &fixture, |cx| {
                    cx.update(|cx| state.read(cx).settings().background_blur == wanted)
                })
            })
        };
        assert!(
            slide_to(cx, 1., crate::settings_view::MAX_BLUR),
            "the slider's far end did not reach the state"
        );
        let written = std::fs::read_to_string(&fixture.settings_path).unwrap();
        assert!(
            written.contains(&format!(
                "background_blur = {}",
                crate::settings_view::MAX_BLUR
            )),
            "the blur never reached the file: {written}"
        );
        // And back to none, because a slider that only goes one way is not one.
        assert!(slide_to(cx, 0., 0), "the blur could not be turned off");
    }

    #[gpui_kit::test]
    fn transparency_is_calibrated_to_the_painted_background_layers(cx: &mut TestAppContext) {
        let fixture = open(cx, "opacity-layers");
        for percent in [10_f32, 25., 50., 75., 90.] {
            let opacity = 1. - (percent / 100.).sqrt();
            cx.update(|cx| crate::settings_view::apply_opacity(cx, opacity, 0));
            draw(cx, &fixture);
            let pane = bounds_of(cx, &fixture, "workspace-body");
            let color = cx.update(|cx| gpui_kit::base::Theme::global(cx).tokens.colors.background);
            let layers = cx
                .update_window(fixture.window.into(), |_, window, _| {
                    let center = pane.center().scale(window.scale_factor());
                    window
                        .painted_quads()
                        .iter()
                        .filter(|quad| {
                            quad.bounds.contains(&center)
                                && quad.background == gpui_kit::solid_background(color)
                        })
                        .count()
                })
                .unwrap();
            assert_eq!(
                layers, 2,
                "extra background fills distort the slider at {percent}%"
            );
            let actual = (1. - opacity).powi(layers as i32) * 100.;
            assert!(
                (actual - percent).abs() < 0.01,
                "{percent}% on the slider only passes {actual}% of the desktop"
            );
        }
    }

    /// The transparency is a slider, and it reaches the fills rather than stopping at the state: each
    /// one is drawn at the setting's alpha, which is what lets the desktop through the whole UI. The
    /// text is not one of them: a faded glyph is what this is for.
    #[gpui_kit::test]
    fn the_transparency_slider_reaches_the_window(cx: &mut TestAppContext) {
        let fixture = open(cx, "settings-opacity");
        open_settings_tab(cx, &fixture, "settings-tab-appearance");
        let state = fixture.state.clone();

        // The far end of the track is the most transparent a window goes.
        let moved = |cx: &mut TestAppContext| {
            cx.update(|cx| state.read(cx).settings().background_opacity < 1.0)
        };
        let mut landed = false;
        for _ in 0..3 {
            click_at_fraction(cx, &fixture, "settings-opacity-slider", 1.);
            draw(cx, &fixture);
            if draw_until(cx, &fixture, moved) {
                landed = true;
                break;
            }
        }
        assert!(
            landed,
            "the slider's change did not reach the state: {}",
            cx.update(|cx| fixture.state.read(cx).settings().background_opacity)
        );

        let chosen = cx.update(|cx| fixture.state.read(cx).settings().background_opacity);
        assert!(
            (MIN_OPACITY..MIN_OPACITY + 0.1).contains(&chosen),
            "the far end of the slider is not the most transparent a window gets: {chosen}"
        );
        let colors = cx.update(|cx| gpui_kit::base::Theme::global(cx).tokens.colors);
        assert_eq!(
            colors.background.a, chosen,
            "the window's background is not drawn at the opacity the setting asks for"
        );
        assert_eq!(colors.surface.a, chosen, "a panel is not drawn at it");
        // A line is drawn on a panel rather than beside it, so it is a layer
        // lighter than the panel is — see
        // `settings_view::a_box_on_a_panel_is_a_layer_lighter`.
        assert_eq!(
            colors.border.a,
            chosen * chosen,
            "a line is not drawn a layer lighter than the panel it is on"
        );
        assert_eq!(
            colors.foreground.a, 1.0,
            "the text is drawn at the window's opacity"
        );
        // The editor's surface is the highlight theme's, not the palette's: opaque in
        // the theme file, and the one pane whose background would stay solid.
        let component = cx.update(|cx| gpui_kit::component::Theme::global(cx).clone());
        assert_eq!(
            component
                .highlight_theme
                .style
                .editor_background
                .map(|fill| fill.a),
            Some(chosen),
            "the file editor's background is not drawn at the window's opacity"
        );
        // The gutter's own fill goes with it: it is a second fill of that same
        // background, a column of the window a layer denser than the code beside it.
        assert_eq!(
            component
                .highlight_theme
                .style
                .editor_gutter_background
                .map(|fill| fill.a),
            Some(0.),
            "the line-number gutter is drawn a layer denser than the code"
        );

        // Back to solid: the thumb's other end, which is a value the form can
        // reach rather than a floor it can only fall to.
        let state = fixture.state.clone();
        let mut back = false;
        for _ in 0..3 {
            click_at_fraction(cx, &fixture, "settings-opacity-slider", 0.);
            draw(cx, &fixture);
            if draw_until(cx, &fixture, |cx| {
                cx.update(|cx| state.read(cx).settings().background_opacity == 1.0)
            }) {
                back = true;
                break;
            }
        }
        assert!(back, "the slider could not put the window back to solid");
        // And the theme file's own editor has it back: the fills the setting wrote
        // are the theme's, so a solid window is the theme as it was written.
        let component = cx.update(|cx| gpui_kit::component::Theme::global(cx).clone());
        assert_eq!(
            component
                .highlight_theme
                .style
                .editor_background
                .map(|fill| fill.a),
            Some(1.),
            "the editor's background did not come back solid"
        );
        assert_eq!(
            component.highlight_theme.style.editor_gutter_background, None,
            "the gutter's own fill did not come back as the theme has it"
        );
    }

    /// The thumb is wider than the track is high and sits centred on it, so it
    /// hangs past either end of the travel. That overhang has to stay inside the
    /// pane the rows are drawn in — the rows scroll, and the scroller's clip is
    /// the pane's own edge, which drew the solid end of the transparency slider
    /// as a half circle.
    #[gpui_kit::test]
    fn a_slider_thumb_is_whole_at_the_ends_of_its_track(cx: &mut TestAppContext) {
        let fixture = open(cx, "settings-thumb");
        open_settings_tab(cx, &fixture, "settings-tab-appearance");
        let state = fixture.state.clone();

        let scale = cx
            .update_window(fixture.window.into(), |_, window, _| window.scale_factor())
            .unwrap();
        // What a clip does to a frame is only readable off the quads: they carry
        // the bounds they were painted at beside the clip they were drawn under.
        let cut = |cx: &mut TestAppContext, at: &str| {
            let row = bounds_of(cx, &fixture, "settings-opacity").scale(scale);
            let quads = cx
                .update_window(fixture.window.into(), |_, window, _| window.painted_quads())
                .unwrap();
            let cut: Vec<_> = quads
                .into_iter()
                .filter(|quad| {
                    let (painted, clip) = (&quad.bounds, &quad.content_mask.bounds);
                    row.contains(&painted.center())
                        && !(clip.left() <= painted.left()
                            && clip.top() <= painted.top()
                            && clip.right() >= painted.right()
                            && clip.bottom() >= painted.bottom())
                })
                .collect();
            assert!(
                cut.is_empty(),
                "{at}: this row is painted outside the clip it is drawn under: {cut:?}"
            );
        };

        // The solid end is where the form opens, so it is on screen already.
        draw(cx, &fixture);
        cut(cx, "the solid end");
        // And the far end, once the value the click asked for has landed in the
        // state that moves the thumb.
        let at_min = |cx: &mut TestAppContext| {
            cx.update(|cx| state.read(cx).settings().background_opacity == MIN_OPACITY)
        };
        let mut reached = false;
        for _ in 0..3 {
            click_at_fraction(cx, &fixture, "settings-opacity-slider", 1.);
            if draw_until(cx, &fixture, at_min) {
                reached = true;
                break;
            }
        }
        assert!(
            reached,
            "the click at the far end of the track never got there"
        );
        draw(cx, &fixture);
        cut(cx, "the transparent end");
    }

    /// The settings form floats over the window instead of dimming it: no scrim,
    /// because the window behind it is what the transparency setting is for.
    /// Escape and `Done` are the ways out — a click outside is neither, and it
    /// does not reach the window either.
    #[gpui_kit::test]
    fn the_settings_form_floats_without_a_scrim(cx: &mut TestAppContext) {
        let fixture = open(cx, "settings-no-scrim");
        open_settings(cx, &fixture);

        // A click outside it: the form stays up, and the control behind the form
        // — the tab bar's `+`, which opens the new-tab menu — does not see it. A
        // form without a scrim is still a modal.
        let form = bounds_of(cx, &fixture, "settings");
        let plus = bounds_of(cx, &fixture, "add-tab");
        assert!(
            !form.contains(&plus.center()),
            "the probe is under the form: {plus:?}, {form:?}"
        );
        click(cx, &fixture, "add-tab".to_string());
        draw(cx, &fixture);
        assert!(
            found(cx, &fixture, "settings"),
            "the form closed on a click outside it"
        );
        assert!(
            !found(cx, &fixture, "add-menu"),
            "the click went past the form"
        );

        // Escape is the way out, and it still works.
        press(cx, &fixture, "escape");
        assert!(
            draw_until(cx, &fixture, |cx| !found(cx, &fixture, "settings")),
            "escape left the form up"
        );
    }

    /// The form is a roomy two-pane dialog: the tabs and the rows of the selected one, filling a
    /// surface big enough to read them on.
    #[gpui_kit::test]
    fn the_settings_form_is_a_roomy_dialog(cx: &mut TestAppContext) {
        let fixture = open(cx, "settings-roomy");
        open_settings(cx, &fixture);
        draw(cx, &fixture);

        let form = bounds_of(cx, &fixture, "settings");
        assert!(
            form.size.width > px(840.),
            "the form is not as wide as the dialog it is in: {form:?}"
        );
        assert!(
            form.size.height > px(420.),
            "the form is not as tall as the dialog it is in: {form:?}"
        );
        // The two panes: the tabs beside the rows, both the dialog's height.
        let tabs = bounds_of(cx, &fixture, "settings-tabs");
        let rows = bounds_of(cx, &fixture, "settings-rows");
        assert!(
            tabs.origin.x < rows.origin.x,
            "the tabs are not beside the rows: {tabs:?}, {rows:?}"
        );
        assert!(
            (tabs.size.height - rows.size.height).as_f32().abs() < 1.,
            "the two panes are not the same height: {tabs:?}, {rows:?}"
        );
        assert!(
            rows.origin.x - tabs.right() >= px(20.),
            "the settings panes are cramped"
        );
        let language = bounds_of(cx, &fixture, "settings-language");
        let command = bounds_of(cx, &fixture, "settings-commit-message");
        assert!(
            language.size.height >= px(44.),
            "the control row is cramped"
        );
        assert!(
            command.origin.y >= language.bottom() + px(8.),
            "settings sections touch"
        );
    }

    #[gpui_kit::test]
    fn settings_content_scrolls_without_moving_the_categories(cx: &mut TestAppContext) {
        let fixture = open(cx, "settings-scroll");
        open_settings(cx, &fixture);
        draw(cx, &fixture);
        cx.simulate_window_resize(fixture.window.into(), size(px(1000.), px(440.)));
        draw(cx, &fixture);
        let viewport = bounds_of(cx, &fixture, "settings-rows");
        let content = bounds_of(cx, &fixture, "settings-content");
        let tabs = bounds_of(cx, &fixture, "settings-tabs");
        assert!(
            content.size.height > viewport.size.height,
            "settings content is clipped rather than scrollable: {content:?}, {viewport:?}"
        );
        let before = bounds_of(cx, &fixture, "settings-language").origin.y;
        cx.update_window(fixture.window.into(), |_, window, cx| {
            window.scroll(
                "settings-language",
                ScrollDelta::Lines(Point { x: 0., y: -40. }),
                cx,
            );
        })
        .unwrap();
        draw(cx, &fixture);
        let after = bounds_of(cx, &fixture, "settings-language").origin.y;
        assert!(before - after > px(20.), "the settings body did not scroll");
        assert_eq!(tabs.origin, bounds_of(cx, &fixture, "settings-tabs").origin);
    }

    /// Dialogs use a readable translucent tint when window transparency is enabled.
    #[gpui_kit::test]
    fn a_dialog_follows_window_transparency(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        cx.update(|cx| crate::settings_view::apply_opacity(cx, 0.4, 0));

        let (window, dialog) = cx.update(|cx| {
            (
                gpui_kit::base::Theme::global(cx).tokens.colors.background,
                dialog_background(cx),
            )
        });
        assert_eq!(window.a, 0.4, "the window itself is not see-through");
        assert_eq!(dialog.a, 0.94, "the dialog has no readable glass tint");
        // Only alpha changes; the glass tint keeps the active theme's colour.
        assert_eq!(
            (dialog.h, dialog.s, dialog.l),
            (window.h, window.s, window.l)
        );
    }

    /// The form is a column of tabs and the rows of the one selected: every
    /// setting is in one of them, and picking a tab swaps them. It opens on the
    /// general settings, the everyday ones.
    #[gpui_kit::test]
    fn the_settings_tabs_hold_the_rows_they_name(cx: &mut TestAppContext) {
        let fixture = open(cx, "settings-tabs");
        open_settings(cx, &fixture);
        let assert_stacked = |cx: &mut TestAppContext, label: &str, control: &str| {
            let label = bounds_of(cx, &fixture, label);
            let control = bounds_of(cx, &fixture, control);
            assert!(
                control.origin.y >= label.bottom() + px(6.),
                "the control is not spaced below its label: {label:?}, {control:?}"
            );
        };
        draw(cx, &fixture);
        assert_stacked(cx, "settings-language-label", "settings-language-select");

        // It opens on the general rows, and the other tabs' rows are not drawn: a
        // form is one category at a time.
        assert!(found(cx, &fixture, "settings-language"));
        assert!(found(cx, &fixture, "settings-computer-awake"));
        assert!(!found(cx, &fixture, "settings-font-plus"));
        assert!(!found(cx, &fixture, "settings-opacity-slider"));

        click_visible(cx, &fixture, "settings-tab-terminal");
        assert!(
            draw_until(cx, &fixture, |cx| found(cx, &fixture, "settings-font-plus")),
            "the terminal tab showed no rows"
        );
        assert!(found(cx, &fixture, "settings-scrollback-plus"));
        assert_stacked(cx, "settings-font-label", "settings-font-plus");
        assert_stacked(cx, "settings-scrollback-label", "settings-scrollback-plus");
        assert!(!found(cx, &fixture, "settings-computer-awake"));

        click_visible(cx, &fixture, "settings-tab-appearance");
        assert!(
            draw_until(cx, &fixture, |cx| found(
                cx,
                &fixture,
                "settings-opacity-slider"
            )),
            "the appearance tab showed no rows"
        );
        assert!(found(cx, &fixture, "settings-appearance-select"));
        assert_stacked(
            cx,
            "settings-appearance-label",
            "settings-appearance-select",
        );
        assert_stacked(cx, "settings-opacity-label", "settings-opacity-slider");
        assert_stacked(cx, "settings-blur-label", "settings-blur-slider");
        assert!(!found(cx, &fixture, "settings-font-plus"));
    }

    /// The language is a dropdown on the general tab, and picking one lands in the
    /// state and in the file the state is written to.
    #[gpui_kit::test]
    fn the_language_setting_is_a_dropdown_that_reaches_the_state(cx: &mut TestAppContext) {
        let fixture = open(cx, "settings-language");
        open_settings(cx, &fixture);

        assert!(
            found(cx, &fixture, "settings-language-select"),
            "the general tab has no language picker"
        );

        // Open it and take the next language after the one in effect, through the
        // keyboard the list already answers to.
        click_visible(cx, &fixture, "settings-language-select");
        draw(cx, &fixture);
        press(cx, &fixture, "down");
        press(cx, &fixture, "enter");

        let state = fixture.state.clone();
        assert!(
            draw_until(cx, &fixture, |cx| cx.update(|cx| state
                .read(cx)
                .settings()
                .language
                .is_some())),
            "the picker's choice did not reach the state"
        );
        let written = std::fs::read_to_string(&fixture.settings_path).unwrap();
        assert!(
            written.contains("language"),
            "the language never reached the file: {written}"
        );
    }

    /// The AI command is a field on the general tab, and what is typed lands in
    /// the state and in the file the state is written to.
    #[gpui_kit::test]
    fn the_ai_command_setting_reaches_the_state(cx: &mut TestAppContext) {
        let fixture = open(cx, "settings-ai-command");
        open_settings(cx, &fixture);

        assert!(
            found(cx, &fixture, "settings-commit-message-input"),
            "the general tab has no AI command field"
        );
        click_visible(cx, &fixture, "settings-commit-message-input");
        type_text(cx, &fixture, "cat");

        let state = fixture.state.clone();
        assert!(
            draw_until(cx, &fixture, |cx| cx.update(|cx| state
                .read(cx)
                .settings()
                .commit_message_command
                .as_deref()
                == Some("cat"))),
            "the command never reached the state"
        );
        let written = std::fs::read_to_string(&fixture.settings_path).unwrap();
        assert!(
            written.contains("cat"),
            "the command never reached the file: {written}"
        );
    }

    /// The scrollback moves in the form, and lands in the state — which is where a terminal's spec
    /// reads it from (`with_settings`, tested where it is defined: what a running terminal
    /// keeps is its own config, and a setting that says "new terminals" has to mean it).
    #[gpui_kit::test]
    fn the_scrollback_setting_reaches_the_state(cx: &mut TestAppContext) {
        let fixture = open(cx, "settings-scrollback");
        open_settings_tab(cx, &fixture, "settings-tab-terminal");
        // Through `click_until`: the button's own click is occasionally dropped,
        // and the assertion is about the state, not about how many clicks it
        // took.
        let state = fixture.state.clone();
        assert!(
            click_until(
                cx,
                &fixture,
                "settings-scrollback-plus".to_string(),
                move |cx| cx.update(|cx| state.read(cx).settings().terminal.scrollback) == 6_000
            ),
            "the form's change did not reach the state: {}",
            cx.update(|cx| fixture.state.read(cx).settings().terminal.scrollback)
        );
        // And what the form says about when it lands is on screen.
        assert!(found(cx, &fixture, "settings"), "the form went away");
    }

    /// The sleep setting is a row of the general tab, and picking an answer lands
    /// in the state and in the file the state writes it to.
    #[gpui_kit::test]
    fn the_sleep_setting_reaches_the_state(cx: &mut TestAppContext) {
        let fixture = open(cx, "settings-awake");
        open_settings_tab(cx, &fixture, "settings-tab-general");

        // The row is there, with the answers in it.
        assert!(
            draw_until(cx, &fixture, |cx| found(
                cx,
                &fixture,
                "settings-computer-awake"
            )),
            "the general tab has no sleep row"
        );
        assert!(found(cx, &fixture, "settings-computer-awake-off"));
        assert!(found(cx, &fixture, "settings-computer-awake-auto"));

        let state = fixture.state.clone();
        assert!(
            click_until(
                cx,
                &fixture,
                "settings-computer-awake-auto".to_string(),
                move |cx| cx.update(|cx| state.read(cx).settings().computer_awake)
                    == ComputerAwakeMode::Auto
            ),
            "the form's choice did not reach the state"
        );
        // `settings.toml` says the same thing, spelled the way a user writes it.
        let written = std::fs::read_to_string(&fixture.settings_path).unwrap();
        assert!(written.contains("computer_awake = \"auto\""), "{written}");
    }

    /// The split's way in by mouse: the pane's own context menu, with no split
    /// button anywhere in this app's chrome — the menu names the same keys the
    /// keymap binds.
    #[gpui_kit::test]
    fn the_pane_menu_splits(cx: &mut TestAppContext) {
        let fixture = open(cx, "split-menu");
        // A file is opened and then the terminal is looked at again: the pane
        // area shows one tab at a time, and the menu belongs to the terminal —
        // which has to be the tab on screen for a right-click to land on it.
        click(cx, &fixture, tree_row(&fixture, NOTES));
        assert!(draw_until(cx, &fixture, |cx| file_tabs(&active_tabs(
            cx, &fixture
        ))
        .len()
            == 1));
        assert!(!found(cx, &fixture, "pane-divider"));
        click_visible(cx, &fixture, "tab-0");
        assert!(
            draw_until(cx, &fixture, |cx| cx.update(|cx| fixture
                .workspace
                .read(cx)
                .active_terminal(cx)
                .is_some())),
            "the terminal is not the tab on screen"
        );

        open_pane_menu(cx, &fixture);
        assert_eq!(
            label(cx, &fixture, "pane-menu-split-right").as_deref(),
            Some("Split right")
        );
        assert_eq!(
            label(cx, &fixture, "pane-menu-split-down").as_deref(),
            Some("Split down")
        );
        click_visible(cx, &fixture, "pane-menu-split-right");
        assert!(
            draw_until(cx, &fixture, |cx| found(cx, &fixture, "pane-divider")),
            "the menu did not split"
        );
        // The menu is gone once it has done its thing: a menu floating over the
        // layout it just changed is the kind of thing that feels broken.
        assert!(!found(cx, &fixture, "pane-menu"));
        assert_eq!(
            on_screen(cx, &fixture).len(),
            2,
            "the split did not put a pane beside the one that was focused"
        );
        // One tab, and the bar says it is split — the glyph comes before the
        // name.
        assert!(
            found(cx, &fixture, "tab-onscreen-0"),
            "the split tab is unmarked"
        );
        assert_eq!(
            open_tabs(cx, &fixture),
            2,
            "the split opened a tab of its own"
        );

        // And the other axis is the other entry.
        open_pane_menu(cx, &fixture);
        click_visible(cx, &fixture, "pane-menu-split-down");
        assert!(
            draw_until(cx, &fixture, |cx| found(cx, &fixture, "pane-0-1")),
            "the down entry did not divide the pane it was opened on"
        );
        assert!(
            !found(cx, &fixture, "pane-1-0"),
            "the down entry divided the pane with the keyboard instead"
        );
        assert_eq!(
            on_screen(cx, &fixture).len(),
            3,
            "the down entry did not make a third pane"
        );
    }

    /// The report this is here for: two panes, a right-click on the terminal, and "Split down" has
    /// to divide *that* pane.
    #[gpui_kit::test]
    fn the_pane_menu_splits_the_pane_it_was_opened_on(cx: &mut TestAppContext) {
        let fixture = open(cx, "split-menu-pane");
        click(cx, &fixture, tree_row(&fixture, NOTES));
        assert!(draw_until(cx, &fixture, |cx| file_tabs(&active_tabs(
            cx, &fixture
        ))
        .len()
            == 1));
        // The terminal's tab is the one on screen, so a split divides *its* pane and opens a second
        // terminal beside it — with the keyboard.
        click_visible(cx, &fixture, "tab-0");
        assert!(
            draw_until(cx, &fixture, |cx| cx.update(|cx| fixture
                .workspace
                .read(cx)
                .active_terminal(cx)
                .is_some())),
            "the terminal is not the tab on screen"
        );
        press(cx, &fixture, "cmd-d");
        draw(cx, &fixture);
        assert_eq!(on_screen(cx, &fixture).len(), 2);

        open_pane_menu(cx, &fixture);
        click_visible(cx, &fixture, "pane-menu-split-down");
        assert!(
            draw_until(cx, &fixture, |cx| found(cx, &fixture, "pane-0-1")),
            "the split did not divide the pane the menu was opened on"
        );
        assert!(
            !found(cx, &fixture, "pane-1-0"),
            "the split divided the pane with the keyboard instead"
        );
        // Three panes, and the right half still belongs to the terminal the first
        // split opened.
        assert_eq!(on_screen(cx, &fixture).len(), 3);
        let terminal = bounds_of(cx, &fixture, "pane-0-0");
        let editor = bounds_of(cx, &fixture, "pane-1");
        assert!(
            terminal.origin.x < editor.origin.x,
            "the terminal is not the pane on the left: {terminal:?} beside {editor:?}"
        );
    }

    /// The menu's own padding, and the pixels of the pane area outside it.
    #[gpui_kit::test]
    fn the_pane_menus_own_padding_and_the_pane_area_outside_it(cx: &mut TestAppContext) {
        let fixture = open(cx, "pane-menu-padding");
        open_pane_menu(cx, &fixture);
        let menu = bounds_of(cx, &fixture, "pane-menu");

        // A pixel inside the menu's own padding, at its top-left corner.
        let at = Point::new(menu.origin.x + px(1.), menu.origin.y + px(1.));
        cx.update_window(fixture.window.into(), |_, window, cx| {
            window.dispatch_event(
                gpui_kit::MouseMoveEvent {
                    position: at,
                    pressed_button: None,
                    modifiers: Default::default(),
                }
                .to_platform_input(),
                cx,
            );
            window.render_frame(cx);
            window.dispatch_event(
                gpui_kit::MouseDownEvent {
                    button: gpui_kit::MouseButton::Left,
                    position: at,
                    modifiers: Default::default(),
                    click_count: 1,
                    first_mouse: false,
                }
                .to_platform_input(),
                cx,
            );
            window.dispatch_event(
                gpui_kit::MouseUpEvent {
                    button: gpui_kit::MouseButton::Left,
                    position: at,
                    modifiers: Default::default(),
                    click_count: 1,
                }
                .to_platform_input(),
                cx,
            );
            window.render_frame(cx);
        })
        .expect("click the menu's padding");
        assert!(
            found(cx, &fixture, "pane-menu"),
            "a click on the menu's own padding closed it and went to the grid"
        );

        // And somewhere else in the pane area: the click is the scrim's, so the
        // menu closes and the grid underneath does not act.
        let outside = Point::new(px(4.), px(4.));
        cx.update_window(fixture.window.into(), |_, window, cx| {
            window.dispatch_event(
                gpui_kit::MouseDownEvent {
                    button: gpui_kit::MouseButton::Left,
                    position: outside,
                    modifiers: Default::default(),
                    click_count: 1,
                    first_mouse: false,
                }
                .to_platform_input(),
                cx,
            );
            window.render_frame(cx);
        })
        .expect("click outside the menu");
        assert!(
            !found(cx, &fixture, "pane-menu"),
            "a click outside the pane menu did not close it"
        );
    }

    /// Escape closes the pane menu, the way it closes every other overlay.
    #[gpui_kit::test]
    fn escape_closes_the_pane_menu(cx: &mut TestAppContext) {
        let fixture = open(cx, "split-menu-escape");
        open_pane_menu(cx, &fixture);
        press(cx, &fixture, "escape");
        assert!(
            draw_until(cx, &fixture, |cx| !found(cx, &fixture, "pane-menu")),
            "escape did not close the pane menu"
        );
        assert!(
            !found(cx, &fixture, "pane-divider"),
            "escape split something"
        );
    }

    /// A change is written to `settings.toml`, which is what a restart reads.
    #[gpui_kit::test]
    fn a_settings_change_is_written_to_the_file(cx: &mut TestAppContext) {
        let fixture = open(cx, "settings-write");
        assert!(
            !fixture.settings_path.exists(),
            "the fixture wrote settings.toml before anything changed one"
        );

        open_settings_tab(cx, &fixture, "settings-tab-terminal");
        // Waiting for the write rather than asserting on the instant: the write
        // is an effect, and a dropped synthetic click looks exactly like a slow
        // one from here.
        assert!(
            click_until(cx, &fixture, "settings-font-plus".to_string(), |_cx| {
                fixture.settings_path.exists()
            }),
            "changing a setting never wrote settings.toml"
        );

        let written = std::fs::read_to_string(&fixture.settings_path)
            .expect("changing a setting did not write settings.toml");
        // The value, and the section it belongs to: a file that lost the shape
        // the user hand-edits is worse than no file.
        assert!(
            written.contains("[terminal]") && written.contains("font_size = 14"),
            "the written file does not carry the change:\n{written}"
        );
    }

    /// A write that fails is said out loud: the app already looks like what the
    /// user picked, so this line is the only place they can learn it will not
    /// survive a restart.
    #[gpui_kit::test]
    fn a_settings_write_that_fails_says_so(cx: &mut TestAppContext) {
        let fixture = open(cx, "settings-unwritable");
        // A directory where the file goes: the write's rename cannot land on it.
        std::fs::create_dir_all(&fixture.settings_path).unwrap();

        open_settings_tab(cx, &fixture, "settings-tab-terminal");
        // Through `click_until`: a synthetic click is occasionally dropped, and
        // what this is about is that the failure is said, not how many clicks it
        // took to make one land.
        assert!(
            click_until(cx, &fixture, "settings-font-plus".to_string(), |cx| found(
                cx,
                &fixture,
                "settings-error"
            )),
            "a failed write said nothing"
        );
        let said = label(cx, &fixture, "settings-error").unwrap_or_default();
        assert!(
            said.contains("settings.toml"),
            "the form did not say what it could not write: {said}"
        );
        // And the change still applied: the user asked for it, and the app is
        // consistent for as long as it runs.
        assert_eq!(
            cx.update(|cx| fixture.state.read(cx).settings().terminal.font_size),
            14.,
        );
    }

    /// The settings form, open. The palette opens it too; this is the same call
    /// the palette's row makes.
    fn open_settings(cx: &mut TestAppContext, fixture: &Fixture) {
        cx.update_window(fixture.window.into(), |_, window, cx| {
            fixture
                .workspace
                .update(cx, |workspace, cx| workspace.open_settings_form(window, cx))
        })
        .expect("open the settings form");
    }

    /// The settings form, with one of its tabs showing. The rows live behind
    /// their tab, so a test that clicks a row has to be looking at it.
    fn open_settings_tab(cx: &mut TestAppContext, fixture: &Fixture, tab: &str) {
        open_settings(cx, fixture);
        click_visible(cx, fixture, tab);
        draw(cx, fixture);
    }

    /// Click an element a fraction of the way across it. A slider is a value
    /// along a track: where the click lands is the value.
    fn click_at_fraction(cx: &mut TestAppContext, fixture: &Fixture, id: &str, at: f32) {
        let bounds = bounds_of(cx, fixture, id);
        // A pixel inside the edge: the click is refused when it lands exactly on
        // it, and the far end of a slider is a value worth reaching.
        let offset = point((bounds.size.width - px(1.)) * at, bounds.size.height / 2.);
        let id = id.to_string();
        cx.update_window(fixture.window.into(), move |_, window, cx| {
            window.click_at(id, offset, cx);
        })
        .expect("click the element");
    }

    /// The rows the visible terminal's grid has, which is how a cell size is
    /// readable from outside: the same window fits fewer rows in bigger type.
    fn terminal_rows(cx: &mut TestAppContext, fixture: &Fixture) -> usize {
        cx.update(|cx| {
            fixture
                .workspace
                .read(cx)
                .active_terminal(cx)
                .expect("a terminal on screen")
                .read(cx)
                .screen()
                .screen_lines
        })
    }

    /// The settings a terminal's spec is built with: the scrollback, and the
    /// font size — which the terminal takes from the theme rather than from the
    /// spec, so the spec carries the one value it cannot read for itself.
    #[test]
    fn a_spec_takes_the_scrollback_from_the_settings() {
        let settings = syro_core::config::TerminalSettings {
            scrollback: 6_000,
            font_size: 19.,
        };
        let spec = with_settings(
            TermSpec::shell(std::env::temp_dir(), TermSize::new(80, 24)),
            &settings,
        );
        assert_eq!(spec.scrollback, 6_000);
    }

    /// What ten terminals cost, measured rather than hoped for.
    #[gpui_kit::test]
    fn ten_terminals_cost_what_the_plan_assumes(cx: &mut TestAppContext) {
        let fixture = open(cx, "measure-terminals");
        let baseline = own_rss_kb();

        let mut views = spawn_terminals(cx, &fixture, 1);
        let one = own_rss_kb();

        views.append(&mut spawn_terminals(cx, &fixture, 9));
        let ten = own_rss_kb();

        assert_eq!(views.len(), 10);
        let live = views
            .iter()
            .filter(|view| cx.update(|cx| view.read(cx).exit_status().is_none()))
            .count();
        assert_eq!(live, 10, "not every terminal has a running child");

        // Printed rather than only asserted: it is a measurement.
        println!(
            "syro terminals (debug): baseline {:.1} MB, 1 terminal {:.1} MB, \
             10 terminals {:.1} MB, marginal {:.1} MB/terminal",
            mb(baseline),
            mb(one),
            mb(ten),
            mb(ten.saturating_sub(one)) / 9.,
        );

        // The cheap, true half: nine more terminals are not nine times the plan's whole budget.
        let marginal = ten.saturating_sub(one);
        assert!(
            marginal < 9 * 30 * 1024,
            "nine terminals cost {:.1} MB, which is past the 30 MB each a debug build is allowed",
            mb(marginal)
        );
    }

    /// `n` live terminals in `fixture`'s window, each with a running child and something in its
    /// grid.
    fn spawn_terminals(
        cx: &mut TestAppContext,
        fixture: &Fixture,
        n: usize,
    ) -> Vec<Entity<TerminalView>> {
        let spec = TermSpec {
            command: "/bin/sh".to_string(),
            // Five thousand lines, which is the scrollback the plan's scenario asks for and where a
            // terminal's memory actually goes; then a sleep that keeps the child alive, because
            // an exited child would make "a running child" meaningless.
            args: vec!["-c".to_string(), "seq 1 5000; sleep 60".to_string()],
            ..TermSpec::shell(fixture.root.clone(), TermSize::new(120, 40))
        };
        let views: Vec<Entity<TerminalView>> = cx.update(|cx| {
            (0..n)
                .map(|_| {
                    let spec = spec.clone();
                    cx.new(|cx| {
                        TerminalView::new(cx, spec, None, None, None).expect("a scratch terminal")
                    })
                })
                .collect()
        });

        assert!(
            draw_until(cx, fixture, |cx| views.iter().all(
                |view| cx.update(|cx| view.read(cx).screen().text().contains("5000"))
            )),
            "a terminal never finished printing"
        );
        views
    }

    /// This process's resident set, in kilobytes — the whole process, so the
    /// difference between two readings is what changed in it.
    fn own_rss_kb() -> u64 {
        let pid = std::process::id().to_string();
        let out = std::process::Command::new("ps")
            .args(["-o", "rss=", "-p", &pid])
            .output()
            .expect("ps answered");
        String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse()
            .expect("ps printed a number of kilobytes")
    }

    fn mb(kb: u64) -> f64 {
        kb as f64 / 1024.
    }

    /// Restart the worktree's terminal, which is what builds a view that
    /// measures its cells again.
    fn restart_terminal(cx: &mut TestAppContext, fixture: &Fixture) {
        let worktree_id = cx.update(|cx| {
            fixture
                .state
                .read(cx)
                .active_id()
                .cloned()
                .expect("an active worktree")
        });
        cx.update_window(fixture.window.into(), |_, window, cx| {
            window.dispatch_action(
                Box::new(crate::state::terminal::New {
                    worktree_id,
                    agent: None,
                    resume: None,
                    seed: None,
                    model: None,
                }),
                cx,
            );
        })
        .expect("restart the terminal");
    }

    // --- The window itself -----------------------------------------------

    /// A window the display cannot hold is shrunk to fit, rather than opening with its last lines —
    /// this app's status bar among them — below the Dock.
    #[gpui_kit::test]
    fn a_window_bigger_than_the_display_is_shrunk_to_fit(cx: &mut TestAppContext) {
        let window = open_blank(cx, size(px(2000.), px(1200.)));
        cx.update_window(window.into(), |_, window, cx| fit_to_display(window, cx))
            .expect("fit the window");
        // The platform's own bounds, not `viewport_size`: that is a snapshot
        // taken when the window is resized by the platform, and the test
        // platform's `resize` is a mutation with no event behind it.
        assert_eq!(
            cx.update_window(window.into(), |_, window, _| window.window_bounds())
                .expect("read the window"),
            WindowBounds::Windowed(Bounds::new(Point::default(), size(px(1912.), px(1072.)))),
            "the window was not fitted to the display"
        );
    }

    /// And a window that already fits is left exactly as it is: the fit is a
    /// correction, not a resize on every start.
    #[gpui_kit::test]
    fn a_window_that_fits_is_left_alone(cx: &mut TestAppContext) {
        let window = open_blank(cx, size(px(900.), px(500.)));
        cx.update_window(window.into(), |_, window, cx| fit_to_display(window, cx))
            .expect("fit the window");
        assert_eq!(
            cx.update_window(window.into(), |_, window, _| window.window_bounds())
                .expect("read the window"),
            WindowBounds::Windowed(Bounds::new(Point::default(), size(px(900.), px(500.)))),
            "a window that fits was resized anyway"
        );
    }

    /// A window of the given size, holding nothing.
    fn open_blank(cx: &mut TestAppContext, content: gpui_kit::Size<Pixels>) -> WindowHandle<Root> {
        cx.update(|cx| {
            let bounds = Bounds {
                origin: Point::default(),
                size: content,
            };
            // The component layer's theme global comes from `init`, and the
            // window's root reads it.
            gpui_kit::init(cx);
            let (window, _blank) = gpui_kit::open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    ..Default::default()
                },
                cx,
                |_, cx| cx.new(|_| Blank),
            )
            .expect("open a blank window");
            window.downcast::<Root>().expect("Base Root")
        })
    }
}
