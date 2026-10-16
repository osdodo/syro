//! The sidebar: repos, the worktrees under each, and which one is active.

use std::collections::HashSet;

use gpui_kit::base::{ColorTokens, StyledExt as _, TestSupportExt as _, Theme};
use gpui_kit::component::{Icon, IconName};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rust_i18n::t;
use syro_core::model::{AgentStatus, RepoId, Worktree};

use crate::resize::{self, WidthDrag};
use crate::resource_bar::{ResourceBar, Row, format_bytes};
use crate::state::{AppState, branch_or_name, worktree};

/// The width a fresh window opens with, and the range the drag handle keeps it
/// in. The width a files-app sidebar has.
const WIDTH: f32 = 240.;
const MIN_WIDTH: f32 = 220.;
const MAX_WIDTH: f32 = 500.;
/// The header's height: the 24 px search field plus 12 px above and below, so
/// the field sits well clear of the window's top edge.
const HEADER_HEIGHT: f32 = 48.;
/// One row of a menu, so that the submenu beside it can be put on the row that
/// opened it rather than a guess away from it.
const MENU_ROW_HEIGHT: f32 = 23.;
/// The menu panel's width, which is also where its submenu starts.
const MENU_WIDTH: f32 = 190.;
/// The menu's own padding, which is where its first row starts.
const MENU_PADDING: f32 = 4.;
/// The air between two rows, which the submenu's own position has to count.
const MENU_GAP: f32 = 2.;
/// The columns a worktree row ends in: the status word, and the two numbers the
/// readout measured. The word's column is the width of the widest word plus the
/// breathing dot that rides beside `Working` — a row with no word keeps it too,
/// so every name ends at the same place rather than a shell's title running
/// through the status column to reach the numbers. The two numbers are floors
/// rather than widths, and they are read at the widest they get — `100%`,
/// `1023 MB`: a number that outgrows its column takes the room from the name,
/// where a fixed width would fold it onto a second line and paint that line over
/// the row below.
const STATUS_MAX_WIDTH: f32 = 64.;
const CPU_WIDTH: f32 = 30.;
const MEM_WIDTH: f32 = 46.;
/// The length a terminal row ends in: wide enough for `1h05m`, so a running
/// terminal's numbers do not creep left as its minutes turn into hours. A floor
/// like the numbers above, for the same reason. The columns are all as narrow as
/// their own contents, because a 240-point sidebar has to hold a status, a
/// title, a cost and a length on one line.
const UPTIME_WIDTH: f32 = 38.;

/// How the worktrees of each repository are ordered.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
enum Sort {
    /// The order git reports, which is the order this panel has always drawn:
    /// the repository's own checkout first.
    #[default]
    Project,
    /// By the branch, alphabetically.
    Name,
    /// By what the agents in it are doing: waiting for you, then working, then
    /// everything else.
    AgentActivity,
    /// By when this window was last pointed at it.
    Recent,
}

impl Sort {
    /// In the order the menu lists them, which is what the submenu shows.
    const ALL: [Sort; 4] = [Self::Name, Self::AgentActivity, Self::Recent, Self::Project];

    fn label(self) -> String {
        match self {
            Self::Project => t!("sidebar.sort_project"),
            Self::Name => t!("sidebar.sort_name"),
            Self::AgentActivity => t!("sidebar.sort_activity"),
            Self::Recent => t!("sidebar.sort_recent"),
        }
        .to_string()
    }

    /// The id a test clicks an entry by.
    fn id(self) -> &'static str {
        match self {
            Self::Project => "sort-project",
            Self::Name => "sort-name",
            Self::AgentActivity => "sort-activity",
            Self::Recent => "sort-recent",
        }
    }
}

/// A row's menu: the panel's own, a worktree's, or a repository heading's — and
/// where it was opened.
#[derive(Clone)]
struct Menu {
    /// What the entries act on.
    target: Target,
    /// Where the click was, in window coordinates. The menu is drawn in a layer
    /// above the panel (see `menu_overlay`), because the rows scroll and a menu
    /// drawn inside them would be clipped at the panel's edge.
    at: Point<Pixels>,
    /// Whether the sort submenu is open beside it.
    sort_open: bool,
}

/// What a menu's entries act on: the panel itself, one worktree, or a whole
/// repository.
#[derive(Clone)]
enum Target {
    Sidebar,
    Worktree {
        worktree_id: String,
        /// The worktree's repository, which is what "new worktree" is about.
        repo_id: String,
        /// Whether this row is the repository's own checkout. That one is a
        /// project rather than a worktree: git refuses to remove it as one, so
        /// the menu says what the dialog will ask.
        own_checkout: bool,
    },
    Repo {
        repo_id: String,
    },
}

impl Target {
    /// The repository, where the target has one.
    fn repo_id(&self) -> Option<&str> {
        match self {
            Self::Sidebar => None,
            Self::Worktree { repo_id, .. } | Self::Repo { repo_id } => Some(repo_id),
        }
    }

    /// The prefix a test finds this menu's entries under: the menus are
    /// different lists, and an entry that reads the same in two of them is still
    /// two different clicks.
    fn prefix(&self) -> &'static str {
        match self {
            Self::Sidebar => "sidebar-menu",
            Self::Worktree { .. } => "worktree-menu",
            Self::Repo { .. } => "repo-menu",
        }
    }
}

/// One entry of a menu.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum MenuItem {
    ShowHidden,
    HideAsleep,
    SortBy,
    NewWorktree,
    SharedPaths,
    Reveal,
    HideRepo,
    RemoveWorktree,
    RemoveRepo,
}

impl MenuItem {
    /// The entries a menu for `target` lists, in the order it lists them: what
    /// makes something comes first, and what throws something away is last.
    ///
    /// A worktree's menu has no "new worktree" in it: that belongs to the
    /// repository, and the repository's own menu is where it is.
    fn for_target(target: &Target) -> &'static [MenuItem] {
        match target {
            Target::Sidebar => &[Self::ShowHidden, Self::HideAsleep, Self::SortBy],
            Target::Worktree { .. } => &[Self::Reveal, Self::RemoveWorktree],
            Target::Repo { .. } => &[
                Self::NewWorktree,
                Self::SharedPaths,
                Self::HideRepo,
                Self::RemoveRepo,
            ],
        }
    }

    fn label(self, target: &Target) -> String {
        match self {
            Self::ShowHidden => t!("sidebar.show_hidden"),
            Self::HideAsleep => t!("sidebar.hide_asleep"),
            Self::SortBy => t!("sidebar.sort_by"),
            Self::NewWorktree => t!("sidebar.new_worktree"),
            Self::SharedPaths => t!("sidebar.shared_paths"),
            Self::Reveal => t!("sidebar.reveal_worktree"),
            Self::HideRepo => t!("sidebar.hide_repo"),
            // The repository's own checkout is the project itself, and the
            // dialog it opens asks the project question — so the entry says that
            // too, rather than promising to remove a worktree that is not one.
            Self::RemoveWorktree => match target {
                Target::Worktree {
                    own_checkout: true, ..
                } => t!("dialog.remove_project"),
                _ => t!("sidebar.remove_worktree"),
            },
            Self::RemoveRepo => t!("dialog.remove_project"),
        }
        .to_string()
    }

    /// The id a test clicks an entry by.
    fn id(self, target: &Target) -> String {
        let entry = match self {
            Self::ShowHidden => "show-hidden",
            Self::HideAsleep => "hide-asleep",
            Self::SortBy => "sort-by",
            Self::NewWorktree => "new",
            Self::SharedPaths => "shared-paths",
            Self::Reveal => "reveal",
            Self::HideRepo => "hide",
            Self::RemoveWorktree | Self::RemoveRepo => "delete",
        };
        format!("{}-{entry}", target.prefix())
    }

    /// Whether the entry opens something beside the menu rather than doing
    /// something itself.
    fn opens_submenu(self) -> bool {
        self == Self::SortBy
    }

    /// Whether the entry is one that throws something away: drawn in the
    /// destructive colour, like the file tree's own delete.
    fn is_destructive(self) -> bool {
        matches!(self, Self::RemoveWorktree | Self::RemoveRepo)
    }
}

pub struct Sidebar {
    state: Entity<AppState>,
    /// The readout, for what each worktree's terminals cost. A view of its own
    /// because it owns the sweep, and this is how its numbers reach a row.
    resources: Entity<ResourceBar>,
    /// The keyboard, for the keys the panel handles itself: Escape, to back out
    /// of the row menu.
    focus: FocusHandle,
    /// The width the drag handle set.
    width: WidthDrag,
    /// Re-renders on every `CoreEvent`, which is how a worktree created
    /// elsewhere (a refresh, a removal) reaches the list.
    _events: Subscription,
    /// And on every plain `notify` of the state, which is what a change with no
    /// event behind it looks like — a record being read back. `subscribe` only
    /// hears `emit`.
    _changes: Subscription,
    /// And on every sweep the readout makes, which is where a row's numbers come
    /// from.
    _resources: Subscription,
    /// The repositories whose worktrees are folded away. A view preference, so
    /// it lives with the view and not in `state.json` — the list is the same
    /// list either way.
    collapsed: HashSet<RepoId>,
    /// How the worktrees of each repository are ordered.
    sort: Sort,
    /// Whether worktrees with nothing working in them are left out of the list.
    hide_asleep: bool,
    /// The worktrees this window has been pointed at, newest first. What
    /// `Sort::Recent` orders by, and only for as long as the window is open.
    recent: Vec<String>,
    /// A row's menu, open on one worktree or on one repository.
    menu: Option<Menu>,
}

impl Sidebar {
    pub fn new(
        state: Entity<AppState>,
        resources: Entity<ResourceBar>,
        cx: &mut Context<Self>,
    ) -> Self {
        let events = cx.subscribe(
            &state,
            |_this, _state, _event: &syro_core::event::CoreEvent, cx| {
                cx.notify();
            },
        );
        let resources_changed = cx.observe(&resources, |_this, _resources, cx| cx.notify());
        let changes = cx.observe(&state, |this, state, cx| {
            // What "recent" means: the worktree the window was last pointed at,
            // newest first. Recorded here rather than in the state, because it is
            // about this window's list and nothing else reads it.
            if let Some(id) = state.read(cx).active_id().cloned() {
                if this.recent.first() != Some(&id) {
                    this.recent.retain(|recent| recent != &id);
                    this.recent.insert(0, id);
                }
            }
            cx.notify();
        });
        // The width a previous run was dragged to, or the default width.
        // `WidthDrag::new` clamps, so a width written by a wider window cannot
        // open this one wider than the panel is allowed to be.
        let widened = state.read(cx).sidebar_width().unwrap_or(WIDTH);
        Self {
            state,
            resources,
            focus: cx.focus_handle(),
            width: WidthDrag::new(widened, MIN_WIDTH, MAX_WIDTH),
            _events: events,
            _changes: changes,
            _resources: resources_changed,
            collapsed: HashSet::new(),
            sort: Sort::default(),
            hide_asleep: false,
            recent: Vec::new(),
            menu: None,
        }
    }

    /// How wide the panel is now. Only a test reads it today — a drag is proved
    /// by what it changed, and nothing else has an opinion about the number.
    #[cfg(test)]
    pub fn width(&self) -> f32 {
        self.width.width()
    }

    /// Follow the pointer while the drag handle is held. `None` is the release.
    fn resize_pointer(&mut self, pointer_x: Option<f32>, cx: &mut Context<Self>) {
        match pointer_x {
            // The sidebar's trailing edge follows the pointer: right is wider.
            Some(x) => {
                if self.width.move_to(x, 1.) {
                    cx.notify();
                }
            }
            None => {
                if self.width.dragging() {
                    self.width.end();
                    // Written when the drag ends rather than on every move: the
                    // file is what a restart reads, and a restart only ever sees
                    // where the drag stopped.
                    let width = self.width.width();
                    self.state
                        .update(cx, |state, cx| state.set_sidebar_width(width, cx));
                    cx.notify();
                }
            }
        }
    }

    /// Whether a worktree is asleep: nothing in it is working. An agent that is
    /// idle, one that is waiting for you, and a worktree with no terminals at all
    /// are all asleep — the difference is not one this list acts on.
    fn is_asleep(&self, worktree: &Worktree, cx: &App) -> bool {
        self.state.read(cx).status_of(&worktree.id) != Some(AgentStatus::Working)
    }

    /// Order one repository's worktrees as the chosen sort says. The repository's
    /// own checkout is not special here: a sort is a sort.
    fn sort_worktrees(&self, worktrees: &mut [Worktree], cx: &App) {
        match self.sort {
            // The order git reports, which is the order this panel has always
            // drawn: the repository's own checkout first.
            Sort::Project => {}
            Sort::Name => worktrees.sort_by(|a, b| branch_or_name(a).cmp(branch_or_name(b))),
            Sort::AgentActivity => worktrees
                .sort_by_key(|worktree| std::cmp::Reverse(self.activity_rank(worktree, cx))),
            // Both sorts are stable, so worktrees with the same rank keep the
            // order git gave them.
            Sort::Recent => worktrees.sort_by_key(|worktree| self.recent_rank(&worktree.id)),
        }
    }

    /// How much a worktree wants the reader: one waiting for you first, then one
    /// working, then anything else.
    fn activity_rank(&self, worktree: &Worktree, cx: &App) -> u8 {
        match self.state.read(cx).status_of(&worktree.id) {
            Some(AgentStatus::Permission) => 3,
            Some(AgentStatus::Working) => 2,
            Some(AgentStatus::Idle) => 1,
            None => 0,
        }
    }

