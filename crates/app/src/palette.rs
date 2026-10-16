//! the two lists: files, and the worktrees to jump to.

use std::collections::BTreeMap;
use std::path::Path;
use std::rc::Rc;

use gpui_kit::base::{StyledExt as _, TestSupportExt as _, Theme};
use gpui_kit::component::WindowExt as _;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rust_i18n::t;
use syro_core::model::{RepoId, WorktreeId};
use syro_core::session::Session;

use crate::Workspace;
use crate::session_panel;
use crate::state::{diff, editor, repo, tab, terminal, worktree};

/// How many rows are drawn.
const MAX_ROWS: usize = 50;

// ---------------------------------------------------------------- ranking

/// How well `candidate` answers `query`: lower is better, `None` is no match.
pub fn score(query: &str, candidate: &str) -> Option<i32> {
    if query.is_empty() {
        return Some(0);
    }
    let query = query.to_lowercase();
    let candidate = candidate.to_lowercase();
    let haystack: Vec<char> = candidate.chars().collect();
    let needle: Vec<char> = query.chars().collect();

    let mut score = 0i32;
    let mut position = 0usize;
    let mut last: Option<usize> = None;

    for character in needle {
        // The earliest match at or after the cursor, which is what keeps the
        // score about this query rather than a luckier alignment later in the
        // string.
        let found = haystack[position..]
            .iter()
            .position(|other| *other == character)
            .map(|offset| position + offset)?;

        if let Some(previous) = last {
            score += (found - previous - 1) as i32;
        }
        if found > 0 && matches!(haystack[found - 1], '/' | '.' | '-' | '_') {
            score -= 5;
        }
        last = Some(found);
        position = found + 1;
    }

    let file_name = candidate.rsplit('/').next().unwrap_or(candidate.as_str());
    if file_name.starts_with(&query) {
        score -= 200;
    } else if file_name.contains(&query) {
        score -= 100;
    }
    Some(score)
}

/// The indices of `candidates` that match `query`, best first.
pub fn rank(query: &str, candidates: &[String]) -> Vec<usize> {
    let mut scored: Vec<(i32, usize)> = Vec::new();
    for (index, candidate) in candidates.iter().enumerate() {
        if let Some(score) = score(query, candidate) {
            scored.push((score, index));
        }
    }
    scored.sort_by(|left, right| left.0.cmp(&right.0).then(left.1.cmp(&right.1)));
    scored.into_iter().map(|(_, index)| index).collect()
}

// ---------------------------------------------------------------- the table

/// What running a command has to hand: the workspace, and the context the item list was built
/// in.
#[derive(Clone)]
pub struct Ctx {
    pub workspace: WeakEntity<Workspace>,
    pub active_worktree: Option<WorktreeId>,
    pub active_repo: Option<RepoId>,
    /// The tab on screen, for the commands that act on one.
    pub active_tab: Option<usize>,
}

impl Ctx {
    /// Run `f` against the workspace, if it is still alive. The window is the
    /// one the palette is in: an element listener has it, and an app-level
    /// listener defers until it does (see [`apply_keymap`]).
    fn with_workspace(
        &self,
        window: &mut Window,
        cx: &mut App,
        f: impl FnOnce(&mut Workspace, &mut Window, &mut Context<Workspace>),
    ) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        workspace.update(cx, |workspace, cx| f(workspace, window, cx));
    }
}

/// One command: what a keymap calls it, what the palette draws, and what it does.
pub struct ActionSpec {
    /// the spelling, and what `[keymap]` names — `"terminal::New"`.
    pub name: &'static str,
    pub dispatch: fn(&mut Window, &mut App, &Ctx),
}

