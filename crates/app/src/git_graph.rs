//! The git graph: every branch, the commits they meet on, and the branches the
//! reader can throw away from here.
//!
//! The source control panel is a column beside the panes and shows one branch.
//! This is the window-filling version of the same question — where two branches
//! met, and which one is safe to delete — drawn as one row per commit with the
//! lines between them in a column of their own.

use std::collections::BTreeMap;
use std::path::PathBuf;

use gpui_kit::base::{ColorTokens, StyledExt as _, TestSupportExt as _, Theme};
use gpui_kit::component::button::ButtonVariant;
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, WindowExt as _};
use gpui_kit::*;
use rust_i18n::t;
use syro_core::event::CoreEvent;
use syro_core::git::{self, Branch, Commit, FileChange, RemoteRef};
use syro_core::session;

use crate::git_panel::commit_file_row;
use crate::state::{AppState, worktree};

/// How much history the graph draws. Deeper than the panel's fifty — this is the
/// view for reading a history, and a screenful is around forty rows — but still a
/// ceiling, because `--all` on a repository with a decade of work would ask git
/// for every commit it has.
const COMMITS: usize = 400;

/// How wide one lane is, and how tall one row. The row height is what the middle
/// of a row — where the lines between lanes meet — is measured from.
const LANE: f32 = 14.;
const ROW_HEIGHT: f32 = 22.;

/// One commit's files, as its row opened onto them.
struct OpenCommit {
    id: String,
    /// What git said it changed, or why it would not say.
    files: Result<Vec<FileChange>, String>,
}

/// One row's place in the graph: which lane its dot is in, and which lanes the
/// lines through the row belong to.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct GraphCell {
    /// The lane the commit's dot is drawn in.
    pub lane: usize,
    /// Lanes whose line comes in from the top and ends at the dot: the branches
    /// that merged into this commit.
    pub into: Vec<usize>,
    /// Lanes the dot opens a line into at the bottom: this commit's other
    /// parents, which is what a merge is.
    pub from: Vec<usize>,
    /// Lanes that pass the row by without meeting it.
    pub through: Vec<usize>,
}

/// Place every commit in the graph. `commits` must be in `--topo-order`, where a
/// commit is always listed before its parents — which is what lets one pass over
/// the list know where every line goes.
///
/// A lane is one line: the lane a commit lands in is the one already waiting for
/// it, or a lane nothing is using any more. The lanes never shift, so a line
/// keeps its column for the whole width of the history.
pub(crate) fn cells(commits: &[Commit]) -> (Vec<GraphCell>, usize) {
    // Each lane is the commit id its line is waiting to arrive at, or `None` for
    // a lane that has finished and can be handed to the next branch.
    let mut lanes: Vec<Option<String>> = Vec::new();
    let mut out = Vec::with_capacity(commits.len());
    for commit in commits {
        let top = lanes.clone();
        // The lane that was waiting for this commit is its own; a commit nothing
        // was waiting for — the tip of a branch — starts one, in the first lane
        // a line has finished with.
        let waiting: Vec<usize> = top
            .iter()
            .enumerate()
            .filter(|(_, id)| id.as_deref() == Some(commit.id.as_str()))
            .map(|(lane, _)| lane)
            .collect();
        let lane = match waiting.first() {
            Some(lane) => *lane,
            None => match lanes.iter().position(Option::is_none) {
                Some(free) => {
                    lanes[free] = Some(commit.id.clone());
                    free
                }
                None => {
                    lanes.push(Some(commit.id.clone()));
                    lanes.len() - 1
                }
            },
        };
        // Every other lane waiting for it merged into it, and is over.
        for &merged in waiting.iter().skip(1) {
            lanes[merged] = None;
        }
        // This lane's line goes on to the first parent, or ends here.
        lanes[lane] = commit.parents.first().cloned();
        // And every other parent gets a lane of its own, drawn from the dot.
        let mut from = Vec::new();
        for parent in commit.parents.iter().skip(1) {
            let at = match lanes
                .iter()
                .position(|id| id.as_deref() == Some(parent.as_str()))
            {
                Some(existing) => existing,
                None => match lanes.iter().position(Option::is_none) {
                    Some(free) => {
                        lanes[free] = Some(parent.clone());
                        free
                    }
                    None => {
                        lanes.push(Some(parent.clone()));
                        lanes.len() - 1
                    }
                },
            };
            from.push(at);
        }
        // The row is read off the two states: what came in from above, what
        // carries on through, and what starts below.
        let at =
            |lanes: &[Option<String>], lane: usize| lanes.get(lane).cloned().flatten().is_some();
        let high = top.len().max(lanes.len());
        let mut cell = GraphCell {
            lane,
            ..Default::default()
        };
        for i in 0..high {
            if i == lane {
                continue;
            }
            match (at(&top, i), at(&lanes, i)) {
                (true, false) => cell.into.push(i),
                (true, true) => cell.through.push(i),
                (false, true) => cell.from.push(i),
                (false, false) => {}
            }
        }
        // An extra parent that took over a lane already running is a line from
        // the dot too, even though nothing opened or closed there.
        for i in from {
            if i != lane && !cell.from.contains(&i) {
                cell.from.push(i);
            }
        }
        out.push(cell);
    }
    // Lanes are only ever added to, so the list never grew past the widest row.
    (out, lanes.len().max(1))
}