    /// Where a worktree sits in this window's recent list: lower is more recent,
    /// and one that has never been opened is last.
    fn recent_rank(&self, worktree_id: &str) -> usize {
        self.recent
            .iter()
            .position(|id| id == worktree_id)
            .unwrap_or(usize::MAX)
    }

    /// Escape backs out of the row menu. Enter is a *click* on a focused
    /// element, so Escape is the only key that means "never mind".
    fn handle_escape(
        &mut self,
        event: &KeyDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.keystroke.key != "escape" {
            return;
        }
        if self.menu.take().is_some() {
            cx.notify();
        }
        cx.stop_propagation();
    }

    /// Drop the row menu, if one is open.
    fn close_menu(&mut self, cx: &mut Context<Self>) {
        if self.menu.take().is_some() {
            cx.notify();
        }
    }

    /// Fold a repository's worktrees away, or open them again.
    fn toggle_repo(&mut self, repo_id: &str, cx: &mut Context<Self>) {
        if !self.collapsed.remove(repo_id) {
            self.collapsed.insert(repo_id.to_string());
        }
        cx.notify();
    }

    /// Open a row's menu, at the point it was opened.
    fn open_menu(
        &mut self,
        target: Target,
        at: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.menu = Some(Menu {
            target,
            at,
            sort_open: false,
        });
        // The keyboard goes to the panel, so Escape reaches the handler that
        // closes this — a menu is a thing you can back out of.
        window.focus(&self.focus, cx);
        cx.notify();
    }

    /// Do what an entry says to what the menu is open on.
    fn run_menu_item(&mut self, item: MenuItem, window: &mut Window, cx: &mut Context<Self>) {
        // The submenu opens beside the menu rather than replacing it, so this one
        // entry leaves the menu up.
        if item.opens_submenu() {
            if let Some(menu) = self.menu.as_mut() {
                menu.sort_open = true;
            }
            cx.notify();
            return;
        }
        let Some(menu) = self.menu.take() else {
            return;
        };
        match item {
            // Handled above: this one opens a submenu.
            MenuItem::SortBy => {}
            MenuItem::ShowHidden => self.state.update(cx, |state, cx| state.show_all_repos(cx)),
            MenuItem::HideAsleep => self.hide_asleep = !self.hide_asleep,
            MenuItem::NewWorktree => {
                let Some(repo_id) = menu.target.repo_id() else {
                    return;
                };
                window.dispatch_action(
                    Box::new(worktree::New {
                        repo_id: repo_id.to_string(),
                    }),
                    cx,
                );
            }
            MenuItem::SharedPaths => {
                let Some(repo_id) = menu.target.repo_id() else {
                    return;
                };
                window.dispatch_action(
                    Box::new(crate::state::repo::EditSharedPaths {
                        repo_id: repo_id.to_string(),
                    }),
                    cx,
                );
            }
            MenuItem::HideRepo => {
                let Some(repo_id) = menu.target.repo_id() else {
                    return;
                };
                self.state
                    .update(cx, |state, cx| state.set_repo_hidden(repo_id, true, cx));
            }
            MenuItem::Reveal => {
                let Target::Worktree { worktree_id, .. } = &menu.target else {
                    return;
                };
                let path = self
                    .state
                    .read(cx)
                    .worktree(worktree_id)
                    .map(|worktree| worktree.path.clone());
                if let Some(path) = path {
                    crate::opener::reveal(&path);
                }
            }
            // The confirmation, not the removal: `worktree remove` on a dirty
            // worktree is the one thing the row cannot decide on its own.
            MenuItem::RemoveWorktree => {
                let Target::Worktree { worktree_id, .. } = &menu.target else {
                    return;
                };
                window.dispatch_action(
                    Box::new(worktree::AskRemove {
                        worktree_id: worktree_id.clone(),
                    }),
                    cx,
                );
            }
            // The same dialog the row's own `×` opens: a repository's own
            // checkout is the worktree git refuses to remove, so asking to
            // remove *it* is asking the project question.
            MenuItem::RemoveRepo => {
                let Some(repo_id) = menu.target.repo_id() else {
                    return;
                };
                let state = self.state.read(cx);
                let own = state
                    .repos()
                    .iter()
                    .find(|repo| repo.id == repo_id)
                    .and_then(|repo| {
                        state
                            .worktrees_of(repo_id)
                            .find(|worktree| worktree.path == repo.path)
                            .map(|worktree| worktree.id.clone())
                    });
                if let Some(worktree_id) = own {
                    window.dispatch_action(Box::new(worktree::AskRemove { worktree_id }), cx);
                }
            }
        }
        cx.notify();
    }

    /// The row menu, in a layer above the panel.
    fn menu_overlay(
        &self,
        menu: &Menu,
        window_size: Size<Pixels>,
        colors: &ColorTokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let this = cx.entity().downgrade();
        let dismiss = this.clone();
        let mut panel = div()
            .id("sidebar-menu")
            .test_support()
            .v_flex()
            .w(px(MENU_WIDTH))
            .p(px(MENU_PADDING))
            .gap(px(MENU_GAP))
            .rounded_md()
            .border_1()
            .border_color(colors.border)
            // Solid at any window opacity, like every menu: a list of entries
            // over the wallpaper is not the place to read it through.
            .bg(crate::settings_view::solid(colors.surface))
            // The whole panel is the menu, padding and the few pixels between
            // two entries included: a click there is a click on the menu, not on
            // the row it happens to be drawn over.
            .occlude()
            // The menu is above the list in a layer, so a wheel event would go to
            // the rows underneath it.
            .on_scroll_wheel(|_event, _window, cx| cx.stop_propagation());
        for item in MenuItem::for_target(&menu.target) {
            panel = panel.child(self.menu_item(*item, &menu.target, colors, cx));
        }

        deferred(
            // The scrim: a click anywhere outside the menu closes it, on either
            // button — which is what makes the menu feel like a menu.
            anchored().position(point(px(0.), px(0.))).child(
                div()
                    .id("sidebar-menu-scrim")
                    .test_support()
                    .w(window_size.width)
                    .h(window_size.height)
                    // Blocks the mouse from the rows behind it: a row under the
                    // menu is not hovered while the menu is up, and a click on one
                    // dismisses the menu instead of acting.
                    .occlude()
                    .on_mouse_down(MouseButton::Left, move |_event, _window, cx| {
                        this.update(cx, |sidebar, cx| sidebar.close_menu(cx)).ok();
                    })
                    .on_mouse_down(MouseButton::Right, move |_event, _window, cx| {
                        dismiss
                            .update(cx, |sidebar, cx| sidebar.close_menu(cx))
                            .ok();
                    })
                    .child(
                        anchored()
                            .position(menu.at)
                            .anchor(Anchor::TopLeft)
                            .snap_to_window_with_margin(px(8.))
                            .child(panel)
                            // The submenu, on the row that opened it and just past
                            // the menu's own edge: one a guess away from its row is
                            // a menu that looks detached from it.
                            .when(menu.sort_open, |el| {
                                el.child(
                                    anchored()
                                        .position(point(
                                            menu.at.x + px(MENU_WIDTH),
                                            menu.at.y
                                                + px(MENU_PADDING
                                                    + 2. * (MENU_ROW_HEIGHT + MENU_GAP)),
                                        ))
                                        .anchor(Anchor::TopLeft)
                                        .snap_to_window_with_margin(px(8.))
                                        .child(self.sort_menu(colors, cx)),
                                )
                            }),
                    ),
            ),
        )
        .with_priority(gpui_kit::base::POPUP_PRIORITY)
        .into_any_element()
    }

    /// The sort submenu: every order the list can be in, with the one it is in
    /// marked.
    fn sort_menu(&self, colors: &ColorTokens, cx: &mut Context<Self>) -> AnyElement {
        let mut panel = div()
            .id("sidebar-sort-menu")
            .test_support()
            .v_flex()
            .w(px(150.))
            .p(px(MENU_PADDING))
            .gap(px(MENU_GAP))
            .rounded_md()
            .border_1()
            .border_color(colors.border)
            .bg(crate::settings_view::solid(colors.surface))
            .shadow_lg()
            .occlude()
            .on_scroll_wheel(|_event, _window, cx| cx.stop_propagation());
        for sort in Sort::ALL {
            let chosen = self.sort == sort;
            panel = panel.child(
                div()
                    .id(SharedString::from(sort.id()))
                    .test_support()
                    .aria_label(sort.label())
                    .aria_selected(chosen)
                    .h_flex()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .h(px(MENU_ROW_HEIGHT))
                    .rounded_sm()
                    .text_xs()
                    .text_color(colors.foreground)
                    .cursor_pointer()
                    .hover(|el| el.bg(colors.muted))
                    .child(
                        // The dot the chosen one carries, and the same 6 px of
                        // nothing for the rest, so the names line up.
                        div()
                            .size(px(6.))
                            .flex_shrink_0()
                            .rounded_full()
                            .when(chosen, |el| el.bg(colors.primary)),
                    )
                    .child(SharedString::from(sort.label()))
                    .on_click(cx.listener(move |this, _event, _window, cx| {
                        this.sort = sort;
                        this.close_menu(cx);
                    })),
            );
        }
        panel.into_any_element()
    }

    /// One entry of a row menu.
    fn menu_item(
        &self,
        item: MenuItem,
        target: &Target,
        colors: &ColorTokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let this = cx.entity().downgrade();
        // What the entry says beside its name: the sort the list is in, and
        // whether the sleeping ones are hidden.
        let value = match item {
            MenuItem::SortBy => Some(self.sort.label()),
            MenuItem::HideAsleep => Some(
                if self.hide_asleep {
                    t!("common.on")
                } else {
                    t!("common.off")
                }
                .to_string(),
            ),
            _ => None,
        };
        div()
            .id(SharedString::from(item.id(target)))
            .test_support()
            .aria_label(item.label(target))
            .h_flex()
            .items_center()
            .gap_2()
            .px_2()
            .h(px(MENU_ROW_HEIGHT))
            .rounded_sm()
            .text_xs()
            .text_color(if item.is_destructive() {
                colors.destructive
            } else {
                colors.foreground
            })
            .cursor_pointer()
            .hover(|el| el.bg(colors.muted))
            .child(div().flex_1().min_w_0().child(item.label(target)))
            .when_some(value, |el, value| {
                el.child(
                    div()
                        .text_color(colors.muted_foreground)
                        .child(SharedString::from(value)),
                )
            })
            .when(item.opens_submenu(), |el| {
                el.child(
                    Icon::new(IconName::ChevronRight)
                        .size(px(12.))
                        .text_color(colors.muted_foreground),
                )
            })
            .on_click(move |_event, window, cx| {
                this.update(cx, |sidebar, cx| sidebar.run_menu_item(item, window, cx))
                    .ok();
            })
            .into_any_element()
    }

    /// One worktree row: the branch, what its agents are doing, and one line per
    /// terminal under it with what that terminal costs. The active one is marked
    /// with an accent background.
    fn row(&self, worktree: Worktree, active: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = Theme::global(cx).tokens.colors.clone();
        // What the worktree's terminals cost, from the readout's last sweep. A
        // worktree with nothing running in it has no numbers and draws none, and
        // a sweep that has not answered yet is the same nothing.
        let usage = self
            .resources
            .read(cx)
            .worktree_usage(&worktree.id)
            .filter(|usage| !usage.terminals.is_empty());
        let id = worktree.id.clone();
        let for_menu = worktree.id.clone();
        let for_repo = worktree.repo_id.clone();
        // Read here rather than in the listener: the menu's word for "remove"
        // depends on it, and the listener cannot borrow the panel.
        let own_checkout = self.state.read(cx).is_own_checkout(&worktree.id);
        // The branch name is the whole line: what the agents are doing and what
        // they cost are on the terminal lines under it, so a status and a total
        // here would only say the same thing twice.
        let mut card = div()
            .id(SharedString::from(format!("worktree-{}", worktree.id)))
            .test_support()
            // The row as one thing, and the one line it draws: the branch, or the
            // worktree's own name where there is no branch.
            .aria_label(SharedString::from(branch_or_name(&worktree).to_string()))
            // A card, not a band: the wrapper below insets it from the panel's
            // edges, so the highlight stops short instead of running under the
            // rows beside it.
            .w_full()
            .px_2()
            .py_1p5()
            // `md` rather than `lg`: a wide radius on a short card reads as a
            // pill, and this row is a list item.
            .rounded_md()
            // The card is always here — the row on screen fills it and gives it an edge — so that marking
            // one never moves a name by a pixel.
            .border_1()
            .border_color(if active {
                colors.foreground.alpha(0.18)
            } else {
                colors.foreground.alpha(0.)
            })
            .cursor_pointer()
            // Washes of the text colour rather than of a token: a dark palette
            // has one grey for every surface, so a hover has to be built from
            // the two ends of the scale to stay visible in both themes.
            .when(active, |el| el.bg(colors.foreground.alpha(0.08)))
            .hover(|el| el.bg(colors.foreground.alpha(0.04)))
            .child(
                div()
                    .h_flex()
                    // No indent of its own: the card's `px_2` is the row's whole
                    // inset, so the hover is padded the same on both sides.
                    .gap_1p5()
                    .items_center()
                    .text_sm()
                    .text_color(if active {
                        colors.accent_foreground
                    } else {
                        colors.foreground
                    })
                    // The branch, and only the branch: the worktree's own name is
                    // the repository's for its own checkout and a slug of the
                    // branch for one this app made, so a second line repeated
                    // either the heading above it or the line beside it. A
                    // checkout on no branch has no branch to draw, and falls back
                    // to that name.
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .font_weight(FontWeight::MEDIUM)
                            .child(SharedString::from(branch_or_name(&worktree).to_string())),
                    )
                    // What the columns under this branch are, on the branch's own
                    // line: a terminal's word, its two numbers and its length are
                    // read as columns, and a worktree with nothing running in it
                    // has none to name.
                    .when_some(usage.as_ref(), |line, _| {
                        line.child(column_labels(&worktree.id, &colors))
                    }),
            );