/// Every action a `[keymap]` entry may name, the ones about the app first and
/// then the ones about what is on screen. This table is the keymap's vocabulary
/// and nothing else: a command with no key of its own reaches the app through
/// its menu, its icon or its panel.
pub fn actions() -> Vec<&'static ActionSpec> {
    const TABLE: &[ActionSpec] = &[
        ActionSpec {
            name: "repo::Add",
            dispatch: |window, cx, _ctx| {
                window.dispatch_action(Box::new(repo::AskAdd), cx);
            },
        },
        ActionSpec {
            name: "worktree::New",
            dispatch: |window, cx, ctx| {
                // The form is a view, and the workspace is where views live; the
                // sidebar's `+` reaches it through the workspace's element
                // listener, which a dialog's button is not inside of.
                let Some(repo_id) = ctx.active_repo.clone() else {
                    return;
                };
                ctx.with_workspace(window, cx, |workspace, window, cx| {
                    workspace.open_new_worktree_form(&repo_id, window, cx)
                });
            },
        },
        ActionSpec {
            name: "worktree::AskRemove",
            dispatch: |window, cx, ctx| {
                let Some(worktree_id) = ctx.active_worktree.clone() else {
                    return;
                };
                ctx.with_workspace(window, cx, |workspace, window, cx| {
                    workspace.open_remove_worktree_form(&worktree_id, window, cx)
                });
            },
        },
        // Beside the worktree removal, because it is the other half of the same
        // question — and because the repo's own checkout *is* this one: the `×`
        // that cannot be a worktree removal ends in a project removal.
        ActionSpec {
            name: "repo::Remove",
            dispatch: |window, cx, ctx| {
                // Nothing on disk is touched; see `AppState::remove_repo`.
                let Some(repo_id) = ctx.active_repo.clone() else {
                    return;
                };
                window.dispatch_action(Box::new(repo::Remove { repo_id }), cx);
            },
        },
        ActionSpec {
            name: "terminal::Add",
            dispatch: |window, cx, _ctx| {
                window.dispatch_action(Box::new(terminal::Add), cx);
            },
        },
        ActionSpec {
            name: "terminal::New",
            dispatch: |window, cx, ctx| {
                let Some(worktree_id) = ctx.active_worktree.clone() else {
                    return;
                };
                window.dispatch_action(
                    Box::new(terminal::New {
                        worktree_id,
                        agent: None,
                        resume: None,
                        seed: None,
                        model: None,
                    }),
                    cx,
                );
            },
        },
        ActionSpec {
            name: "diff::Open",
            dispatch: |window, cx, _ctx| {
                window.dispatch_action(Box::new(diff::Open), cx);
            },
        },
        ActionSpec {
            name: "editor::Save",
            dispatch: |window, cx, _ctx| {
                window.dispatch_action(Box::new(editor::Save), cx);
            },
        },
        ActionSpec {
            name: "tab::Close",
            dispatch: |window, cx, ctx| {
                let (Some(worktree_id), Some(index)) =
                    (ctx.active_worktree.clone(), ctx.active_tab)
                else {
                    return;
                };
                window.dispatch_action(Box::new(tab::Close { worktree_id, index }), cx);
            },
        },
        // the split, with the keys a terminal app uses (group "Terminal Panes").
        ActionSpec {
            name: "split::Right",
            dispatch: |window, cx, _ctx| {
                window.dispatch_action(Box::new(crate::split::Right), cx);
            },
        },
        ActionSpec {
            name: "split::Down",
            dispatch: |window, cx, _ctx| {
                window.dispatch_action(Box::new(crate::split::Down), cx);
            },
        },
        ActionSpec {
            name: "split::ClosePane",
            dispatch: |window, cx, _ctx| {
                window.dispatch_action(Box::new(crate::split::ClosePane), cx);
            },
        },
        ActionSpec {
            name: "settings::Open",
            dispatch: |window, cx, ctx| {
                ctx.with_workspace(window, cx, |workspace, window, cx| {
                    workspace.open_settings_form(window, cx)
                });
            },
        },
        ActionSpec {
            name: "workspace::Refresh",
            dispatch: |window, cx, _ctx| {
                window.dispatch_action(Box::new(crate::state::Refresh), cx);
            },
        },
        // The right sidebar's three panels, which have no keys of their own: they
        // are behind the activity strip's icons.
        ActionSpec {
            name: "panel::ShowFiles",
            dispatch: |window, cx, _ctx| {
                window.dispatch_action(
                    Box::new(crate::right_sidebar::Show {
                        panel: crate::right_sidebar::Panel::Files,
                    }),
                    cx,
                );
            },
        },
        ActionSpec {
            name: "panel::ShowSessions",
            dispatch: |window, cx, _ctx| {
                window.dispatch_action(
                    Box::new(crate::right_sidebar::Show {
                        panel: crate::right_sidebar::Panel::Sessions,
                    }),
                    cx,
                );
            },
        },
        ActionSpec {
            name: "panel::ShowGit",
            dispatch: |window, cx, _ctx| {
                window.dispatch_action(
                    Box::new(crate::right_sidebar::Show {
                        panel: crate::right_sidebar::Panel::Git,
                    }),
                    cx,
                );
            },
        },
    ];

    let list: Vec<&'static ActionSpec> = TABLE.iter().collect();

    list
}