/// The branch menu: which branch it is about, and where it was opened.
struct BranchMenu {
    branch: Branch,
    at: Point<Pixels>,
}

pub struct GitGraph {
    state: Entity<AppState>,
    /// The worktree's directory: what the paths git reports are relative to, and
    /// what a delete runs against.
    root: PathBuf,
    branches: Vec<Branch>,
    /// Each commit's branches, by commit id — a map, because the rows are drawn
    /// from it once per commit and a per-row scan of every branch is not.
    by_commit: BTreeMap<String, Vec<Branch>>,
    commits: Vec<Commit>,
    cells: Vec<GraphCell>,
    /// How many lanes the widest row has: the graph column's width.
    width: usize,
    /// The commit whose files are open under its row, if one is.
    open: Option<OpenCommit>,
    /// Why the last read failed, in git's own words.
    error: Option<String>,
    /// What the last delete said, or why git would not do it.
    notice: Option<String>,
    /// The branch git refused to delete. Drawn with the forced retry beside it:
    /// an unmerged branch is the one case git says no to, and the only thing to
    /// do about it is say yes again.
    refused: Option<String>,
    /// A delete is in flight, and the rows are not a place to start another.
    busy: bool,
    /// The branch a right-click opened a menu on, if one is up.
    menu: Option<BranchMenu>,
    /// Whether something moved under the graph while it was up: a worktree
    /// removed from the sidebar is a branch this view is still drawing.
    stale: bool,
    focus: FocusHandle,
    _task: Option<Task<()>>,
    /// Re-reads when the repository's worktrees move.
    _events: Subscription,
}