        // One line per terminal: the rows the readout used to list in a panel of
        // its own, a step under the worktree's own numbers.
        if let Some(usage) = &usage {
            let mut terminals = div().v_flex().w_full();
            for row in &usage.terminals {
                terminals = terminals.child(terminal_line(&worktree.id, row, &colors));
            }
            card = card.child(terminals);
        }

        let card = card
            // The row's menu, on the right button. What the row *is* is the
            // left-click; everything that can be done to it is this, which is
            // what every file manager does and what leaves the row itself
            // unwritten-over.
            .on_aux_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                let at = match event {
                    ClickEvent::Mouse(mouse) => mouse.up.position,
                    ClickEvent::Touch(touch) => touch.position,
                    // A keyboard-generated click has no place to open a menu at,
                    // and opening one in a corner is worse than not opening it.
                    ClickEvent::Keyboard(_) => return,
                };
                this.open_menu(
                    Target::Worktree {
                        worktree_id: for_menu.clone(),
                        repo_id: for_repo.clone(),
                        own_checkout,
                    },
                    at,
                    window,
                    cx,
                );
            }))
            .on_click(cx.listener(move |_this, _event: &ClickEvent, window, cx| {
                // The action, not the state: this is the same path a
                // keybinding would take, and the handler that acts on it
                // lives on the root view.
                window.dispatch_action(
                    Box::new(worktree::Activate {
                        worktree_id: id.clone(),
                    }),
                    cx,
                );
            }));

        // The margin that keeps the highlight off the panel's edges is this
        // wrapper's padding: a `w_full` card with a margin of its own would
        // overflow the row it sits in.
        div().w_full().px_1p5().child(card)
    }
}

/// The names of the columns a worktree's terminal rows fill, drawn on the branch's
/// own line — which is the header those rows hang under. Each label keeps the
/// width its column is drawn at below, so a name sits over the column it names
/// rather than beside it.
fn column_labels(worktree_id: &str, colors: &ColorTokens) -> AnyElement {
    // The word a row draws first, before the length and the numbers, in the
    // column the row keeps even when it has no word: the name beside it stops at
    // this column's left edge either way.
    let word = div()
        .id(SharedString::from(format!(
            "terminal-columns-status-{worktree_id}"
        )))
        .test_support()
        .min_w(px(STATUS_MAX_WIDTH))
        .text_right()
        .child(t!("resource.column_status").to_string());
    let number = |column: &str, width: f32, name: String| {
        div()
            .id(SharedString::from(format!(
                "terminal-columns-{column}-{worktree_id}"
            )))
            .test_support()
            .min_w(px(width))
            .text_right()
            .child(name)
    };
    div()
        .id(SharedString::from(format!(
            "terminal-columns-{worktree_id}"
        )))
        .test_support()
        .aria_label(SharedString::from(format!(
            "{} {} {} {}",
            t!("resource.column_status"),
            t!("resource.column_uptime"),
            t!("resource.column_cpu"),
            t!("resource.column_rss")
        )))
        .h_flex()
        .items_center()
        .gap_1p5()
        .flex_shrink_0()
        // The numbers' own size rather than the branch's: these are the headings
        // of the small print under them.
        .text_xs()
        .text_color(colors.muted_foreground)
        .child(word)
        // The length sits beside the word it belongs with; the two numbers
        // follow. This order is the order the rows below draw in.
        .child(number(
            "uptime",
            UPTIME_WIDTH,
            t!("resource.column_uptime").to_string(),
        ))
        .child(number(
            "cpu",
            CPU_WIDTH,
            t!("resource.column_cpu").to_string(),
        ))
        .child(number(
            "mem",
            MEM_WIDTH,
            t!("resource.column_rss").to_string(),
        ))
        .into_any_element()
}

/// The two numbers a row ends in, in the columns every row keeps. Labelled for
/// the screen reader as one line, because two numbers in two different units are
/// one reading.
fn row_metrics(id: SharedString, cpu_pct: f32, rss_bytes: u64, colors: &ColorTokens) -> AnyElement {
    div()
        .id(id)
        .test_support()
        .aria_label(SharedString::from(format!(
            "{cpu_pct:.1}% · {}",
            format_bytes(rss_bytes)
        )))
        .h_flex()
        .items_center()
        .gap_1p5()
        .flex_shrink_0()
        // A step down from the name they belong to: the numbers are the row's
        // footnote, and the smaller size is what keeps them off the name.
        .text_xs()
        .child(
            div()
                .min_w(px(CPU_WIDTH))
                .text_right()
                .text_color(colors.muted_foreground)
                .child(SharedString::from(format!("{cpu_pct:.0}%"))),
        )
        .child(
            div()
                .min_w(px(MEM_WIDTH))
                .text_right()
                .child(SharedString::from(format_bytes(rss_bytes))),
        )
        .into_any_element()
}

/// One terminal under a worktree: what its agent is doing, what it is called, and
/// what it costs. Its numbers are in the same columns as the worktree's own,
/// because they are the same two numbers a step down.
fn terminal_line(worktree_id: &str, row: &Row, colors: &ColorTokens) -> AnyElement {
    // Empty before the agent has worked: the column stays, so the numbers do not move.
    let uptime = row.uptime.map(format_uptime).unwrap_or_default();
    div()
        .id(SharedString::from(format!(
            "terminal-{worktree_id}-{}",
            row.title
        )))
        .test_support()
        .aria_label(SharedString::from(format!(
            "{}: {uptime} · {:.1}% · {}",
            row.title,
            row.cpu_pct,
            format_bytes(row.rss_bytes)
        )))
        .h_flex()
        .items_center()
        .gap_1p5()
        .h(px(18.))
        .flex_shrink_0()
        .child(
            div()
                .id(SharedString::from(format!(
                    "terminal-name-{worktree_id}-{}",
                    row.title
                )))
                .test_support()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_xs()
                .text_color(colors.muted_foreground)
                .child(SharedString::from(row.title.clone())),
        )
        // The word belongs to the columns the line ends in, under the branch's
        // own `状态` label: drawn before the name it would sit under the branch
        // instead. The column keeps its width whether or not there is a word, so
        // a shell's title stops at the column rather than running through it.
        .child(
            div()
                .min_w(px(STATUS_MAX_WIDTH))
                .flex_shrink_0()
                .h_flex()
                .justify_end()
                .children(status_word(
                    SharedString::from(format!("terminal-status-{worktree_id}-{}", row.title)),
                    row.status,
                    row.stalled,
                    row.exit,
                    row.busy,
                    colors,
                )),
        )
        // The length sits beside the word, ahead of the numbers, matching the
        // header above it.
        .child(
            div()
                .id(SharedString::from(format!(
                    "terminal-uptime-{worktree_id}-{}",
                    row.title
                )))
                .test_support()
                .aria_label(SharedString::from(uptime.clone()))
                // The one column that moves on its own, so it has a width of its
                // own rather than letting the numbers beside it shift every tick.
                .min_w(px(UPTIME_WIDTH))
                .flex_shrink_0()
                .text_xs()
                .text_right()
                .text_color(colors.muted_foreground)
                .child(SharedString::from(uptime)),
        )
        .child(row_metrics(
            SharedString::from(format!("terminal-metrics-{worktree_id}-{}", row.title)),
            row.cpu_pct,
            row.rss_bytes,
            colors,
        ))
        .into_any_element()
}

/// How long a terminal has been running, in what a row has room for: seconds
/// under a minute, whole minutes under an hour, and hours and minutes past that.
fn format_uptime(uptime: std::time::Duration) -> String {
    let seconds = uptime.as_secs();
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m", seconds / 60)
    } else {
        format!("{}h{:02}m", seconds / 3600, (seconds % 3600) / 60)
    }
}

/// What a row says, before it is coloured and drawn. The order is what decides
/// it: how the child ended, then what its agent said, then whether a job owns
/// the terminal's tty.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Word {
    Done,
    Failed,
    Stalled,
    Waiting,
    Working,
    Idle,
    Running,
}

/// The status a row shows: what its agent is doing, the two only a terminal can
/// have — `Done` and `Failed`, which are its child's exit — and `Running` for a
/// terminal whose child reported nothing while a job runs in its tty. `None` is a
/// plain shell with nothing to report, which draws no word at all.
fn word_of(
    exit: Option<bool>,
    status: Option<AgentStatus>,
    stalled: bool,
    busy: bool,
) -> Option<Word> {
    match (exit, status, stalled) {
        (Some(true), ..) => Some(Word::Done),
        (Some(false), ..) => Some(Word::Failed),
        (None, Some(AgentStatus::Working), true) => Some(Word::Stalled),
        (None, Some(AgentStatus::Permission), _) => Some(Word::Waiting),
        (None, Some(AgentStatus::Working), false) => Some(Word::Working),
        (None, Some(AgentStatus::Idle), _) => Some(Word::Idle),
        // Nothing reported, and the tty belongs to something else: a shell
        // running an agent, a build, an editor — something this app was not told
        // about, which is all the row can say about it.
        (None, None, _) if busy => Some(Word::Running),
        (None, None, _) => None,
    }
}

/// The status a row shows, in one colour and one word: what its agent is doing,
/// plus the two only a terminal can have — `Done` and `Failed`, which are its
/// child's exit. The label stays the sentence the status is; the drawn word is
/// the shorthand, because the column is narrow. `None` is a plain shell with
/// nothing to report, which draws no word at all.
fn status_word(
    id: SharedString,
    status: Option<AgentStatus>,
    stalled: bool,
    exit: Option<bool>,
    busy: bool,
    colors: &ColorTokens,
) -> Option<AnyElement> {
    let said = word_of(exit, status, stalled, busy)?;
    let (colour, label, short) = match said {
        Word::Done => {
            let said = t!("status.done_short");
            (colors.muted_foreground, said.clone(), said)
        }
        Word::Failed => {
            let said = t!("status.failed_short");
            (colors.destructive, said.clone(), said)
        }
        Word::Stalled => (
            colors.muted_foreground,
            t!("status.stalled"),
            t!("status.stalled_short"),
        ),
        Word::Waiting => (
            colors.destructive,
            t!("status.waiting"),
            t!("status.waiting_short"),
        ),
        Word::Working => (
            running_green(),
            t!("status.working"),
            t!("status.working_short"),
        ),
        Word::Idle => (
            colors.muted_foreground,
            t!("status.idle"),
            t!("status.idle_short"),
        ),
        // Not the agent's colour and no dot: an agent at work is what those are
        // for, and this is only something running.
        Word::Running => (
            colors.muted_foreground,
            t!("status.running"),
            t!("status.running_short"),
        ),
    };
    // Only `Working` breathes: it is the one word that means an agent is on a task
    // this moment.
    let running = said == Word::Working;
    div()
        .id(id)
        .test_support()
        .role(Role::Status)
        .aria_label(label)
        .max_w(px(STATUS_MAX_WIDTH))
        .flex_shrink_0()
        .h_flex()
        .gap_1()
        .text_xs()
        .text_color(colour)
        .when(running, |row| row.child(breathing_dot(colour)))
        .child(div().min_w_0().truncate().child(short))
        .into_any_element()
        .into()
}

/// The colour of an agent at work: green reads as "going" on both themes, which
/// the palette's primary (black or white) does not.
fn running_green() -> Hsla {
    hsla(142. / 360., 0.71, 0.45, 1.)
}

/// A dot that fades in and out, so a running row is alive at a glance.
fn breathing_dot(colour: Hsla) -> impl IntoElement {
    div()
        .size(px(6.))
        .rounded_full()
        .flex_shrink_0()
        .bg(colour)
        .with_animation(
            "breathing",
            Animation::new(std::time::Duration::from_millis(1600))
                .repeat()
                .with_easing(bounce(ease_in_out)),
            |dot, delta| dot.opacity(1. - delta * 0.7),
        )
}

impl Render for Sidebar {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // This frame's drag listeners, registered while the frame is painted:
        let tracker_view = cx.entity().downgrade();
        let tracker = canvas(
            |_bounds, _window, _cx| (),
            move |_bounds, _state, window, _cx| {
                resize::follow_pointer(window, &tracker_view, |this, pointer, cx| {
                    this.resize_pointer(pointer.map(|position| position.x.as_f32()), cx)
                });
            },
        )
        .absolute()
        .top(px(0.))
        .left(px(0.))
        .size_full();

        let colors = Theme::global(cx).tokens.colors.clone();
        let active = self.state.read(cx).active_id().cloned();
        let state_error = self.state.read(cx).state_error().map(str::to_string);
        let hook_error = self.state.read(cx).hook_error().map(str::to_string);
        let repos = self.state.read(cx).repos().to_vec();

        // The list is the only part that scrolls: the header and the filter are
        // chrome and stay where they are.
        let mut list = div()
            .id("sidebar-list")
            .test_support()
            .v_flex()
            .flex_1()
            .min_h_0()
            .relative()
            .w_full()
            // The rows are cards, so the panel's own edges are not theirs: this
            // is the gutter they float in. Repositories are further apart than
            // the worktrees inside one, which is what makes a group a group.
            .px_1()
            .py_2()
            .gap_2()
            .overflow_y_scroll();