/// The command with this `[keymap]` name.
pub fn action_by_name(name: &str) -> Option<&'static ActionSpec> {
    actions().into_iter().find(|spec| spec.name == name)
}

/// What is on screen, as a title for the palette's row: the first line is the
/// title, then a keyword, then the detail the caller passes.
#[derive(Clone)]
pub struct Item {
    pub title: SharedString,
    pub detail: Option<SharedString>,
    pub group: SharedString,
    /// Run by a click or by Enter. `Fn`, not `FnMut`: a row cannot be run twice
    /// by the same keystroke, and a command that ran once must not be re-runnable
    /// through state it kept.
    run: Rc<dyn Fn(&mut Window, &mut App)>,
}

impl Item {
    pub fn new(
        title: impl Into<SharedString>,
        run: impl Fn(&mut Window, &mut App) + 'static,
    ) -> Self {
        Self {
            title: title.into(),
            detail: None,
            group: SharedString::default(),
            run: Rc::new(run),
        }
    }

    pub fn detail(mut self, detail: impl Into<SharedString>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    pub fn group(mut self, group: impl Into<SharedString>) -> Self {
        self.group = group.into();
        self
    }
}

/// The files quick open lists: the active worktree's tracked files, and what opening one does.
pub fn file_items(ctx: &Ctx, root: &Path, paths: Vec<String>) -> Vec<Item> {
    paths
        .into_iter()
        .map(|path| {
            let ctx = ctx.clone();
            let absolute = root.join(&path);
            Item::new(
                SharedString::from(file_name_of(&path)),
                move |window, cx| {
                    let path = absolute.clone();
                    ctx.with_workspace(window, cx, |workspace, window, cx| {
                        workspace.open_path(path.clone(), None, window, cx)
                    });
                },
            )
            .group(t!("palette.group.file"))
            .detail(path)
        })
        .collect()
}

/// The worktrees of every repository, for the jump. The repository is the
/// detail line, because two repositories can hold the same name.
pub fn worktree_items(
    ctx: &Ctx,
    worktrees: Vec<(WorktreeId, String, String, String)>,
) -> Vec<Item> {
    worktrees
        .into_iter()
        .map(|(id, repo, name, branch)| {
            let ctx = ctx.clone();
            let active = ctx.active_worktree.as_deref() == Some(id.as_str());
            Item::new(
                SharedString::from(if active {
                    t!("palette.active", name = name).to_string()
                } else {
                    name.clone()
                }),
                move |window, cx| {
                    window.dispatch_action(
                        Box::new(worktree::Activate {
                            worktree_id: id.clone(),
                        }),
                        cx,
                    );
                },
            )
            .group(t!("palette.group.worktree"))
            .detail(SharedString::from(format!("{repo} · {branch}")))
        })
        .collect()
}

/// The past agent sessions the last scan found, for the global search: the
/// session's own title, with the agent and the directory it worked in as the
/// detail — two sessions can share a title, and the directory is what tells
/// them apart.
pub fn session_items(sessions: Vec<Session>) -> Vec<Item> {
    sessions
        .into_iter()
        .map(|session| {
            let title = if session.title.trim().is_empty() {
                session.id.clone()
            } else {
                session.title.clone()
            };
            let detail = format!("{} · {}", session.agent.display(), session.cwd.display());
            Item::new(SharedString::from(title), move |window, cx| {
                window.dispatch_action(
                    Box::new(session_panel::Resume {
                        session: session.clone(),
                    }),
                    cx,
                );
            })
            .group(t!("palette.group.session"))
            .detail(SharedString::from(detail))
        })
        .collect()
}

/// The projects, for the global search. Selecting one goes to it, which is its
/// own checkout where it has one and its first worktree otherwise — the closest
/// a flat list comes to opening a project.
pub fn repo_items(
    ctx: &Ctx,
    repos: Vec<(RepoId, String, String, Option<WorktreeId>)>,
) -> Vec<Item> {
    repos
        .into_iter()
        .filter_map(|(id, name, path, target)| {
            let active = ctx.active_repo.as_deref() == Some(id.as_str());
            let target = target?;
            Some(
                Item::new(
                    SharedString::from(if active {
                        t!("palette.active", name = name).to_string()
                    } else {
                        name.clone()
                    }),
                    move |window, cx| {
                        window.dispatch_action(
                            Box::new(worktree::Activate {
                                worktree_id: target.clone(),
                            }),
                            cx,
                        );
                    },
                )
                .group(t!("palette.group.project"))
                .detail(SharedString::from(path)),
            )
        })
        .collect()
}

fn file_name_of(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).to_string()
}