impl GitGraph {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        // A worktree that comes or goes is a branch this graph is drawing, and
        // the delete this view offers for one of them goes through the worktree
        // that holds it — so the sidebar's own removals have to reach it too.
        let events = cx.subscribe(&state, |this, _state, event: &CoreEvent, cx| match event {
            CoreEvent::WorktreeRemoved { .. }
            | CoreEvent::WorktreeCreated { .. }
            | CoreEvent::ActiveWorktreeChanged { .. } => {
                this.stale = true;
                cx.notify();
            }
            _ => {}
        });
        let mut view = Self {
            root: state
                .read(cx)
                .active()
                .map(|worktree| worktree.path.clone())
                .unwrap_or_default(),
            state,
            branches: Vec::new(),
            by_commit: BTreeMap::new(),
            commits: Vec::new(),
            cells: Vec::new(),
            width: 1,
            open: None,
            error: None,
            notice: None,
            refused: None,
            busy: false,
            menu: None,
            stale: false,
            focus: cx.focus_handle(),
            _task: None,
            _events: events,
        };
        view.refresh(cx);
        view
    }

    /// Read the branches and the history again. One `for-each-ref` and one
    /// `git log --all`, which is what the two halves of this view are.
    pub(crate) fn refresh(&mut self, cx: &mut Context<Self>) {
        // The graph is one worktree's repository, and which worktree that is can
        // move under it — a click in the sidebar behind the dialog is enough.
        if let Some(worktree) = self.state.read(cx).active() {
            self.root = worktree.path.clone();
        }
        match (
            git::branches(&self.root),
            git::graph_commits(&self.root, COMMITS),
        ) {
            (Ok(branches), Ok(commits)) => {
                let (cells, width) = cells(&commits);
                self.by_commit = branches.iter().fold(BTreeMap::new(), |mut map, branch| {
                    map.entry(branch.commit.clone())
                        .or_insert_with(Vec::new)
                        .push(branch.clone());
                    map
                });
                self.branches = branches;
                self.commits = commits;
                self.cells = cells;
                self.width = width;
                self.error = None;
                self.stale = false;
                // A commit the history no longer has — a delete, a rebase — has
                // no row to be open under.
                if self
                    .open
                    .as_ref()
                    .is_some_and(|open| !self.commits.iter().any(|commit| commit.id == open.id))
                {
                    self.open = None;
                }
            }
            (branches, commits) => {
                // git's own words: the branches' failure if there was one, the
                // log's otherwise, which for an empty repository is exactly
                // "does not have any commits yet".
                self.error = branches
                    .err()
                    .or_else(|| commits.err())
                    .map(|error| format!("{error:#}"));
            }
        }
        cx.notify();
    }

    /// Open a commit's files under its row, or close them if they are the ones
    /// already open. Asked for on the click, like the panel's: `git show` for the
    /// one row looked at, never for the four hundred above and below it.
    fn toggle_commit(&mut self, commit: &Commit, cx: &mut Context<Self>) {
        self.open = match &self.open {
            Some(open) if open.id == commit.id => None,
            _ => Some(OpenCommit {
                id: commit.id.clone(),
                files: git::commit_files(&self.root, &commit.id)
                    .map_err(|error| format!("{error:#}")),
            }),
        };
        cx.notify();
    }

    /// Ask before deleting: a branch is a name somebody chose, and the dialog is
    /// where the question is asked in full.
    ///
    /// Two of them ask something else, because the delete is something else.
    /// A remote branch is a `push --delete` at a remote — the one delete that
    /// changes something on somebody else's machine — and the question says so.
    /// A branch one of this app's worktrees has checked out is the common case in
    /// a window where every worktree is a branch, and git refuses to delete it —
    /// `-D` refuses it too — so that branch goes the way its worktree goes, and
    /// the workspace's own removal dialog is the one asked with, because it names
    /// the worktree *and* the branch it takes with it.
    fn ask_delete(&mut self, branch: &Branch, window: &mut Window, cx: &mut Context<Self>) {
        // git will not delete the branch it is on.
        if branch.current || self.busy {
            return;
        }
        if let Some(remote) = branch.remote.clone() {
            let name = branch.name.clone();
            let graph = cx.weak_entity();
            window.open_alert_dialog(cx, move |alert, _, _| {
                let confirm = remote.clone();
                let graph = graph.clone();
                alert
                    .title(t!("git.delete_remote_branch", branch = name).to_string())
                    .description(
                        t!(
                            "git.delete_remote_branch_question",
                            branch = name,
                            remote = remote.remote
                        )
                        .to_string(),
                    )
                    .confirm()
                    .ok_text(t!("git.delete"))
                    .ok_variant(ButtonVariant::Danger)
                    .on_ok(move |_, _, cx| {
                        let _ = graph.update(cx, |graph, cx| graph.delete_remote(&confirm, cx));
                        true
                    })
            });
            return;
        }
        let holder = self
            .state
            .read(cx)
            .worktrees()
            .iter()
            .find(|worktree| worktree.branch == branch.name)
            .map(|worktree| worktree.id.clone());
        if let Some(worktree_id) = holder {
            // Focused first, so the action reaches the workspace's element
            // listener from inside its subtree — the road the panel's own
            // button takes.
            window.focus(&self.focus, cx);
            window.dispatch_action(Box::new(worktree::AskRemove { worktree_id }), cx);
            return;
        }
        let name = branch.name.clone();
        let graph = cx.weak_entity();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let confirm = name.clone();
            let graph = graph.clone();
            alert
                .title(t!("git.delete_branch", branch = name).to_string())
                .description(t!("git.delete_branch_question", branch = name).to_string())
                .confirm()
                .ok_text(t!("git.delete"))
                .ok_variant(ButtonVariant::Danger)
                .on_ok(move |_, _, cx| {
                    let _ = graph.update(cx, |graph, cx| graph.delete(&confirm, false, cx));
                    true
                })
        });
    }

    /// Delete `branch`, on the background executor: git is a child process, and
    /// the window is not waiting for it. `force` is the second ask, after git has
    /// refused once — see [`Self::ask_delete`].
    fn delete(&mut self, branch: &str, force: bool, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        self.busy = true;
        self.refused = None;
        self.notice = Some(t!("git.deleting", branch = branch).to_string());
        cx.notify();
        let (root, name) = (self.root.clone(), branch.to_string());
        let task = cx.spawn(async move |this, cx| {
            let deleting = name.clone();
            let (result, held) = cx
                .background_executor()
                .spawn(async move {
                    // Which worktree holds it, asked on the way in: a branch one
                    // holds cannot be deleted by any flag, and the forced retry
                    // beside git's refusal would be a button that always fails.
                    let held = git::branch_worktree(&root, &deleting)
                        .map(|holder| holder.is_some())
                        .unwrap_or(false);
                    (git::branch_delete(&root, &deleting, force), held)
                })
                .await;
            this.update(cx, |graph, cx| {
                graph.busy = false;
                match result {
                    Ok(()) => {
                        graph.notice = Some(t!("git.branch_deleted", branch = name).to_string());
                        graph.refused = None;
                    }
                    Err(error) => {
                        // git's own refusal, and the one thing that answers it —
                        // unless there is no thing that answers it.
                        graph.notice = Some(format!("{error:#}"));
                        graph.refused = (!held).then(|| name.clone());
                    }
                }
                graph.refresh(cx);
            })
            .ok();
        });
        self._task = Some(task);
    }

    /// Delete `branch` on its remote: a `push --delete`, which is the one delete
    /// that changes something on somebody else's machine. On the background
    /// executor, like the local one, and with no forced retry: there is no flag
    /// that talks a remote into keeping a branch it has refused to lose.
    fn delete_remote(&mut self, remote: &RemoteRef, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        self.busy = true;
        self.refused = None;
        let deleting = format!("{}/{}", remote.remote, remote.branch);
        self.notice = Some(t!("git.deleting", branch = deleting).to_string());
        cx.notify();
        let (root, remote) = (self.root.clone(), remote.clone());
        let task = cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(
                    async move { git::remote_branch_delete(&root, &remote.remote, &remote.branch) },
                )
                .await;
            this.update(cx, |graph, cx| {
                graph.busy = false;
                graph.notice = Some(match result {
                    Ok(said) if said.is_empty() => {
                        t!("git.branch_deleted", branch = deleting).to_string()
                    }
                    Ok(said) => said,
                    // The remote's own words: a protected branch, a hook, a
                    // remote that is not there.
                    Err(error) => format!("{error:#}"),
                });
                graph.refresh(cx);
            })
            .ok();
        });
        self._task = Some(task);
    }

    /// The way out of a refusal git made: the same delete, asked force.
    fn force_button(&self, colors: &ColorTokens, cx: &mut Context<Self>) -> AnyElement {
        let Some(branch) = self.refused.clone() else {
            return div().into_any_element();
        };
        div()
            .id("git-graph-force")
            .test_support()
            .aria_label(t!("git.delete_anyway", branch = branch).to_string())
            .flex_shrink_0()
            .px_1()
            .rounded_sm()
            .border_1()
            .border_color(colors.destructive)
            .text_color(colors.destructive)
            .cursor_pointer()
            .hover(|el| el.bg(colors.muted))
            .child(t!("git.delete_anyway", branch = branch))
            .on_click(cx.listener(move |this, _event, _window, cx| this.delete(&branch, true, cx)))
            .into_any_element()
    }

    fn refresh_button(&self, colors: &ColorTokens, cx: &mut Context<Self>) -> AnyElement {
        div()
            .id("git-graph-refresh")
            .test_support()
            .aria_label(t!("git.refresh_graph"))
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

    /// The way out: the graph covers the window, and this is the button that
    /// gives it back. Escape does the same, and is listed nowhere.
    fn close_button(&self, colors: &ColorTokens, cx: &mut Context<Self>) -> AnyElement {
        div()
            .id("git-graph-close")
            .test_support()
            .aria_label(t!("git.close_graph"))
            .h_flex()
            .items_center()
            .justify_center()
            .size(px(22.))
            .rounded_sm()
            .cursor_pointer()
            .text_color(colors.muted_foreground)
            .hover(|el| el.bg(colors.muted))
            .child(Icon::new(IconName::Close).size(px(14.)))
            .on_click(cx.listener(|_this, _event, window, cx| {
                window.dispatch_action(Box::new(crate::state::gitgraph::Close), cx);
            }))
            .into_any_element()
    }

    /// The row over the table: what is in it, and the way to ask git again.
    fn toolbar(&self, colors: &ColorTokens, cx: &mut Context<Self>) -> AnyElement {
        let mut row = div()
            .id("git-graph-toolbar")
            .test_support()
            .h_flex()
            .items_center()
            .justify_between()
            .gap_2()
            .w_full()
            .px_2()
            .h(px(ROW_HEIGHT + 8.))
            .border_b_1()
            .border_color(colors.border)
            .child(
                div()
                    .min_w_0()
                    .overflow_hidden()
                    .text_ellipsis()
                    .text_xs()
                    .text_color(colors.muted_foreground)
                    .child(SharedString::from(
                        t!(
                            "git.graph_summary",
                            branches = self.branches.len(),
                            commits = self.commits.len()
                        )
                        .to_string(),
                    )),
            )
            .child(
                div()
                    .h_flex()
                    .items_center()
                    .gap_1()
                    .flex_shrink_0()
                    .child(self.refresh_button(colors, cx))
                    .child(self.close_button(colors, cx)),
            );
        if let Some(notice) = self.notice.clone() {
            row = row.child(
                div()
                    .id("git-graph-notice")
                    .test_support()
                    .role(Role::Status)
                    .aria_label(SharedString::from(notice.clone()))
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .text_ellipsis()
                    .text_xs()
                    .text_color(colors.muted_foreground)
                    .child(SharedString::from(notice)),
            );
        }
        if self.refused.is_some() {
            row = row.child(self.force_button(colors, cx));
        }
        row.into_any_element()
    }

    /// The names over the columns. The widths are the rows' own, so a column is
    /// as wide as its name says it is.
    fn head_row(&self, colors: &ColorTokens) -> AnyElement {
        div()
            .id("git-graph-head")
            .test_support()
            .h_flex()
            .items_center()
            .w_full()
            .h(px(ROW_HEIGHT))
            .pr_2()
            .border_b_1()
            .border_color(colors.border)
            .text_xs()
            .text_color(colors.muted_foreground)
            .child(div().flex_shrink_0().w(px(graph_width(self.width))))
            .child(div().flex_1().min_w_0().child(t!("git.graph_description")))
            .child(
                div()
                    .flex_shrink_0()
                    .w(px(DATE_WIDTH))
                    .child(t!("git.graph_date")),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .w(px(AUTHOR_WIDTH))
                    .overflow_hidden()
                    .text_ellipsis()
                    .child(t!("git.graph_author")),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .w(px(COMMIT_WIDTH))
                    .child(t!("git.graph_commit")),
            )
            .into_any_element()
    }

    /// One lane's line, or the horizontal piece of a corner: everything the graph
    /// cell is drawn from is a box.
    fn segment(&self, left: f32, top: f32, width: f32, height: f32, color: Hsla) -> AnyElement {
        div()
            .absolute()
            .left(px(left))
            .top(px(top))
            .w(px(width))
            .h(px(height))
            .bg(color)
            .into_any_element()
    }

    /// The lines and the dot of one row.
    fn graph_cell(&self, cell: &GraphCell, cx: &App) -> AnyElement {
        let themes = cx.theme();
        let palette = [
            themes.blue,
            themes.green,
            themes.yellow,
            themes.magenta,
            themes.cyan,
            themes.red,
        ];
        let color = |lane: usize| palette[lane % palette.len()];
        let x = |lane: usize| lane as f32 * LANE + LANE / 2.;
        let mid = ROW_HEIGHT / 2.;
        let mut graph = div()
            .relative()
            .flex_shrink_0()
            .h_full()
            .w(px(graph_width(self.width)));
        // The corners first, so the dot lands on top of the lines it joins.
        for &lane in cell.into.iter().chain(cell.from.iter()) {
            let (left, right) = (x(lane).min(x(cell.lane)), x(lane).max(x(cell.lane)));
            graph = graph.child(self.segment(
                left - 0.5,
                mid - 0.5,
                right - left + 1.,
                1.,
                color(lane),
            ));
        }
        // A line coming in from above ends at the middle of the row...
        for &lane in &cell.into {
            graph = graph.child(self.segment(x(lane) - 0.5, 0., 1., mid, color(lane)));
        }
        // ...and one starting below begins there.
        for &lane in &cell.from {
            graph =
                graph.child(self.segment(x(lane) - 0.5, mid, 1., ROW_HEIGHT - mid, color(lane)));
        }
        // The lanes that are not this commit's pass the whole row by.
        for &lane in &cell.through {
            graph = graph.child(self.segment(x(lane) - 0.5, 0., 1., ROW_HEIGHT, color(lane)));
        }
        // The dot's own line, and the dot.
        graph = graph.child(self.segment(x(cell.lane) - 0.5, 0., 1., ROW_HEIGHT, color(cell.lane)));
        graph
            .child(
                div()
                    .absolute()
                    .left(px(x(cell.lane) - 3.5))
                    .top(px(mid - 3.5))
                    .size(px(7.))
                    .rounded_full()
                    .bg(color(cell.lane)),
            )
            .into_any_element()
    }

    /// One branch as a pill: the one `HEAD` is on is drawn brighter, and every
    /// other local one carries the way to delete it — which is the whole of what
    /// "editing branches" means here.
    fn branch_chip(
        &self,
        branch: &Branch,
        colors: &ColorTokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // A remote branch is a name on a remote, and it is not the one `HEAD` is
        // on: everything but the current branch can be offered a delete.
        let deletable = !branch.current && !self.busy;
        let mut chip = div()
            .id(SharedString::from(format!(
                "git-graph-branch-{}",
                branch.name
            )))
            .test_support()
            .aria_label(SharedString::from(branch.name.clone()))
            .h_flex()
            .items_center()
            .gap_0p5()
            .flex_shrink_0()
            .px_1()
            .rounded_full()
            .border_1()
            .border_color(if branch.current {
                colors.primary
            } else {
                colors.border
            })
            .text_color(if branch.current {
                colors.primary
            } else {
                colors.muted_foreground
            })
            .text_xs()
            .child(SharedString::from(branch.name.clone()));
        if deletable {
            let wanted = branch.clone();
            chip = chip.child(
                div()
                    .id(SharedString::from(format!(
                        "git-graph-delete-{}",
                        branch.name
                    )))
                    .test_support()
                    .aria_label(t!("git.delete_branch", branch = branch.name).to_string())
                    .rounded_full()
                    .px_0p5()
                    .cursor_pointer()
                    .hover(|el| el.bg(colors.border))
                    .child(SharedString::from("\u{00d7}"))
                    .on_click(cx.listener(move |this, _event, window, cx| {
                        // The row's own click opens the commit's files, which is
                        // not what this one is asking for.
                        cx.stop_propagation();
                        this.ask_delete(&wanted, window, cx);
                    })),
            );
        }
        // And the other way in: a right-click on the name, where the operations
        // about the branch itself live.
        let wanted = branch.clone();
        chip.on_mouse_down(
            MouseButton::Right,
            cx.listener(move |this, event, window, cx| {
                cx.stop_propagation();
                this.open_branch_menu(wanted.clone(), event, window, cx);
            }),
        )
        .into_any_element()
    }

    /// Right-click a branch's name: the operations that are about *it* rather
    /// than about the commit it sits on. The branch `HEAD` is on is the one
    /// thing with nothing here — git will not delete it — so it gets no menu
    /// rather than an empty one.
    fn open_branch_menu(
        &mut self,
        branch: Branch,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if branch.current || self.busy {
            return;
        }
        self.menu = Some(BranchMenu {
            branch,
            at: event.position,
        });
        // The keyboard goes to the graph, so Escape closes the menu before it
        // closes the graph — and the graph's own listener is where that is
        // decided.
        window.focus(&self.focus, cx);
        cx.notify();
    }

    fn close_menu(&mut self, cx: &mut Context<Self>) {
        self.menu = None;
        cx.notify();
    }

    /// Escape closes the menu, and only then the graph: the menu is the innermost
    /// thing that is open. It stops here only while there is a menu to close.
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

    /// The branch menu, in a layer above the graph: a menu drawn in the row
    /// itself would be clipped by the list the row is scrolling in.
    fn menu_overlay(
        &self,
        window_size: Size<Pixels>,
        colors: &ColorTokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(menu) = self.menu.as_ref() else {
            return div().into_any_element();
        };
        let this = cx.entity().downgrade();
        let row_view = this.clone();
        let branch = menu.branch.clone();
        let label = t!("git.delete_branch", branch = branch.name).to_string();
        let panel = div()
            .id("git-graph-branch-menu")
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
            .on_scroll_wheel(|_event, _window, cx| cx.stop_propagation())
            .child(
                div()
                    .id("git-graph-branch-menu-delete")
                    .test_support()
                    .aria_label(SharedString::from(label.clone()))
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .cursor_pointer()
                    .hover(|el| el.bg(colors.muted))
                    .child(SharedString::from(label))
                    .on_click(move |_event, window, cx| {
                        row_view
                            .update(cx, |graph, cx| {
                                graph.menu = None;
                                // The one question a branch is worth asking: the
                                // delete is confirmed before it runs, and a branch
                                // that will not go offers the forced retry.
                                graph.ask_delete(&branch, window, cx);
                            })
                            .ok();
                    }),
            );

        // The scrim: a click anywhere outside the menu, on either button, closes
        // it — which is what makes the menu feel like a menu.
        let dismiss = this.clone();
        deferred(
            anchored().position(point(px(0.), px(0.))).child(
                div()
                    .id("git-graph-branch-scrim")
                    .test_support()
                    .w(window_size.width)
                    .h(window_size.height)
                    // Blocks the mouse from everything behind it: the rows under
                    // the menu are not hovered and not clicked while it is up.
                    .occlude()
                    .on_mouse_down(MouseButton::Left, move |_event, _window, cx| {
                        dismiss.update(cx, |graph, cx| graph.close_menu(cx)).ok();
                    })
                    .on_mouse_down(MouseButton::Right, {
                        let dismiss = this.clone();
                        move |_event, _window, cx| {
                            dismiss.update(cx, |graph, cx| graph.close_menu(cx)).ok();
                        }
                    })
                    .child(
                        anchored()
                            .position(menu.at)
                            .anchor(Anchor::TopLeft)
                            .snap_to_window_with_margin(px(8.))
                            .child(panel),
                    ),
            ),
        )
        .with_priority(gpui_kit::base::POPUP_PRIORITY)
        .into_any_element()
    }

    /// One commit: the graph, the message and the names pointing at it, and the
    /// columns git printed.
    fn commit_row(
        &self,
        index: usize,
        commit: &Commit,
        colors: &ColorTokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut description = div()
            .flex_1()
            .min_w_0()
            .h_flex()
            .items_center()
            .gap_1()
            .overflow_hidden()
            .child(
                div()
                    .min_w_0()
                    .overflow_hidden()
                    .text_ellipsis()
                    .child(SharedString::from(commit.summary.clone())),
            );
        for branch in self
            .by_commit
            .get(&commit.id)
            .map(Vec::as_slice)
            .unwrap_or_default()
        {
            description = description.child(self.branch_chip(branch, colors, cx));
        }
        let wanted = commit.clone();
        div()
            .id(SharedString::from(format!(
                "git-graph-commit-{}",
                commit.short
            )))
            .test_support()
            .aria_label(SharedString::from(commit.summary.clone()))
            .h_flex()
            .items_center()
            .w_full()
            .h(px(ROW_HEIGHT))
            .pr_2()
            .text_xs()
            .cursor_pointer()
            .hover(|el| el.bg(colors.muted))
            .child(self.graph_cell(&self.cells[index], cx))
            .child(description)
            .child(
                div()
                    .flex_shrink_0()
                    .w(px(DATE_WIDTH))
                    .text_color(colors.muted_foreground)
                    .child(SharedString::from(session::relative_time(
                        session::now_ms(),
                        commit.at.saturating_mul(1000),
                    ))),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .w(px(AUTHOR_WIDTH))
                    .overflow_hidden()
                    .text_ellipsis()
                    .text_color(colors.muted_foreground)
                    .child(SharedString::from(commit.author.clone())),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .w(px(COMMIT_WIDTH))
                    .text_color(colors.muted_foreground)
                    .child(SharedString::from(commit.short.clone())),
            )
            .on_click(cx.listener(move |this, _event, _window, cx| this.toggle_commit(&wanted, cx)))
            .into_any_element()
    }
}