        if let Some(error) = state_error {
            // Two repos can each have an error, so the id is the repo's and the
            // window's is a word a repo id cannot be.
            list = list.child(self.banner("state", error, &colors));
        }

        // A `[[hook]]` that failed.
        if let Some(error) = hook_error {
            list = list.child(self.banner("hook", error, &colors));
        }

        if repos.is_empty() {
            // The affordance overlay for an empty list: centred, an icon, a label, a description — and it *is* the way in.
            list = list.child(
                div()
                    .id("repo-empty")
                    .test_support()
                    .aria_label(t!("sidebar.empty_add"))
                    .absolute()
                    .top(px(8.))
                    .bottom(px(8.))
                    .left(px(8.))
                    .right(px(8.))
                    .v_flex()
                    .items_center()
                    .justify_center()
                    .gap_1()
                    .rounded_md()
                    .border_1()
                    .border_color(colors.border)
                    .bg(colors.background)
                    .p_4()
                    .text_center()
                    .cursor_pointer()
                    .hover(|el| el.bg(colors.muted))
                    .child(div().child(Icon::new(IconName::Plus).size(px(20.))))
                    .child(div().text_sm().child(t!("sidebar.no_repositories")))
                    .child(
                        div()
                            .text_xs()
                            .text_color(colors.muted_foreground)
                            .child(t!("sidebar.add_hint")),
                    )
                    .on_click(|_event, window, cx| {
                        window.dispatch_action(Box::new(crate::state::repo::AskAdd), cx);
                    }),
            );
        }

        // Read once: a hidden repository is not drawn at all, and what is left of
        // the list is what the count at the bottom is about.
        let hidden = self.state.read(cx).hidden_repos().to_vec();
        for repo in repos {
            if hidden.contains(&repo.id) {
                continue;
            }
            let worktrees: Vec<Worktree> = self
                .state
                .read(cx)
                .worktrees_of(&repo.id)
                .cloned()
                .collect();
            let error = self.state.read(cx).error_for(&repo.id).map(str::to_string);
            let pending = self
                .state
                .read(cx)
                .pending_for(&repo.id)
                .map(str::to_string);
            let mut matching: Vec<Worktree> = worktrees
                .into_iter()
                .filter(|worktree| {
                    let is_active = active.as_deref() == Some(worktree.id.as_str());
                    // A sleeping worktree is one nothing is working in, and
                    // hiding those is all that setting does. The one on screen
                    // stays: a list that hides what you are looking at looks
                    // broken.
                    is_active || !self.hide_asleep || !self.is_asleep(worktree, cx)
                })
                .collect();
            self.sort_worktrees(&mut matching, cx);
            // A repository with nothing left to show is not a heading with nothing
            // under it: hiding the sleeping ones narrows the list rather than
            // annotating it.
            if matching.is_empty() && self.hide_asleep && error.is_none() && pending.is_none() {
                continue;
            }

            // Folded away: the repository is its heading and nothing under it.
            let folded = self.collapsed.contains(&repo.id);

            let mut group = div()
                .id(SharedString::from(format!("repo-{}", repo.id)))
                .test_support()
                .v_flex()
                .gap_1()
                .child(
                    div()
                        .id(SharedString::from(format!("repo-heading-{}", repo.id)))
                        .test_support()
                        .h_flex()
                        .items_center()
                        .justify_between()
                        .h(px(24.))
                        .px_2()
                        .rounded_sm()
                        .cursor_pointer()
                        // A wash of the text colour rather than of a token: a dark
                        // palette has one grey for every surface, and the heading
                        // is drawn on two of them.
                        .hover(|el| el.bg(colors.foreground.alpha(0.04)))
                        // The whole heading folds the repository away, which is
                        // what a heading does in every file manager.
                        .on_click({
                            let repo_id = repo.id.clone();
                            cx.listener(move |this, _event, _window, cx| {
                                this.toggle_repo(&repo_id, cx)
                            })
                        })
                        .child(
                            div()
                                .h_flex()
                                .items_center()
                                .gap_1p5()
                                .flex_1()
                                .min_w_0()
                                // The repository is what the rows under it belong
                                // to, so it reads as a heading rather than as a
                                // fourth worktree: a folder, and the name a step
                                // down and unweighted. The folder is also the fold:
                                // closed while the worktrees are away.
                                .child(
                                    Icon::new(if folded {
                                        IconName::Folder
                                    } else {
                                        IconName::FolderOpen
                                    })
                                    .size(px(14.)),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(colors.foreground)
                                        .truncate()
                                        .child(SharedString::from(repo.name.clone())),
                                ),
                        )
                        .child(
                            // The `...`: everything that can be done to the
                            // repository. It does not fold the heading — the click
                            // is the button's, and it stops the heading's own
                            // click from also firing.
                            div()
                                .id(SharedString::from(format!("repo-menu-{}", repo.id)))
                                .test_support()
                                .aria_label(t!("sidebar.repo_menu"))
                                .h_flex()
                                .items_center()
                                .justify_center()
                                // The same 20 px box as the header's buttons: two
                                // actions, one size, lining up down the right edge
                                // of the panel.
                                .size(px(20.))
                                .rounded_sm()
                                .cursor_pointer()
                                .text_color(colors.muted_foreground)
                                .hover(|el| el.bg(colors.muted).text_color(colors.foreground))
                                .child(Icon::new(IconName::Ellipsis).size(px(14.)))
                                .on_click({
                                    let repo_id = repo.id.clone();
                                    cx.listener(move |this, event: &ClickEvent, window, cx| {
                                        cx.stop_propagation();
                                        let at = match event {
                                            ClickEvent::Mouse(mouse) => mouse.up.position,
                                            ClickEvent::Touch(touch) => touch.position,
                                            // A keyboard-generated click has no
                                            // place to open a menu at.
                                            ClickEvent::Keyboard(_) => return,
                                        };
                                        this.open_menu(
                                            Target::Repo {
                                                repo_id: repo_id.clone(),
                                            },
                                            at,
                                            window,
                                            cx,
                                        );
                                    })
                                }),
                        ),
                );

            // Folded away, the repository is the heading and nothing under it. A
            // filter opens it: a search that hides its own match is a search that
            // looks broken.
            if !folded {
                for worktree in matching {
                    let is_active = active.as_deref() == Some(worktree.id.as_str());
                    group = group.child(self.row(worktree, is_active, cx));
                }
            }

            if let Some(label) = pending {
                group = group.child(
                    div()
                        .id(SharedString::from(format!("pending-{}", repo.id)))
                        .test_support()
                        .px_2()
                        .text_xs()
                        .text_color(colors.muted_foreground)
                        .child(SharedString::from(label)),
                );
            }
            if let Some(error) = error {
                group = group.child(self.banner(&repo.id, error, &colors));
            }
            list = list.child(group);
        }

        let mut panel = div()
            .id("sidebar")
            .test_support()
            .relative()
            .v_flex()
            .w(px(self.width.width()))
            .h_full()
            .flex_shrink_0()
            .bg(colors.surface)
            .border_r_1()
            // One line at the seam, and it is the panel's own border: the drag handle draws none of its
            // own, because a hairline three pixels inside this one was two lines at every edge.
            .border_color(if self.width.dragging() {
                colors.primary
            } else {
                colors.border
            })
            // Escape backs out of the name being typed and of the row menu. On
            // the panel's root rather than on the field: a key goes to the
            // focused element and bubbles from there, and the field is the
            // focused element.
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::handle_escape))
            .child(tracker)
            .child(self.header(&colors, cx))
            .child(list)
            .child(self.resize_handle(&colors, cx));

        // The row menu, in a layer above the panel: the rows scroll, so a menu
        // drawn inside them would be clipped at the panel's edge.
        if let Some(menu) = self.menu.clone() {
            panel = panel.child(self.menu_overlay(&menu, window.bounds().size, &colors, cx));
        }
        panel
    }
}

impl Sidebar {
    /// The header row: the global search on the left, drawn as the field it opens,
    /// and the two actions that belong to the panel as a whole on the right.
    fn header(&self, colors: &ColorTokens, cx: &mut Context<Self>) -> AnyElement {
        div()
            .id("sidebar-header")
            .test_support()
            .h_flex()
            .items_center()
            .gap_1()
            // The field's box starts where the row cards' column begins, 12 px in:
            // the list's own gutter plus the margin a card keeps. The buttons keep
            // that same 12 px on the right, so both edges of the panel line up
            // with the search box rather than the search box reaching past them.
            .px_3()
            .h(px(HEADER_HEIGHT))
            .child(self.search_input(colors))
            .child(self.header_button(
                "repo-add",
                &t!("sidebar.add_repository"),
                // A plus: the action is adding a repository, whatever the
                // platform's picker then asks for.
                IconName::Plus,
                colors,
                |window, cx| {
                    // The Add-project dialog's three doors, one of which is
                    // the platform's own folder picker.
                    window.dispatch_action(Box::new(crate::state::repo::AskAdd), cx);
                },
            ))
            // The panel's own menu: the things that are about the whole list —
            // what is hidden, and how it is ordered.
            .child(
                div()
                    .id("sidebar-menu-open")
                    .test_support()
                    .aria_label(t!("sidebar.menu"))
                    .h_flex()
                    .items_center()
                    .justify_center()
                    .size(px(20.))
                    .rounded_sm()
                    .cursor_pointer()
                    .text_color(colors.muted_foreground)
                    .hover(|el| el.bg(colors.muted).text_color(colors.foreground))
                    .child(Icon::new(IconName::Ellipsis).size(px(14.)))
                    .on_click(cx.listener(|this, event: &ClickEvent, window, cx| {
                        let at = match event {
                            ClickEvent::Mouse(mouse) => mouse.up.position,
                            ClickEvent::Touch(touch) => touch.position,
                            // A keyboard-generated click has no place to open a
                            // menu at.
                            ClickEvent::Keyboard(_) => return,
                        };
                        this.open_menu(Target::Sidebar, at, window, cx);
                    })),
            )
            .into_any_element()
    }

    /// The global search, drawn as the field it opens: a search icon, the hint in
    /// small muted type, and a click anywhere in the box that opens the search
    /// overlay. The text is typed there, not here.
    fn search_input(&self, colors: &ColorTokens) -> AnyElement {
        div()
            .id("sidebar-search")
            .test_support()
            .aria_label(t!("sidebar.search"))
            .h_flex()
            .items_center()
            .gap_1p5()
            .flex_1()
            .min_w_0()
            .h(px(24.))
            .px_2()
            .rounded_md()
            // A wash of the text colour, like the worktree cards below: the box
            // sits *on* the panel, and `colors.background` here is the window's
            // own fill, so a see-through window drew it a layer denser than the
            // panel it lies on — a near-solid black slab.
            .bg(colors.foreground.alpha(0.04))
            .border_1()
            .border_color(colors.input)
            .cursor_pointer()
            .text_color(colors.muted_foreground)
            .hover(|el| el.bg(colors.foreground.alpha(0.08)))
            .child(Icon::new(IconName::Search).size(px(14.)).flex_shrink_0())
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .text_ellipsis()
                    .text_xs()
                    .child(t!("palette.search_all_hint")),
            )
            .on_click(move |_event, window, cx| {
                window.dispatch_action(
                    Box::new(crate::palette::Open {
                        kind: crate::palette::Kind::All,
                    }),
                    cx,
                );
            })
            .into_any_element()
    }

    /// One button in the header.
    fn header_button(
        &self,
        id: &'static str,
        label: &str,
        icon: IconName,
        colors: &ColorTokens,
        on_click: impl Fn(&mut Window, &mut App) + 'static,
    ) -> AnyElement {
        div()
            .id(id)
            .test_support()
            .aria_label(label)
            .h_flex()
            .items_center()
            .justify_center()
            .size(px(20.))
            .rounded_sm()
            .cursor_pointer()
            .text_color(colors.muted_foreground)
            .hover(|el| el.bg(colors.muted))
            .child(Icon::new(icon).size(px(14.)))
            .on_click(move |_event, window, cx| on_click(window, cx))
            .into_any_element()
    }

    /// The drag handle on the trailing edge.
    fn resize_handle(&self, colors: &ColorTokens, cx: &mut Context<Self>) -> AnyElement {
        div()
            .id("sidebar-resize")
            .test_support()
            .aria_label(t!("sidebar.resize"))
            .absolute()
            .top(px(0.))
            .right(px(0.))
            .h_full()
            .w(px(6.))
            .flex()
            .items_center()
            .justify_center()
            .cursor_col_resize()
            .hover(|el| el.bg(colors.border))
            // No line of its own: the panel's own trailing border is the seam, and a hairline here sits
            // three pixels inside it — two lines at every edge.
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, _window, cx| {
                    this.width.begin(event.position.x.as_f32());
                    cx.notify();
                }),
            )
            .into_any_element()
    }
}

impl Sidebar {
    /// A one-line complaint from git or from the store. Not a dialog: these are
    /// recoverable and belong next to the thing they are about.
    fn banner(&self, id: &str, message: String, colors: &ColorTokens) -> impl IntoElement {
        div()
            .id(SharedString::from(format!("banner-{id}")))
            .test_support()
            // The message is the element's accessible name: a screen reader
            // reads it, and a test reads it back through the same door rather
            // than through a second copy kept for tests.
            .aria_label(message.clone())
            .px_2()
            .py_1()
            .rounded_md()
            .text_xs()
            .bg(colors.muted)
            .text_color(colors.destructive)
            .child(SharedString::from(message))
    }
}