// ---------------------------------------------------------------- keybindings

/// The list's own keys, bound in the `palette` context so the dialog's Enter
/// does not take them first.
#[derive(Clone, PartialEq, Debug, Action)]
#[action(namespace = palette, no_json)]
pub struct Next;

#[derive(Clone, PartialEq, Debug, Action)]
#[action(namespace = palette, no_json)]
pub struct Previous;

#[derive(Clone, PartialEq, Debug, Action)]
#[action(namespace = palette, no_json)]
pub struct Confirm;

/// Open one of the three lists. Bound to a key, and dispatched by anything that
/// wants to offer a command without a keystroke of its own.
#[derive(Clone, PartialEq, Debug, Action)]
#[action(namespace = palette, no_json)]
pub struct Open {
    pub kind: Kind,
}

/// `palette::Run` is what a user keymap binds to: one action whose argument is the command's
/// name.
#[derive(Clone, PartialEq, Debug, Action)]
#[action(namespace = palette, no_json)]
pub struct Run {
    pub name: String,
}

/// Bind the app's own defaults, then whatever `[keymap]` overrides.
pub fn apply_keymap(cx: &mut App, settings: &BTreeMap<String, String>) -> Vec<String> {
    let mut unknown = Vec::new();
    // The app's own keys are bound by `app_keymap`, next to the actions they reach and as typed
    // actions; this is the user's layer on top of them, and it goes through `palette::Run`
    // because a `[keymap]` entry names an action as a string.
    for (key, name) in settings {
        if action_by_name(name).is_none() {
            unknown.push(name.clone());
            continue;
        }
        cx.bind_keys([KeyBinding::new(key, Run { name: name.clone() }, None)]);
    }
    unknown
}

// ---------------------------------------------------------------- the surface

/// Which list is behind the palette. The only thing that differs between the
/// two is the list and the words around it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Files,
    All,
}

impl Kind {
    fn placeholder(self) -> String {
        match self {
            Kind::Files => t!("palette.search_files"),
            Kind::All => t!("palette.search_all_hint"),
        }
        .to_string()
    }

    /// The word under the list when nothing matches.
    fn empty(self) -> String {
        match self {
            Kind::Files => t!("palette.no_file"),
            Kind::All => t!("palette.no_match_all"),
        }
        .to_string()
    }
}

/// The modal: a query field, the ranked list, and the keys that move through it.
pub struct Palette {
    kind: Kind,
    items: Vec<Item>,
    /// Indices into `items`, filtered and ranked; the top [`MAX_ROWS`] are drawn.
    shown: Vec<usize>,
    selected: usize,
    /// The list's own scroll position, so the keyboard can walk the selection
    /// past the last row on screen without losing sight of it.
    scroll: ScrollHandle,
    query: Entity<InputState>,
    _events: Subscription,
}