/// How wide the graph column is: one lane per line, plus the room the outermost
/// dot needs on either side.
fn graph_width(lanes: usize) -> f32 {
    LANE * lanes as f32 + LANE / 2.
}

/// The three columns after the description, in pixels: an age, an author, and a
/// hash. Fixed rather than fitted to their contents, so every row's columns line
/// up with every other row's.
const DATE_WIDTH: f32 = 90.;
const AUTHOR_WIDTH: f32 = 150.;
const COMMIT_WIDTH: f32 = 70.;

impl Render for GitGraph {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // A worktree that moved under this view — a branch published, a worktree
        // removed — is a graph that is no longer true.
        if self.stale {
            self.refresh(cx);
        }
        let colors = Theme::global(cx).tokens.colors.clone();
        let body = div()
            .id("git-graph")
            .test_support()
            .track_focus(&self.focus)
            // The branch menu is the only thing this view listens for a key
            // about; everything else is the workspace's.
            .on_key_down(cx.listener(Self::handle_key_down))
            .v_flex()
            .size_full()
            .min_h_0()
            .w_full()
            .child(self.toolbar(&colors, cx))
            .child(self.head_row(&colors));
        // The menu outlives the early returns below: a branch can be deleted from
        // an empty history too, and the overlay is not part of the rows.
        let menu = self.menu_overlay(window.viewport_size(), &colors, cx);