/// The sidebar in a real (headless) window, driven the way a user drives it:
#[cfg(test)]
mod window_tests {
    use std::path::{Path, PathBuf};

    use gpui_kit::base::{Root, StyledExt as _, Theme};
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{
        App, AppContext, Bounds, Context, Entity, IntoElement, ParentElement, Pixels, Point,
        Render, SharedString, Styled, TestAppContext, Window, WindowBounds, WindowHandle,
        WindowOptions, div, px, size,
    };
    use syro_core::config::Settings;
    use syro_core::model::{AgentSpec, AgentStatus, AgentStatusRule, HookSpec, State};
    use syro_core::store::Store;

    use super::MAX_WIDTH;
    use crate::Workspace;
    use crate::state::fixture::{scratch_repo, temp_store};
    use crate::state::{AppState, register_actions, tab, terminal, worktree};

    struct Fixture {
        window: WindowHandle<Root>,
        state: Entity<AppState>,
        /// The root view, kept for the one assertion that needs it: a drag is
        /// only proved by the width the panel ended up with.
        workspace: Entity<Workspace>,
        /// The path, not a `Store`: read it back by making one.
        state_path: PathBuf,
        worktrees: Vec<PathBuf>,
        _dir: PathBuf,
        _store_dir: PathBuf,
    }

    /// A window at 900x500 with the scratch repo loaded from a scratch store.
    fn open(cx: &mut TestAppContext, tag: &str) -> Fixture {
        open_with(cx, tag, Settings::default())
    }

    /// The same window with settings: a preset is where the status rules come
    /// from, so a test that drives an agent has to say which one.
    fn open_with(cx: &mut TestAppContext, tag: &str, settings: Settings) -> Fixture {
        open_seeded(cx, tag, settings, true, |_| {})
    }

    /// The same window over a `state.json` this test wrote: what a restart finds
    /// is the file, so a test about a restart has to write one.
    fn open_from(cx: &mut TestAppContext, tag: &str, state: impl FnOnce(&mut State)) -> Fixture {
        open_seeded(cx, tag, Settings::default(), true, state)
    }

    /// A window on the state a first run has: no repository yet, which is the
    /// screen the header's first button exists for.
    fn open_empty(cx: &mut TestAppContext, tag: &str) -> Fixture {
        open_seeded(cx, tag, Settings::default(), false, |_| {})
    }