impl Palette {
    pub fn new(kind: Kind, items: Vec<Item>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let query = cx.new(|cx| InputState::new(window, cx).placeholder(kind.placeholder()));
        // The list is a function of what is typed, so the field's event is what asks for the redraw.
        let events = cx.subscribe(&query, |this, input, _event: &InputEvent, cx| {
            let text = input.read(cx).value().to_string();
            this.refilter(&text);
            cx.notify();
        });
        let mut palette = Self {
            kind,
            items,
            shown: Vec::new(),
            selected: 0,
            scroll: ScrollHandle::new(),
            query,
            _events: events,
        };
        palette.refilter("");
        palette
    }

    fn refilter(&mut self, query: &str) {
        let candidates: Vec<String> = self
            .items
            .iter()
            .map(|item| match &item.detail {
                // Rank on the path as well as the name: `quick/menu.rs` beats a
                // file called `menu.rs` in another directory for `quick menu`.
                Some(detail) => format!("{} {}", item.title, detail),
                None => item.title.to_string(),
            })
            .collect();
        self.shown = rank(query, &candidates);
        self.selected = 0;
    }

    fn move_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        let count = self.shown.len().min(MAX_ROWS);
        if count == 0 {
            return;
        }
        let next = self.selected as isize + delta;
        self.selected = next.clamp(0, count as isize - 1) as usize;
        // The selected row is the child at that index of the scrolling list
        // (see `render`), which is what `scroll_to_item` measures against.
        self.scroll.scroll_to_item(self.selected);
        cx.notify();
    }

    /// Run the selected row.
    fn run_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(index) = self.shown.get(self.selected).copied() else {
            return;
        };
        let run = self.items[index].run.clone();
        let handle = window.window_handle();
        cx.defer(move |cx| {
            let _ = handle.update(cx, |_, window, cx| {
                window.close_dialog(cx);
                run(window, cx);
            });
        });
    }

    /// The list's three keys. Actions in the `palette` context rather than a
    /// capture listener, because the dialog this sits in binds Enter for its own
    /// Confirm: a bound key is dispatched as an action and no listener sees it.
    fn handle_next(&mut self, _: &Next, _window: &mut Window, cx: &mut Context<Self>) {
        self.move_selection(1, cx);
    }

    fn handle_previous(&mut self, _: &Previous, _window: &mut Window, cx: &mut Context<Self>) {
        self.move_selection(-1, cx);
    }

    fn handle_confirm(&mut self, _: &Confirm, window: &mut Window, cx: &mut Context<Self>) {
        self.run_selected(window, cx);
    }

    fn row(&self, position: usize, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let colors = Theme::global(cx).tokens.colors.clone();
        let item = &self.items[index];
        let selected = position == self.selected;
        let mut row = div()
            .id(SharedString::from(format!("palette-item-{position}")))
            .test_support()
            .aria_label(item.title.clone())
            .aria_selected(selected)
            .h_flex()
            .items_center()
            .justify_between()
            .gap_3()
            .px_3()
            .py_2()
            .min_h(px(40.))
            .rounded_md()
            .cursor_pointer()
            .when(selected, |el| el.bg(colors.muted))
            .hover(|el| el.bg(colors.muted))
            .on_click(cx.listener(move |this, _event, window, cx| {
                // A click selects what it clicked and runs it: the row under the
                // pointer is the row the user meant, whatever the keyboard had
                // selected.
                this.selected = position;
                this.run_selected(window, cx);
            }));

        // Both lines shrink before the group badge, so long paths never cover it.
        let mut left = div()
            .v_flex()
            .gap_1()
            .flex_1()
            .min_w_0()
            .overflow_hidden()
            .child(
                div()
                    .text_sm()
                    .text_color(colors.foreground)
                    .w_full()
                    .min_w_0()
                    .truncate()
                    .child(item.title.clone()),
            );
        if let Some(detail) = item.detail.clone() {
            left = left.child(
                div()
                    .text_xs()
                    .text_color(colors.muted_foreground)
                    .w_full()
                    .min_w_0()
                    .truncate()
                    .child(detail),
            );
        }
        row = row.child(left);

        let mut right = div().h_flex().items_center().gap_2().flex_shrink_0();
        if !item.group.is_empty() {
            right = right.child(
                div()
                    .text_xs()
                    .text_color(colors.muted_foreground)
                    .px_2()
                    .py_0p5()
                    .rounded_sm()
                    .bg(colors.muted)
                    .child(item.group.clone()),
            );
        }
        row.child(right).into_any_element()
    }
}