        if let Some(error) = self.error.clone() {
            return body
                .child(
                    div()
                        .id("git-graph-error")
                        .test_support()
                        .role(Role::Alert)
                        .aria_label(SharedString::from(error.clone()))
                        .px_2()
                        .py_1()
                        .text_xs()
                        .text_color(colors.destructive)
                        .child(SharedString::from(error)),
                )
                .child(menu)
                .into_any_element();
        }
        if self.commits.is_empty() {
            return body
                .child(
                    div()
                        .id("git-graph-empty")
                        .test_support()
                        .aria_label(t!("git.no_commits"))
                        .px_2()
                        .py_1()
                        .text_xs()
                        .text_color(colors.muted_foreground)
                        .child(t!("git.no_commits")),
                )
                .child(menu)
                .into_any_element();
        }

        // The rows scroll inside the view: the toolbar and the column names stay
        // where they are while a long history goes by.
        let mut rows = div()
            .id("git-graph-rows")
            .test_support()
            .v_flex()
            .flex_1()
            .min_h_0()
            .w_full()
            .overflow_y_scroll();
        for (index, commit) in self.commits.iter().enumerate() {
            rows = rows.child(self.commit_row(index, commit, &colors, cx));
            if let Some(open) = self.open.as_ref().filter(|open| open.id == commit.id) {
                rows = rows.child(match &open.files {
                    Err(error) => under_commit(
                        SharedString::from(format!("git-graph-error-{}", commit.short)),
                        SharedString::from(error.clone()),
                        colors.destructive,
                    ),
                    Ok(files) if files.is_empty() => under_commit(
                        SharedString::from(format!("git-graph-empty-{}", commit.short)),
                        SharedString::from(t!("git.commit_empty").to_string()),
                        colors.muted_foreground,
                    ),
                    Ok(files) => {
                        let mut block = div()
                            .id(SharedString::from(format!(
                                "git-graph-open-{}",
                                commit.short
                            )))
                            .test_support()
                            .v_flex()
                            .w_full()
                            .pb_1();
                        for change in files.iter().take(30) {
                            block = block.child(commit_file_row(
                                &commit.id,
                                change,
                                &colors,
                                self.focus.clone(),
                            ));
                        }
                        block.into_any_element()
                    }
                });
            }
        }
        body.child(rows).child(menu).into_any_element()
    }
}