    fn open_seeded(
        cx: &mut TestAppContext,
        tag: &str,
        settings: Settings,
        with_repo: bool,
        state: impl FnOnce(&mut State),
    ) -> Fixture {
        let (dir, repo, worktrees) = scratch_repo(tag);
        let (state_path, store_dir) = temp_store(tag);
        let mut saved = State {
            repos: if with_repo { vec![repo] } else { Vec::new() },
            ..Default::default()
        };
        state(&mut saved);
        Store::new(state_path.clone()).save(&saved).unwrap();

        let mut built: Option<Entity<AppState>> = None;
        // The root view is not held below: the window's `Root` owns it, and that the clicks in these
        // tests reach the listeners at all is the proof.
        let (window, workspace) = cx.update(|cx| {
            // The component layer's theme global comes from `init`, and a chrome widget that reads it
            // panics without this.
            gpui_kit::init(cx);
            let bounds = Bounds {
                origin: Point::default(),
                size: size(px(900.), px(500.)),
            };
            let (window, workspace) = gpui_kit::open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    ..Default::default()
                },
                cx,
                |_, cx| {
                    let state = cx.new(|cx| {
                        AppState::open(Store::new(state_path.clone()), dir.join("workspaces"), cx)
                            .with_settings(settings)
                    });
                    register_actions(&state, cx);
                    let handle = cx.new(|cx| Workspace::new(cx, state.clone()));
                    // The workspace's own action, registered the way `main` registers it: a `terminal::New`
                    // dispatched here has no other listener, and a test against a window nobody
                    // ships proves nothing.
                    cx.on_action({
                        let handle = handle.clone();
                        move |action: &terminal::New, cx: &mut App| {
                            handle.update(cx, |workspace, cx| workspace.start_terminal(action, cx));
                        }
                    });
                    cx.on_action({
                        let handle = handle.clone();
                        move |action: &tab::Close, cx: &mut App| {
                            handle.update(cx, |workspace, cx| {
                                workspace.close_tab(&action.worktree_id, action.index, cx)
                            });
                        }
                    });
                    built = Some(state);
                    handle
                },
            )
            .expect("open test window");
            (window.downcast::<Root>().expect("Base Root"), workspace)
        });
        let mut fixture = Fixture {
            window,
            state: built.expect("the state was built"),
            workspace,
            state_path,
            worktrees,
            _dir: dir,
            _store_dir: store_dir,
        };
        draw(cx, &fixture);
        fixture.worktrees.sort();
        fixture
    }

    fn draw(cx: &mut TestAppContext, fixture: &Fixture) {
        cx.update_window(fixture.window.into(), |_, window, cx| {
            window.draw(cx).clear(cx);
        })
        .expect("draw a frame");
    }

    /// Draw frames until `done`, the same way a real event loop would: an
    /// action is dispatched on the next cycle, and a git call lands after the
    /// background executor has run.
    fn draw_until(
        cx: &mut TestAppContext,
        fixture: &Fixture,
        done: impl Fn(&mut TestAppContext) -> bool,
    ) -> bool {
        // The budget is wall clock, not a frame count.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while std::time::Instant::now() < deadline {
            // The terminal's repaint loop is a 16 ms timer, and a test's clock
            // only moves when it is told to. The git calls these tests wait on
            // are background tasks and need the pump, not the clock.
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

    /// Put the app in front of the user, which is half of what decides whether a
    /// status is worth a notification. A window a test never activated is a
    /// window nobody is looking at.
    fn focus(cx: &mut TestAppContext, fixture: &Fixture) {
        cx.update_window(fixture.window.into(), |_, window, _| {
            window.activate_window()
        })
        .expect("activate the window");
        assert!(
            cx.update(|cx| cx.active_window().is_some()),
            "the test platform reports no focused window"
        );
    }

    /// Click an element by id, the way a user does: the click reaches the
    /// listeners the frame registered, not the view directly.
    fn click(cx: &mut TestAppContext, fixture: &Fixture, id: &str) {
        let id = id.to_string();
        cx.update_window(fixture.window.into(), |_, window, cx| window.click(id, cx))
            .expect("click");
        draw(cx, fixture);
    }

    /// Type into a text field: click it (which focuses it), then hand the text
    /// to whatever has focus.
    fn type_into(cx: &mut TestAppContext, fixture: &Fixture, id: &str, text: &str) {
        let id = id.to_string();
        cx.update_window(fixture.window.into(), |_, window, cx| {
            window.click(id, cx);
            window.input(text, cx);
        })
        .expect("type");
        draw(cx, fixture);
    }

    /// Open a row's menu: the right button is what does it, and `right_click`
    /// sends the platform's own aux click.
    fn right_click(cx: &mut TestAppContext, fixture: &Fixture, id: &str) {
        let id = id.to_string();
        cx.update_window(fixture.window.into(), move |_, window, cx| {
            window.right_click(id, cx)
        })
        .expect("right click");
        draw(cx, fixture);
    }

    fn press(cx: &mut TestAppContext, fixture: &Fixture, key: &str) {
        let key = key.to_string();
        cx.update_window(fixture.window.into(), move |_, window, cx| {
            window.press(&key, cx)
        })
        .expect("press");
        draw(cx, fixture);
    }

    /// The accessible name of an element, if it is on screen.
    fn label(cx: &mut TestAppContext, fixture: &Fixture, id: &str) -> Option<String> {
        let id = id.to_string();
        cx.update_window(fixture.window.into(), move |_, window, _| {
            window
                .try_find(id)
                .and_then(|el| el.label().map(str::to_string))
        })
        .expect("read the frame")
    }

    fn found(cx: &mut TestAppContext, fixture: &Fixture, id: &str) -> bool {
        let id = id.to_string();
        cx.update_window(fixture.window.into(), move |_, window, _| {
            window.try_find(id).is_some()
        })
        .expect("read the frame")
    }

    /// Where an element ended up, in window coordinates. A panel's side and a
    /// drag's two ends are both only readable here.
    fn bounds(cx: &mut TestAppContext, fixture: &Fixture, id: &str) -> Bounds<Pixels> {
        let wanted = id.to_string();
        cx.update_window(fixture.window.into(), |_, window, _| {
            window.try_find(wanted).map(|el| el.bounds())
        })
        .expect("read the frame")
        .unwrap_or_else(|| panic!("{id} is not on screen"))
    }

    /// The status the state holds for `worktree_id`, in the words a status reads
    /// as. The worktree's own line no longer draws it — its terminal lines do.
    fn status_word(
        cx: &mut TestAppContext,
        fixture: &Fixture,
        worktree_id: &str,
    ) -> Option<String> {
        cx.update(|cx| {
            let state = fixture.state.read(cx);
            let word = match (state.status_of(worktree_id)?, state.stalled_of(worktree_id)) {
                (AgentStatus::Permission, _) => "Waiting for you",
                (AgentStatus::Working, true) => "Working, but quiet",
                (AgentStatus::Working, false) => "Working",
                (AgentStatus::Idle, _) => "Idle",
            };
            Some(word.to_string())
        })
    }

    /// A row's word, in the order that decides it: how the child ended, then what
    /// its agent reported, then a job in the terminal's tty.
    #[test]
    fn a_rows_word_is_the_exit_then_the_agent_then_a_job() {
        use super::{Word, word_of};
        use syro_core::model::AgentStatus::{Idle, Permission, Working};

        assert_eq!(
            word_of(Some(true), Some(Working), false, true),
            Some(Word::Done)
        );
        assert_eq!(word_of(Some(false), None, false, false), Some(Word::Failed));
        assert_eq!(
            word_of(None, Some(Working), true, false),
            Some(Word::Stalled)
        );
        assert_eq!(
            word_of(None, Some(Permission), false, false),
            Some(Word::Waiting)
        );
        assert_eq!(
            word_of(None, Some(Working), false, false),
            Some(Word::Working)
        );
        // An agent's own report says more than the tty does, so it wins.
        assert_eq!(word_of(None, Some(Idle), false, true), Some(Word::Idle));
        // A shell at its prompt says nothing; one with a job in front of it says
        // that much, which is what a shell running `bp` reads as.
        assert_eq!(word_of(None, None, false, false), None);
        assert_eq!(word_of(None, None, false, true), Some(Word::Running));
    }

    /// A length is read in the largest unit that still says something: seconds
    /// under a minute, whole minutes under an hour, hours and minutes past that.
    #[test]
    fn a_length_is_read_in_the_largest_unit_it_has() {
        let read = |seconds| super::format_uptime(std::time::Duration::from_secs(seconds));
        assert_eq!(read(0), "0s");
        assert_eq!(read(59), "59s");
        assert_eq!(read(60), "1m");
        assert_eq!(read(599), "9m");
        assert_eq!(read(3600), "1h00m");
        assert_eq!(read(3900), "1h05m");
    }

    /// The agent the status tests start.
    fn waiting_agent() -> AgentSpec {
        AgentSpec {
            id: "title-agent".to_string(),
            name: "Title Agent".to_string(),
            command: "/bin/sh".to_string(),
            args: vec![
                "-c".to_string(),
                "printf '\\033]0;Title Agent needs you\\007'; sleep 30".to_string(),
            ],
            resume_args: Vec::new(),
            new_args: Vec::new(),
            env: Default::default(),
            status: vec![AgentStatusRule {
                pattern: "needs you".to_string(),
                requires: None,
                state: AgentStatus::Permission,
            }],
        }
    }

    /// The agent the width test starts: one whose title reads as working, which
    /// is the only word a dot rides beside.
    fn working_agent() -> AgentSpec {
        AgentSpec {
            id: "width-agent".to_string(),
            name: "Width Agent".to_string(),
            command: "/bin/sh".to_string(),
            args: vec![
                "-c".to_string(),
                "printf '\\033]0;Width Agent working\\007'; sleep 30".to_string(),
            ],
            resume_args: Vec::new(),
            new_args: Vec::new(),
            env: Default::default(),
            status: vec![AgentStatusRule {
                pattern: "working".to_string(),
                requires: None,
                state: AgentStatus::Working,
            }],
        }
    }

    fn settings_with(agents: Vec<AgentSpec>) -> Settings {
        Settings {
            agents,
            ..Default::default()
        }
    }

    /// The worktree on screen, and one that is not.
    fn active_and_other(cx: &mut TestAppContext, fixture: &Fixture) -> (String, String) {
        let repo_id = cx.update(|cx| {
            fixture
                .state
                .read(cx)
                .repos()
                .first()
                .expect("the scratch repo")
                .id
                .clone()
        });
        cx.update(|cx| {
            let state = fixture.state.read(cx);
            let active = state.active_id().expect("something is active").clone();
            let other = state
                .worktrees_of(&repo_id)
                .find(|worktree| worktree.id != active)
                .expect("the scratch repo has three worktrees")
                .id
                .clone();
            (active, other)
        })
    }

    /// A row's hover is a card, and a card is padded the same on both sides: the
    /// text used to be indented under the heading's name, which made the left
    /// look heavier than the right.
    #[gpui_kit::test]
    fn a_rows_hover_is_padded_the_same_on_both_sides(cx: &mut TestAppContext) {
        let fixture = open(cx, "row-padding");
        let (active, _other) = active_and_other(cx, &fixture);
        let has_row = |cx: &mut TestAppContext| {
            cx.update(|cx| {
                fixture
                    .workspace
                    .read(cx)
                    .resource_bar()
                    .read(cx)
                    .worktree_usage(&active)
                    .is_some_and(|usage| !usage.terminals.is_empty())
            })
        };
        assert!(
            draw_until(cx, &fixture, has_row),
            "the worktree never got a row"
        );
        let title = cx
            .update(|cx| {
                fixture
                    .workspace
                    .read(cx)
                    .resource_bar()
                    .read(cx)
                    .worktree_usage(&active)
                    .and_then(|usage| usage.terminals.first().map(|row| row.title.clone()))
            })
            .expect("a row");
        let left_of =
            |cx: &mut TestAppContext, id: &str| bounds(cx, &fixture, id).origin.x.as_f32();
        let right_of = |cx: &mut TestAppContext, id: &str| {
            let b = bounds(cx, &fixture, id);
            b.origin.x.as_f32() + b.size.width.as_f32()
        };
        let card = format!("worktree-{active}");
        let name = format!("terminal-name-{active}-{title}");
        let metrics = format!("terminal-metrics-{active}-{title}");
        let left = left_of(cx, &name) - left_of(cx, &card);
        let right = right_of(cx, &card) - right_of(cx, &metrics);
        assert!(
            (left - right).abs() < 2.,
            "the row's hover is lopsided: {left} on the left against {right} on the right"
        );
    }

    /// The row's highlight is a card, not a band: it keeps a margin from the
    /// panel's own edges, so a row never looks like it was painted edge to edge.
    #[gpui_kit::test]
    fn a_rows_card_stops_short_of_the_panels_edges(cx: &mut TestAppContext) {
        let fixture = open(cx, "row-inset");
        let (active, _other) = active_and_other(cx, &fixture);
        assert!(
            draw_until(cx, &fixture, |cx| found(
                cx,
                &fixture,
                &format!("worktree-{active}")
            )),
            "the worktree never got a card"
        );
        let panel = bounds(cx, &fixture, "sidebar");
        let card = bounds(cx, &fixture, &format!("worktree-{active}"));
        let panel_right = panel.origin.x + panel.size.width;
        let card_right = card.origin.x + card.size.width;
        assert!(
            card.origin.x > panel.origin.x,
            "the card is flush with the panel's left edge: {card:?} in {panel:?}"
        );
        assert!(
            card_right < panel_right,
            "the card runs past the panel's right edge: {card:?} in {panel:?}"
        );
    }

    /// The search box starts at the same column the rows are drawn in, so it
    /// never reaches past the line the list is inset to.
    #[gpui_kit::test]
    fn the_search_box_starts_no_further_out_than_the_rows_column(cx: &mut TestAppContext) {
        let fixture = open(cx, "search-column");
        let (active, _other) = active_and_other(cx, &fixture);
        assert!(
            draw_until(cx, &fixture, |cx| found(
                cx,
                &fixture,
                &format!("worktree-{active}")
            )),
            "the worktree never got a card"
        );
        let search = bounds(cx, &fixture, "sidebar-search").origin.x;
        let panel = bounds(cx, &fixture, "sidebar").origin.x;
        let card = bounds(cx, &fixture, &format!("worktree-{active}")).origin.x;
        assert!(
            search >= card && search > panel,
            "the search box reaches past the rows' column: search {search}, card {card}, panel {panel}"
        );
    }

    /// A worktree's column names come with the rows they head: one with nothing
    /// running in it draws none.
    #[gpui_kit::test]
    fn a_worktrees_columns_are_named_only_where_its_rows_are(cx: &mut TestAppContext) {
        let fixture = open(cx, "column-names");
        let (active, other) = active_and_other(cx, &fixture);
        // Asked of the readout rather than guessed from the fixture: which
        // worktrees have rows is the sweep's answer, not this test's.
        let rows = |cx: &mut TestAppContext, id: &str| {
            let id = id.to_string();
            cx.update(|cx| {
                fixture
                    .workspace
                    .read(cx)
                    .resource_bar()
                    .read(cx)
                    .worktree_usage(&id)
                    .is_some_and(|usage| !usage.terminals.is_empty())
            })
        };
        assert!(
            draw_until(cx, &fixture, |cx| rows(cx, &active)),
            "the worktree on screen never got a row"
        );
        for (id, runs) in [(&active, true), (&other, false)] {
            assert_eq!(
                found(cx, &fixture, &format!("terminal-columns-{id}")),
                runs,
                "the columns of {id}"
            );
        }
    }

    #[gpui_kit::test]
    fn an_agents_title_shows_as_the_worktrees_status(cx: &mut TestAppContext) {
        let fixture = open_with(cx, "sidebar-status", settings_with(vec![waiting_agent()]));
        let id = cx.update(|cx| {
            fixture
                .state
                .read(cx)
                .active_id()
                .expect("something is active")
                .clone()
        });

        // A plain shell has no preset, so it has no dot.
        assert_eq!(status_word(cx, &fixture, &id), None);

        cx.dispatch_action(
            fixture.window.into(),
            terminal::New {
                worktree_id: id.clone(),
                agent: Some("title-agent".to_string()),
                resume: None,
                seed: None,
            },
        );

        // The whole path, once: the action starts the agent, the agent's child
        // sets its title, the view reads it against the preset's rules, the
        // workspace hands the reading to the state, and the dot shows it.
        assert!(
            draw_until(cx, &fixture, |cx| status_word(cx, &fixture, &id)
                == Some("Waiting for you".to_string())),
            "the waiting agent never reached the sidebar: {:?}",
            status_word(cx, &fixture, &id)
        );

        // The word is in the columns the branch's own line names, under the
        // `状态` label rather than under the branch name, and the `CPU` label is
        // over the number.
        let title = cx
            .update(|cx| {
                fixture
                    .workspace
                    .read(cx)
                    .resource_bar()
                    .read(cx)
                    .worktree_usage(&id)
                    .and_then(|usage| {
                        usage
                            .terminals
                            .iter()
                            .find(|row| row.status.is_some())
                            .map(|row| row.title.clone())
                    })
            })
            .expect("the agent has a row");
        let status = format!("terminal-status-{id}-{title}");
        assert!(
            draw_until(cx, &fixture, |cx| found(cx, &fixture, &status)),
            "the word never reached the row"
        );
        let right = |cx: &mut TestAppContext, id: &str| {
            let bounds = bounds(cx, &fixture, id);
            bounds.origin.x.as_f32() + bounds.size.width.as_f32()
        };
        let label = right(cx, &format!("terminal-columns-status-{id}"));
        let word = right(cx, &status);
        assert!(
            (label - word).abs() < 1.,
            "the word is not under the 状态 label: {word} against {label}"
        );
        let cpu_label = bounds(cx, &fixture, &format!("terminal-columns-cpu-{id}"))
            .origin
            .x
            .as_f32();
        let number = bounds(cx, &fixture, &format!("terminal-metrics-{id}-{title}"))
            .origin
            .x
            .as_f32();
        assert!(
            (cpu_label - number).abs() < 1.,
            "the number is not under the CPU label: {number} against {cpu_label}"
        );

        // A row with no word keeps the column too: the shell's name stops at the
        // status column rather than running through it to the numbers.
        let shell = cx
            .update(|cx| {
                fixture
                    .workspace
                    .read(cx)
                    .resource_bar()
                    .read(cx)
                    .worktree_usage(&id)
                    .and_then(|usage| {
                        usage
                            .terminals
                            .iter()
                            .find(|row| row.status.is_none())
                            .map(|row| row.title.clone())
                    })
            })
            .expect("the shell has a row");
        let name = right(cx, &format!("terminal-name-{id}-{shell}"));
        let column = bounds(cx, &fixture, &format!("terminal-columns-status-{id}"))
            .origin
            .x
            .as_f32();
        assert!(
            name <= column,
            "the shell's name runs into the status column: {name} against {column}"
        );

        // Closing the agent's tab: its status goes with the process it described, or the dot would stay
        // red for an agent that is gone.
        cx.dispatch_action(
            fixture.window.into(),
            tab::Close {
                worktree_id: id.clone(),
                index: 1,
            },
        );
        assert!(
            draw_until(cx, &fixture, |cx| status_word(cx, &fixture, &id).is_none()),
            "the dot outlived the agent: {:?}",
            status_word(cx, &fixture, &id)
        );
    }

    /// The word a row draws has to fit the column it ends in, dot and all: at 44
    /// the dot beside `运行中` pushed the label past the edge and the row drew
    /// `运行…`, which says less than the word it meant.
    #[gpui_kit::test]
    fn a_working_rows_word_is_never_cut_short(cx: &mut TestAppContext) {
        let fixture = open_with(
            cx,
            "sidebar-word-width",
            settings_with(vec![working_agent()]),
        );
        let id = cx.update(|cx| {
            fixture
                .state
                .read(cx)
                .active_id()
                .expect("something is active")
                .clone()
        });
        cx.dispatch_action(
            fixture.window.into(),
            terminal::New {
                worktree_id: id.clone(),
                agent: Some("width-agent".to_string()),
                resume: None,
                seed: None,
            },
        );
        // Read back through the readout rather than guessed from the fixture: the
        // title the agent's own child set is the row's, and working is the word
        // that carries the dot.
        let word = |cx: &mut TestAppContext| {
            cx.update(|cx| {
                fixture
                    .workspace
                    .read(cx)
                    .resource_bar()
                    .read(cx)
                    .worktree_usage(&id)
                    .and_then(|usage| {
                        usage
                            .terminals
                            .iter()
                            .find(|row| row.status == Some(AgentStatus::Working))
                            .map(|row| format!("terminal-status-{id}-{}", row.title))
                    })
            })
        };
        assert!(
            draw_until(cx, &fixture, |cx| {
                word(cx).is_some_and(|word| found(cx, &fixture, &word))
            }),
            "the working agent never reached the row"
        );
        let drawn = word(cx).expect("the row's word");
        let width = bounds(cx, &fixture, &drawn).size.width.as_f32();
        assert!(
            width < super::STATUS_MAX_WIDTH,
            "the word is cut short: {width} in a {} column",
            super::STATUS_MAX_WIDTH
        );
    }

    #[gpui_kit::test]
    fn waiting_for_input_notifies_the_worktree_the_user_is_not_watching(cx: &mut TestAppContext) {
        let fixture = open(cx, "sidebar-notify");
        cx.update(|cx| cx.set_app_identity(crate::BUNDLE_ID, crate::APP_NAME));
        focus(cx, &fixture);
        let (active, other) = active_and_other(cx, &fixture);

        // The status goes in through the state's own door — the test above is
        // what proves a real agent's title arrives there.
        let note = |cx: &mut TestAppContext, status: Option<AgentStatus>| {
            cx.update(|cx| {
                fixture.state.update(cx, |state, cx| {
                    state.note_agent_status(
                        &other,
                        &"terminal-1".to_string(),
                        Some("claude".to_string()),
                        status,
                        false,
                        cx,
                    )
                })
            });
        };

        note(cx, Some(AgentStatus::Permission));
        let shown = cx.shown_system_notifications();
        assert_eq!(shown.len(), 1, "waiting for input should notify once");
        let other_name = cx.update(|cx| {
            fixture
                .state
                .read(cx)
                .worktree(&other)
                .expect("the worktree")
                .name
                .clone()
        });
        assert_eq!(shown[0].tag, "syro/terminal-1");
        assert_eq!(shown[0].body, format!("{other_name} needs your input"));
        assert_eq!(cx.delivered_system_notifications().len(), 1);

        // Idle is not news — a turn ended, which happens all day — so it
        // notifies nobody.
        note(cx, Some(AgentStatus::Idle));
        assert_eq!(cx.shown_system_notifications().len(), 1);

        // Waiting again: the same terminal's notification is replaced rather
        // than piling up, because the tag is the terminal's.
        note(cx, Some(AgentStatus::Permission));
        assert_eq!(cx.shown_system_notifications().len(), 2);
        assert_eq!(
            cx.delivered_system_notifications().len(),
            1,
            "a second notification for one terminal should replace the first"
        );

        // And the dot is on that worktree, not on the one on screen.
        assert_eq!(
            status_word(cx, &fixture, &other).as_deref(),
            Some("Waiting for you")
        );
        assert_eq!(status_word(cx, &fixture, &active), None);
    }

    #[gpui_kit::test]
    fn the_worktree_on_screen_does_not_notify(cx: &mut TestAppContext) {
        let fixture = open(cx, "sidebar-notify-visible");
        cx.update(|cx| cx.set_app_identity(crate::BUNDLE_ID, crate::APP_NAME));
        focus(cx, &fixture);
        let (active, _) = active_and_other(cx, &fixture);

        cx.update(|cx| {
            fixture.state.update(cx, |state, cx| {
                state.note_agent_status(
                    &active,
                    &"terminal-2".to_string(),
                    Some("claude".to_string()),
                    Some(AgentStatus::Permission),
                    false,
                    cx,
                )
            })
        });

        // The dot still moves: it is the notification a user already looking at
        // the terminal does not need.
        assert_eq!(
            status_word(cx, &fixture, &active).as_deref(),
            Some("Waiting for you")
        );
        assert!(cx.shown_system_notifications().is_empty());
    }

    /// A quiet agent is still working by its own title, so the dot keeps that
    /// reading and goes hollow rather than taking a fourth colour.
    #[gpui_kit::test]
    fn a_quiet_agent_says_so_without_changing_its_status(cx: &mut TestAppContext) {
        let fixture = open(cx, "sidebar-stalled");
        let (active, other) = active_and_other(cx, &fixture);
        let note = |cx: &mut TestAppContext, status: Option<AgentStatus>, stalled: bool| {
            cx.update(|cx| {
                fixture.state.update(cx, |state, cx| {
                    state.note_agent_status(
                        &other,
                        &"terminal-stalled".to_string(),
                        Some("claude".to_string()),
                        status,
                        stalled,
                        cx,
                    )
                })
            });
        };

        note(cx, Some(AgentStatus::Working), false);
        assert_eq!(
            status_word(cx, &fixture, &other).as_deref(),
            Some("Working")
        );
        assert_eq!(status_word(cx, &fixture, &active), None);

        // The same status, and the agent has gone quiet: nothing about the title
        // changed, which is what makes this a second reading rather than a
        // fourth status.
        note(cx, Some(AgentStatus::Working), true);
        assert_eq!(
            status_word(cx, &fixture, &other).as_deref(),
            Some("Working, but quiet")
        );

        note(cx, Some(AgentStatus::Working), false);
        assert_eq!(
            status_word(cx, &fixture, &other).as_deref(),
            Some("Working")
        );
    }

    fn row_id(path: &std::path::Path) -> String {
        format!("worktree-{}", path.display())
    }

    fn row_is_listed(cx: &mut TestAppContext, fixture: &Fixture, path: &std::path::Path) -> bool {
        let id = row_id(path);
        cx.update_window(fixture.window.into(), |_, window, _| {
            window.try_find(id).is_some()
        })
        .expect("read the frame")
    }

    #[gpui_kit::test]
    fn the_sidebar_lists_the_worktrees_and_a_click_activates_one(cx: &mut TestAppContext) {
        let fixture = open(cx, "sidebar-list");
        let active = fixture.worktrees.last().expect("two worktrees").clone();

        // The repo's own checkout plus the two `git worktree add`s: the list is
        // git's, not state.json's.
        for path in &fixture.worktrees {
            assert!(
                row_is_listed(cx, &fixture, path),
                "{} is missing from the sidebar",
                path.display()
            );
        }

        cx.update_window(fixture.window.into(), |_, window, cx| {
            window.click(row_id(&active), cx);
        })
        .expect("click the worktree");

        assert!(
            draw_until(cx, &fixture, |cx| cx.update(|app| {
                fixture.state.read(app).active_id() == Some(&active.display().to_string())
            })),
            "the click never activated {}: {:?}",
            active.display(),
            cx.update(|cx| fixture.state.read(cx).active_id().cloned())
        );

        // And it is the file that remembers it, not just the entity.
        assert_eq!(
            Store::new(fixture.state_path.clone())
                .load()
                .unwrap()
                .active_worktree,
            Some(active.display().to_string())
        );
    }

    #[gpui_kit::test]
    fn creating_a_worktree_puts_a_row_in_the_sidebar(cx: &mut TestAppContext) {
        let fixture = open(cx, "sidebar-create");
        let repo_id = cx.update(|cx| {
            fixture
                .state
                .read(cx)
                .repos()
                .first()
                .expect("the scratch repo")
                .id
                .clone()
        });

        cx.dispatch_action(
            fixture.window.into(),
            worktree::Create {
                repo_id,
                name: "from the action".to_string(),
                base: None,
            },
        );

        // <workspaces root>/<repo>/<name>, and the row only appears once git
        // has made it: nothing in the sidebar is optimistic. Resolved, because
        // that is the spelling git reports and the one the row is keyed by.
        let expected = fixture
            ._dir
            .canonicalize()
            .unwrap_or_else(|_| fixture._dir.clone())
            .join("workspaces")
            .join("main")
            .join("from the action");
        assert!(
            draw_until(cx, &fixture, |cx| row_is_listed(cx, &fixture, &expected)),
            "the created worktree never reached the sidebar at {}",
            expected.display()
        );
        assert!(expected.is_dir());
        cx.update(|cx| {
            assert_eq!(
                fixture.state.read(cx).active_id(),
                Some(&expected.display().to_string()),
                "a worktree you just made is the one you want to look at"
            );
        });
    }

    #[gpui_kit::test]
    fn removing_a_worktree_takes_its_row_away(cx: &mut TestAppContext) {
        let fixture = open(cx, "sidebar-remove");
        let doomed = fixture.worktrees.first().expect("two worktrees").clone();
        assert!(row_is_listed(cx, &fixture, &doomed));

        cx.dispatch_action(
            fixture.window.into(),
            worktree::Remove {
                worktree_id: doomed.display().to_string(),
                force: false,
            },
        );

        assert!(
            draw_until(cx, &fixture, |cx| !row_is_listed(cx, &fixture, &doomed)),
            "the removed worktree is still in the sidebar"
        );
        assert!(!doomed.exists(), "the directory is still there");
    }

    /// What can be done to a worktree lives behind the right button, so the row
    /// itself is only what the worktree *is*.
    #[gpui_kit::test]
    fn the_row_menu_holds_the_row_actions(cx: &mut TestAppContext) {
        let fixture = open(cx, "sidebar-menu");
        let worktree = fixture.worktrees.first().expect("two worktrees").clone();
        let row = format!("worktree-{}", worktree.display());

        // The `×` the row used to carry is the menu's now.
        assert!(
            !found(
                cx,
                &fixture,
                &format!("worktree-remove-{}", worktree.display())
            ),
            "the row still carries a remove button"
        );
        assert!(
            !found(cx, &fixture, "sidebar-menu"),
            "a menu was already up"
        );

        right_click(cx, &fixture, &row);
        assert!(
            draw_until(cx, &fixture, |cx| found(cx, &fixture, "sidebar-menu")),
            "the right-click opened no menu"
        );
        assert_eq!(
            label(cx, &fixture, "worktree-menu-delete").as_deref(),
            Some("Remove worktree")
        );
        assert!(found(cx, &fixture, "worktree-menu-reveal"));
        // A worktree's menu has no "new worktree": that belongs to the
        // repository, and no rename either, because a worktree's name is made
        // from what was typed when it was created.
        assert!(
            label(cx, &fixture, "worktree-menu-new").is_none(),
            "the worktree's menu still offers a new worktree"
        );
        assert!(
            label(cx, &fixture, "worktree-menu-rename").is_none(),
            "the menu still offers a rename"
        );

        // Escape backs out of it, which is what a menu has to do.
        press(cx, &fixture, "escape");
        assert!(
            draw_until(cx, &fixture, |cx| !found(cx, &fixture, "sidebar-menu")),
            "escape left the menu up"
        );

        // The repository's own checkout is the project itself, and git refuses to
        // remove it as a worktree: its menu says what the dialog will ask, rather
        // than promising to remove a worktree that is not one.
        let own = cx.update(|cx| fixture.state.read(cx).repos()[0].path.display().to_string());
        right_click(cx, &fixture, &format!("worktree-{own}"));
        assert!(
            draw_until(cx, &fixture, |cx| found(cx, &fixture, "sidebar-menu")),
            "the right-click opened no menu"
        );
        assert_eq!(
            label(cx, &fixture, "worktree-menu-delete").as_deref(),
            Some("Remove project"),
            "the project's own checkout was offered a worktree removal"
        );
    }

    /// A repository heading is the fold and the menu: clicking it puts the
    /// worktrees away, and the `...` beside it is everything that can be done to
    /// the repository — including hiding it, which has to be undoable from the
    /// panel it disappeared from.
    #[gpui_kit::test]
    fn the_repo_heading_folds_and_offers_its_menu(cx: &mut TestAppContext) {
        let fixture = open(cx, "sidebar-repo-menu");
        let (repo, worktree) = cx.update(|cx| {
            let state = fixture.state.read(cx);
            let repo = state.repos()[0].id.clone();
            let worktree = state.active().expect("something is active").id.clone();
            (repo, worktree)
        });
        let row = format!("worktree-{worktree}");
        assert!(found(cx, &fixture, &row), "the worktree row is missing");

        // The heading folds the worktrees away, and unfolds them again.
        click(cx, &fixture, &format!("repo-heading-{repo}"));
        assert!(
            draw_until(cx, &fixture, |cx| !found(cx, &fixture, &row)),
            "the heading did not fold the worktrees away"
        );
        click(cx, &fixture, &format!("repo-heading-{repo}"));
        assert!(
            draw_until(cx, &fixture, |cx| found(cx, &fixture, &row)),
            "the heading did not open them again"
        );

        // The two `...` line up: the panel's own and a repository's are the same
        // box at the same right edge, which the list's gutter would otherwise
        // push four pixels out of true.
        let panel = bounds(cx, &fixture, "sidebar-menu-open");
        let heading = bounds(cx, &fixture, &format!("repo-menu-{repo}"));
        assert!(
            (panel.origin.x - heading.origin.x).as_f32().abs() < 1.,
            "the `...` buttons do not line up: {panel:?}, {heading:?}"
        );

        // The `...` is the menu, and clicking it does not fold the heading.
        click(cx, &fixture, &format!("repo-menu-{repo}"));
        assert!(
            draw_until(cx, &fixture, |cx| found(cx, &fixture, "sidebar-menu")),
            "the heading's menu did not open"
        );
        assert_eq!(
            label(cx, &fixture, "repo-menu-hide").as_deref(),
            Some("Hide project")
        );
        assert_eq!(
            label(cx, &fixture, "repo-menu-delete").as_deref(),
            Some("Remove project")
        );
        assert!(found(cx, &fixture, &row), "the menu folded the heading");

        // Hiding takes the repository out of the list, and the way back is the
        // panel's own menu — a project that could only be brought back by editing
        // `state.json` is a project that is gone.
        click(cx, &fixture, "repo-menu-hide");
        assert!(
            draw_until(cx, &fixture, |cx| !found(cx, &fixture, &row)),
            "the hidden project is still in the list"
        );
        // And the file it was written to, because a project hidden today is
        // still hidden tomorrow.
        let saved = Store::new(fixture.state_path.clone()).load().unwrap();
        assert_eq!(saved.hidden_repos, vec![repo.clone()]);

        click(cx, &fixture, "sidebar-menu-open");
        assert!(
            draw_until(cx, &fixture, |cx| found(
                cx,
                &fixture,
                "sidebar-menu-show-hidden"
            )),
            "the panel's menu did not open"
        );
        click(cx, &fixture, "sidebar-menu-show-hidden");
        assert!(
            draw_until(cx, &fixture, |cx| found(cx, &fixture, &row)),
            "the hidden project never came back"
        );
    }

    /// The panel's own menu: what is hidden, whether the sleeping ones are drawn,
    /// and the order the list is in — the sort opening beside the menu, on the row
    /// that opened it.
    #[gpui_kit::test]
    fn the_sidebar_menu_sorts_and_hides_the_sleeping(cx: &mut TestAppContext) {
        let fixture = open(cx, "sidebar-menu-sort");
        let (active, other) = active_and_other(cx, &fixture);
        let active_row = row_id(Path::new(&active));
        let other_row = row_id(Path::new(&other));

        // The menu, and the submenu beside it rather than in place of it.
        click(cx, &fixture, "sidebar-menu-open");
        assert!(
            draw_until(cx, &fixture, |cx| found(
                cx,
                &fixture,
                "sidebar-menu-sort-by"
            )),
            "the panel's menu did not open"
        );
        assert!(
            !found(cx, &fixture, "sidebar-sort-menu"),
            "the sort submenu was open before it was asked for"
        );
        click(cx, &fixture, "sidebar-menu-sort-by");
        assert!(
            draw_until(cx, &fixture, |cx| found(cx, &fixture, "sidebar-sort-menu")),
            "the sort submenu did not open"
        );
        for sort in ["sort-name", "sort-activity", "sort-recent", "sort-project"] {
            assert!(found(cx, &fixture, sort), "the submenu has no {sort}");
        }
        // Where it is: beside the menu, on the row that opened it. A submenu a
        // guess away from its own row is a menu that looks detached from it.
        let menu = bounds(cx, &fixture, "sidebar-menu");
        let submenu = bounds(cx, &fixture, "sidebar-sort-menu");
        let opener = bounds(cx, &fixture, "sidebar-menu-sort-by");
        assert!(
            submenu.origin.x >= menu.origin.x + menu.size.width,
            "the submenu is not beside the menu: {submenu:?}, {menu:?}"
        );
        assert!(
            (submenu.origin.y - opener.origin.y).as_f32().abs() <= 2.,
            "the submenu is not on the row that opened it: {submenu:?}, {opener:?}"
        );

        // "Recent" is the one order this test can prove: point the window at the
        // other worktree, ask for that order, and it comes first. The menu is
        // backed out of first, because its scrim covers the rows behind it.
        press(cx, &fixture, "escape");
        click(cx, &fixture, &other_row);
        click(cx, &fixture, "sidebar-menu-open");
        click(cx, &fixture, "sidebar-menu-sort-by");
        click(cx, &fixture, "sort-recent");
        assert!(
            draw_until(cx, &fixture, |cx| !found(cx, &fixture, "sidebar-menu")),
            "picking a sort left the menu up"
        );
        let first = bounds(cx, &fixture, &other_row).origin.y;
        let second = bounds(cx, &fixture, &active_row).origin.y;
        assert!(
            first < second,
            "the recent sort did not put the worktree just opened first: {first:?}, {second:?}"
        );

        // And the sleeping ones: nothing is running in this fixture, so every
        // worktree but the one on screen is asleep — and that one stays, because a
        // list that hides what you are looking at looks broken.
        click(cx, &fixture, "sidebar-menu-open");
        click(cx, &fixture, "sidebar-menu-hide-asleep");
        assert!(
            draw_until(cx, &fixture, |cx| !found(cx, &fixture, &active_row)),
            "the sleeping worktree is still in the list"
        );
        assert!(
            found(cx, &fixture, &other_row),
            "hiding the sleeping ones hid the worktree on screen"
        );
    }

    /// The header is how a repository gets added at all: every other affordance
    /// in this panel hangs off a row that does not exist until one is.
    #[gpui_kit::test]
    fn the_header_offers_a_repository(cx: &mut TestAppContext) {
        let fixture = open(cx, "sidebar-header");
        assert_eq!(
            label(cx, &fixture, "repo-add").as_deref(),
            Some("Add repository")
        );
        // One action, not two: a new worktree is the repository heading's own
        // `+`, in the repository that heading is about.
        assert!(
            !found(cx, &fixture, "worktree-add"),
            "the header still carries a new-worktree button"
        );

        // Adding a repository opens the Add-project dialog's three doors,
        // and the platform's own folder picker is one of them.
        click(cx, &fixture, "repo-add");
        assert!(
            draw_until(cx, &fixture, |cx| found(cx, &fixture, "add-project")),
            "add repository did not open the dialog"
        );
        assert!(found(cx, &fixture, "add-project-browse"));
        assert!(found(cx, &fixture, "add-project-clone"));
        assert!(found(cx, &fixture, "add-project-new"));
        // Backed out of, because the dialog is modal and the next click is in
        // the panel behind it.
        press(cx, &fixture, "escape");
        assert!(
            draw_until(cx, &fixture, |cx| !found(cx, &fixture, "add-project")),
            "escape did not close the dialog"
        );

        // Starting a worktree is the `...` on the repository's own heading, and
        // then the entry in it that makes one.
        let repo = cx.update(|cx| fixture.state.read(cx).repos()[0].id.clone());
        click(cx, &fixture, &format!("repo-menu-{repo}"));
        assert!(
            draw_until(cx, &fixture, |cx| found(cx, &fixture, "repo-menu-new")),
            "the heading's menu did not open"
        );
        click(cx, &fixture, "repo-menu-new");
        assert!(
            found(cx, &fixture, "new-worktree-name"),
            "the new-worktree dialog did not open"
        );
    }

    /// The project menu's shared-paths entry opens the form, and what it adds and
    /// removes is the project's list: kept clean, and saved.
    #[gpui_kit::test]
    fn the_shared_paths_form_edits_the_projects_list(cx: &mut TestAppContext) {
        let fixture = open(cx, "sidebar-shared-paths");
        let repo = cx.update(|cx| fixture.state.read(cx).repos()[0].id.clone());
        let shared = |cx: &mut TestAppContext| {
            cx.update(|cx| fixture.state.read(cx).repos()[0].shared_paths.clone())
        };

        click(cx, &fixture, &format!("repo-menu-{repo}"));
        assert!(
            draw_until(cx, &fixture, |cx| found(
                cx,
                &fixture,
                "repo-menu-shared-paths"
            )),
            "the project menu has no shared-paths entry"
        );
        click(cx, &fixture, "repo-menu-shared-paths");
        assert!(
            draw_until(cx, &fixture, |cx| found(cx, &fixture, "shared-paths")),
            "the entry did not open the form"
        );

        // "Add path" opens the picker on the checkout's top level, and a click on
        // an entry shares it.
        let picker = |cx: &mut TestAppContext| {
            click(cx, &fixture, "shared-paths-add");
            assert!(
                draw_until(cx, &fixture, |cx| found(
                    cx,
                    &fixture,
                    "shared-paths-picker"
                )),
                "add path did not open the picker"
            );
        };
        picker(cx);
        click(cx, &fixture, "shared-paths-entry-README.md");
        assert!(
            draw_until(cx, &fixture, |cx| shared(cx) == ["README.md"]),
            "the picked entry did not reach the project: {:?}",
            shared(cx)
        );
        assert!(found(cx, &fixture, "shared-path-0"));

        // A path the list does not have is typed, and offered as itself.
        picker(cx);
        type_into(cx, &fixture, "shared-paths-input", "./data/");
        click(cx, &fixture, "shared-paths-literal");
        assert!(
            draw_until(cx, &fixture, |cx| shared(cx) == ["README.md", "data"]),
            "the typed path did not reach the project: {:?}",
            shared(cx)
        );

        // A path out of the checkout is refused, and the list is left as it was.
        picker(cx);
        type_into(cx, &fixture, "shared-paths-input", "../elsewhere");
        click(cx, &fixture, "shared-paths-literal");
        assert!(found(cx, &fixture, "shared-paths-error"));
        assert_eq!(shared(cx), ["README.md", "data"]);

        click(cx, &fixture, "shared-path-remove-0");
        assert!(
            draw_until(cx, &fixture, |cx| shared(cx) == ["data"]),
            "the chip's x did not remove it: {:?}",
            shared(cx)
        );
    }

    /// A `[[hook]]` that failed says so where the worktrees are: a hook is the
    /// user's own command, and the panel that shows their worktrees is where
    /// they look for what happened to one.
    #[gpui_kit::test]
    fn a_failed_hook_is_visible_where_the_worktrees_are(cx: &mut TestAppContext) {
        let mut settings = settings_with(vec![waiting_agent()]);
        settings.hooks = vec![HookSpec {
            on: "agent_needs_input".to_string(),
            run: "echo pnpm could not install >&2; exit 3".to_string(),
            terminal: false,
        }];
        let fixture = open_with(cx, "sidebar-hook", settings);
        assert!(
            label(cx, &fixture, "banner-hook").is_none(),
            "nothing has failed yet"
        );

        let id = cx.update(|cx| {
            fixture
                .state
                .read(cx)
                .active_id()
                .expect("something is active")
                .clone()
        });
        // The agent whose title reads as waiting: that is the status the hook is
        // named after, and starting it is how a test makes the app see it.
        cx.dispatch_action(
            fixture.window.into(),
            terminal::New {
                worktree_id: id.clone(),
                agent: Some("title-agent".to_string()),
                resume: None,
                seed: None,
            },
        );

        assert!(
            draw_until(cx, &fixture, |cx| label(cx, &fixture, "banner-hook")
                .is_some()),
            "the failed hook never reached the window"
        );
        let message = label(cx, &fixture, "banner-hook").expect("the banner");
        assert!(
            message.contains("exit 3"),
            "the message does not say how the hook ended: {message}"
        );
        assert!(
            message.contains("pnpm could not install"),
            "the message does not say what the hook printed: {message}"
        );
        // Nothing was rolled back for it: the worktree the hook was reacting to
        // is still a row.
        let repo_id = cx.update(|cx| fixture.state.read(cx).repos()[0].id.clone());
        assert!(
            cx.update(|cx| fixture
                .state
                .read(cx)
                .worktrees_of(&repo_id)
                .any(|worktree| worktree.id == id)),
            "a failed hook removed the worktree it was about"
        );
    }

    /// A drag that ends is where the file gets its width — and the next run
    /// reads it back, which is the whole point of writing it.
    #[gpui_kit::test]
    fn a_dragged_width_is_what_the_next_run_opens_with(cx: &mut TestAppContext) {
        let fixture = open(cx, "sidebar-width-drag");
        let sidebar = cx.update(|cx| fixture.workspace.read(cx).sidebar.clone());

        let handle = bounds(cx, &fixture, "sidebar-resize");
        let from = Point::new(handle.origin.x + px(3.), handle.origin.y + px(100.));
        let to = Point::new(from.x + px(60.), from.y);
        cx.update_window(fixture.window.into(), |_, window, cx| {
            window.drag(from, to, cx)
        })
        .expect("drag the edge");
        draw(cx, &fixture);

        let dragged = cx.update(|cx| sidebar.read(cx).width());
        assert_eq!(dragged, 300., "the drag did not widen the panel");
        assert_eq!(
            cx.update(|cx| fixture.state.read(cx).sidebar_width()),
            Some(dragged),
            "the width the drag ended on was not written to state"
        );
        // And the file, which is what a restart actually reads.
        let saved = Store::new(fixture.state_path.clone()).load().unwrap();
        assert_eq!(saved.sidebar_width, Some(dragged));

        // A window opened on that file: the panel starts where the drag left it.
        let reopened = open_from(cx, "sidebar-width-drag-reopen", |state| {
            state.sidebar_width = Some(410.);
        });
        let reopened_sidebar = cx.update(|cx| reopened.workspace.read(cx).sidebar.clone());
        assert_eq!(
            cx.update(|cx| reopened_sidebar.read(cx).width()),
            410.,
            "a remembered width was not what the panel opened with"
        );

        // And one from a window wide enough to have written more than this panel
        // can be: clamped, rather than opening off the edge.
        let clamped = open_from(cx, "sidebar-width-clamped", |state| {
            state.sidebar_width = Some(9_000.);
        });
        let clamped_sidebar = cx.update(|cx| clamped.workspace.read(cx).sidebar.clone());
        assert_eq!(
            cx.update(|cx| clamped_sidebar.read(cx).width()),
            MAX_WIDTH,
            "an out-of-range width was used as it came in"
        );
    }

    #[gpui_kit::test]
    fn the_edge_drag_resizes_the_sidebar(cx: &mut TestAppContext) {
        let fixture = open(cx, "sidebar-resize-drag");
        let sidebar = cx.update(|cx| fixture.workspace.read(cx).sidebar.clone());
        assert_eq!(
            cx.update(|cx| sidebar.read(cx).width()),
            240.,
            "the panel does not open at the default width"
        );

        let handle = bounds(cx, &fixture, "sidebar-resize");
        let from = Point::new(handle.origin.x + px(3.), handle.origin.y + px(100.));
        let to = Point::new(from.x + px(40.), from.y);
        cx.update_window(fixture.window.into(), |_, window, cx| {
            window.drag(from, to, cx)
        })
        .expect("drag the edge");
        draw(cx, &fixture);

        assert_eq!(
            cx.update(|cx| sidebar.read(cx).width()),
            280.,
            "the drag did not widen the panel"
        );
    }

    /// With no repository the panel has nothing to list, so the way in has to be
    /// the header — and the empty state has to say what the panel is for.
    #[gpui_kit::test]
    fn a_first_run_window_offers_the_header_and_an_empty_state(cx: &mut TestAppContext) {
        let fixture = open_empty(cx, "sidebar-empty");

        assert!(
            cx.update(|cx| fixture.state.read(cx).repos().is_empty()),
            "the fixture seeded a repository"
        );
        // The overlay drawn when the list has nothing in it: centred, with
        // a line saying what to do about it.
        assert!(found(cx, &fixture, "repo-empty"));
        // The header is the top of the panel and the list is under it.
        let header = bounds(cx, &fixture, "sidebar-header");
        let list = bounds(cx, &fixture, "sidebar-list");
        assert!(
            header.origin.y + header.size.height <= list.origin.y,
            "the list is above the header: {header:?}, {list:?}"
        );
        // And the search button is in it: the way to the global search.
        assert!(
            found(cx, &fixture, "sidebar-search"),
            "the sidebar has no search button"
        );
        assert_eq!(
            label(cx, &fixture, "repo-add").as_deref(),
            Some("Add repository"),
            "the empty sidebar has no way in"
        );
        // And clicking the overlay is the way in too: the first screen says
        // "add one", so it is what opens the dialog.
        assert_eq!(
            label(cx, &fixture, "repo-empty").as_deref(),
            Some("Add a repository")
        );
        click(cx, &fixture, "repo-empty");
        assert!(
            draw_until(cx, &fixture, |cx| found(cx, &fixture, "add-project")),
            "the empty state is not a way to add a repository"
        );
    }

    /// A number that outgrows its column takes the room from the name rather than
    /// folding onto a second line. A terminal's line is 18 points tall, so the
    /// second line of a folded number is painted over the row below — which is the
    /// state the readout was once photographed in, `475 MB` split over `MB` and
    /// the next row's cost split the same way.
    #[gpui_kit::test]
    fn a_wider_number_does_not_make_the_row_taller(cx: &mut TestAppContext) {
        let window = cx.update(|cx| {
            gpui_kit::init(cx);
            let bounds = Bounds::new(Point::default(), size(px(240.), px(80.)));
            let (window, _) = gpui_kit::open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    ..Default::default()
                },
                cx,
                |_, cx| cx.new(|_| MetricsColumn),
            )
            .expect("open a blank window");
            window.downcast::<Root>().expect("Base Root")
        });
        cx.update_window(window.into(), |_, window, cx| {
            window.draw(cx).clear(cx);
        })
        .expect("draw a frame");
        let mut height = |id: &str| {
            let id = id.to_string();
            let asked = id.clone();
            cx.update_window(window.into(), |_, window, _| {
                window.try_find(id).map(|el| el.bounds().size.height)
            })
            .expect("read the frame")
            .unwrap_or_else(|| panic!("{asked} is not on screen"))
        };

        let small = height("terminal-metrics-small");
        let wide = height("terminal-metrics-wide");
        assert_eq!(
            small, wide,
            "the cost folded onto a second line: {small:?} against {wide:?}"
        );
    }

    /// Two cost columns, the second wider than the room it is given.
    struct MetricsColumn;

    impl Render for MetricsColumn {
        fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let colors = Theme::global(cx).tokens.colors;
            div()
                .v_flex()
                .w(px(232.))
                .child(super::row_metrics(
                    SharedString::from("terminal-metrics-small"),
                    1.0,
                    9 * 1024 * 1024,
                    &colors,
                ))
                .child(super::row_metrics(
                    SharedString::from("terminal-metrics-wide"),
                    140.0,
                    999 * 1024 * 1024,
                    &colors,
                ))
        }
    }
}