impl Render for Palette {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The keyboard, every frame: a dialog takes focus for its own content region when it opens, so
        // a field focused at construction loses it before anything can be typed.
        let field = self.query.read(cx).focus_handle(cx);
        if window.focused(cx) != Some(field.clone()) {
            window.focus(&field, cx);
        }

        let colors = Theme::global(cx).tokens.colors.clone();
        let drawn = self.shown.len().min(MAX_ROWS);

        // The list takes the height the window can spare: it is the tallest part
        // of the dialog, and a fixed one would put the footer past the bottom of
        // a short window — the dialog clips its content rather than scrolling it.
        // What is left after the dialog's own chrome (its margins, the field, the
        // footer: ~180 px), between a floor that still shows a few rows and the
        // height a roomy window gives it.
        let list_height = (window.viewport_size().height - px(180.))
            .max(px(80.))
            .min(px(620.));

        // The rows are the direct children of the scrolling element: the scroll
        // handle measures the selection by child index, so a wrapper around them
        // would hide every row but the first.
        let mut list = div()
            .id("palette-rows")
            .test_support()
            .v_flex()
            .gap_1()
            .py_1()
            .pr_1()
            .w_full()
            .max_h(list_height)
            .overflow_y_scroll()
            .track_scroll(&self.scroll);
        for position in 0..drawn {
            let index = self.shown[position];
            list = list.child(self.row(position, index, cx));
        }
        if drawn == 0 {
            list = list.child(
                div()
                    .id("palette-empty")
                    .test_support()
                    .h_flex()
                    .items_center()
                    .justify_center()
                    .py_8()
                    .text_sm()
                    .text_color(colors.muted_foreground)
                    .child(self.kind.empty()),
            );
        }

        // How many of how many, said out loud but not drawn: the count is a
        // label for a screen reader and a test, and the footer itself is only
        // the keys.
        let mut counted = t!("palette.count", shown = drawn, total = self.shown.len()).to_string();
        if self.shown.len() > drawn {
            counted = t!(
                "palette.count_capped",
                shown = drawn,
                total = self.shown.len(),
                max = MAX_ROWS
            )
            .to_string();
        }
        let footer = div()
            .id("palette-footer")
            .test_support()
            .aria_label(SharedString::from(counted))
            .h_flex()
            .items_center()
            .justify_end()
            .gap_3()
            .w_full()
            .px_3()
            .pt_3()
            .pb_1()
            .border_t_1()
            .border_color(colors.border)
            .text_xs()
            .text_color(colors.muted_foreground)
            // The keys the list answers to, where a reader looks for them: the
            // palette is opened by a key and driven by three more.
            .child(div().flex_shrink_0().child(t!("palette.hint")));

        // The dialog has no scrim — the window behind is what the transparency
        // setting asked for — so the dimmed backdrop that used to catch a click
        // outside is gone. This canvas is the replacement: it measures the box
        // the palette occupies as it paints and closes the dialog on a click
        // outside it, leaving the no-scrim look.
        let click_away = canvas(
            |_bounds, _window, _cx| (),
            |bounds, (), window, _cx| {
                window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx| {
                    if phase != DispatchPhase::Capture
                        || event.button != MouseButton::Left
                        || bounds.contains(&event.position)
                    {
                        return;
                    }
                    cx.stop_propagation();
                    window.close_dialog(cx);
                });
            },
        )
        .absolute()
        .inset_0();

        div()
            .id("palette")
            .test_support()
            .v_flex()
            .gap_3()
            .w_full()
            .child(click_away)
            // The keys are bound in this context (see `app_keymap`), because the
            // dialog this sits in binds Enter for its own Confirm and a bound key
            // is dispatched as an action: a listener never sees it.
            .key_context("palette")
            .on_action(cx.listener(Self::handle_next))
            .on_action(cx.listener(Self::handle_previous))
            .on_action(cx.listener(Self::handle_confirm))
            .child(
                Input::new(&self.query)
                    .focus_bordered(false)
                    .id("palette-input")
                    .text_sm()
                    .w_full(),
            )
            // A long file list is the normal case, so the list scrolls and the
            // field and the footer do not.
            .child(list)
            .child(footer)
    }
}