/// One line under an open commit, indented past the graph column: the small print
/// the panel uses for the same asides.
fn under_commit(id: SharedString, text: SharedString, colour: Hsla) -> AnyElement {
    div()
        .id(id)
        .test_support()
        .pl(px(graph_width(3)))
        .pr_2()
        .text_xs()
        .text_color(colour)
        .child(text)
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::cells;
    use syro_core::git::Commit;

    fn commit(id: &str, parents: &[&str]) -> Commit {
        Commit {
            id: id.to_string(),
            short: id.to_string(),
            summary: id.to_string(),
            author: "Ada".into(),
            at: 0,
            refs: Vec::new(),
            parents: parents.iter().map(|parent| parent.to_string()).collect(),
        }
    }

    /// A plain history is a straight line: one lane, one dot per row, nothing
    /// joining or leaving.
    #[test]
    fn a_line_of_commits_is_one_lane() {
        let log = [commit("c", &["b"]), commit("b", &["a"]), commit("a", &[])];
        let (cells, width) = cells(&log);
        assert_eq!(width, 1);
        for cell in &cells {
            assert_eq!((cell.lane, cell.into.len(), cell.from.len()), (0, 0, 0));
            assert!(cell.through.is_empty());
        }
    }

    /// Two branches off one commit: the second tip opens a lane of its own, and
    /// the row where the histories meet draws a line between them.
    #[test]
    fn a_merge_draws_a_line_back_into_the_branch_it_merged() {
        // `m` is a merge of `a` (on main) and `b` (on the other branch).
        let log = [
            commit("m", &["a", "b"]),
            commit("a", &["root"]),
            commit("b", &["root"]),
            commit("root", &[]),
        ];
        let (cells, width) = cells(&log);
        assert_eq!(width, 2, "a merge needs two lanes");
        // The merge commit is on lane 0 and opens lane 1 for its second parent.
        assert_eq!(cells[0].lane, 0);
        assert_eq!(cells[0].from, [1]);
        // `a` carries lane 0 on; `b` is the first row in lane 1.
        assert_eq!(cells[1].lane, 0);
        assert_eq!(cells[2].lane, 1);
        // And the two lanes end at the commit they share, one of them drawn back
        // into the other.
        let root = &cells[3];
        assert_eq!(root.lane, 0);
        assert_eq!(root.into, [1]);
        assert!(root.from.is_empty());
    }

    /// A branch that ends where another one is waiting for the same commit hands
    /// its lane back, so a history with many short branches does not grow a lane
    /// per branch.
    #[test]
    fn a_finished_lane_is_reused_by_the_next_branch() {
        // Two tips off the same root, each one commit long.
        let log = [
            commit("tip-a", &["root"]),
            commit("tip-b", &["root"]),
            commit("root", &[]),
        ];
        let (cells, width) = cells(&log);
        assert_eq!(width, 2, "the two tips are both above the root");
        // The root is where both lanes meet: one of them merges into the other.
        assert_eq!(cells[2].lane, 0);
        assert_eq!(cells[2].into, [1]);
        assert_eq!(cells[0].lane, 0);
        assert_eq!(cells[1].lane, 1);
    }
}