#[cfg(test)]
mod tests {
    // Not `use super::*`: the module above globs gpui in, and under `cfg(test)`
    // that glob brings gpui's own `test` attribute — which then shadows the
    // built-in one and every `#[test]` below expands into itself.
    use super::{action_by_name, actions, rank, score};

    /// A list with one of everything the palette shows: a path, a command, and
    /// two entries that only share letters with the query.
    fn candidates() -> Vec<String> {
        vec![
            "src/main.rs".to_string(),
            "crates/app/src/pane/terminal.rs".to_string(),
            "README.md".to_string(),
            "Terminal: New".to_string(),
            "Diff: Open".to_string(),
        ]
    }

    /// The three tiers the palette promises, on candidates that differ in
    /// nothing else: the same directory, the same match position, the same
    /// characters — only the file name's relationship to the query changes.
    #[test]
    fn a_prefix_beats_a_substring_beats_no_file_name_at_all() {
        let prefix = score("term", "a/b/term.rs").unwrap();
        let substring = score("term", "a/b/xxxterm.rs").unwrap();
        let neither = score("term", "a/b/xxx-t-e-r-m.rs").unwrap();

        assert!(
            prefix < substring,
            "prefix {prefix} vs substring {substring}"
        );
        assert!(
            substring < neither,
            "substring {substring} vs neither {neither}"
        );

        let candidates: Vec<String> = ["a/b/xxx-t-e-r-m.rs", "a/b/xxxterm.rs", "a/b/term.rs"]
            .iter()
            .map(|path| path.to_string())
            .collect();
        assert_eq!(rank("term", &candidates), vec![2, 1, 0]);
    }

    /// The path is searched too, which is what makes `quick/` reach a file
    /// whose name says nothing about it. A directory match is not a file name
    /// match, so it sits below all three tiers above.
    #[test]
    fn a_directory_match_beats_no_match_at_all() {
        let in_a_directory = score("quick", "src/quick/menu.rs").unwrap();
        assert!(
            in_a_directory <= 0,
            "a path match should score at least as well as nothing: {in_a_directory}"
        );
        assert_eq!(score("quick", "src/slow/menu.rs"), None);
    }

    #[test]
    fn a_word_start_is_worth_more_than_a_scattered_match() {
        // Same characters, same number of them; the one that starts a word is
        // the one the reader meant.
        let word_start = score("cn", "crates/new.rs").unwrap();
        let scattered = score("cn", "crates/other/new.rs").unwrap();
        assert!(word_start < scattered, "{word_start} vs {scattered}");
    }

    #[test]
    fn a_missing_character_is_no_match() {
        assert_eq!(score("zzz", "src/main.rs"), None);
        assert_eq!(rank("zzz", &candidates()), Vec::<usize>::new());
        // Case does not matter.
        assert_eq!(score("MAIN", "src/main.rs"), score("main", "src/main.rs"));
    }

    #[test]
    fn an_empty_query_matches_everything_in_input_order() {
        let candidates = candidates();
        assert_eq!(
            rank("", &candidates),
            (0..candidates.len()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn ties_keep_the_input_order() {
        let candidates = vec![
            "a/x.rs".to_string(),
            "b/x.rs".to_string(),
            "c/x.rs".to_string(),
        ];
        assert_eq!(rank("x.rs", &candidates), vec![0, 1, 2]);
    }

    #[test]
    fn the_table_names_every_action_it_lists() {
        let mut names: Vec<&str> = actions().iter().map(|spec| spec.name).collect();
        let count = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), count, "two commands share a name");
        for spec in actions() {
            assert!(
                spec.name.contains("::"),
                "{} is not the namespace::Name spelling",
                spec.name
            );
            assert!(
                action_by_name(spec.name).is_some(),
                "{} is listed and not findable by name",
                spec.name
            );
        }
        assert!(action_by_name("nope::Nope").is_none());
    }
}
