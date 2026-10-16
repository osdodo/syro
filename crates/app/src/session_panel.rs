//! The session history panel: the past sessions the agents left behind, and the way back into
//! one.

use std::collections::{BTreeMap, BTreeSet};

use gpui_kit::base::{ColorTokens, ObservedElement, StyledExt as _, TestSupportExt as _, Theme};
use gpui_kit::component::button::ButtonVariant;
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{Icon, IconName, WindowExt as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rust_i18n::t;
use syro_core::session::{self, Cache, DEFAULT_LIMIT, Session, SessionRoots};

use syro_core::event::CoreEvent;

use crate::state::{AppState, terminal};

/// The header's height, which is the tab bar's and the activity strip's: the
/// three are stacked, and a control that jumps between them is a control nobody
/// finds twice.
const HEADER_HEIGHT: f32 = 36.;

/// Re-enter a past session: the row's click.
#[derive(Clone, PartialEq, Debug, Action)]
#[action(namespace = session, no_json)]
pub struct Resume {
    pub session: Session,
}

/// One working directory's sessions, in the order they are drawn.
struct Group {
    /// The directory itself: two projects can share a last path component, and
    /// the group is what tells them apart.
    cwd: String,
    name: String,
    rows: Vec<usize>,
}

pub struct SessionPanel {
    state: Entity<AppState>,
    /// Where the transcripts are read from. A view's own field rather than a
    /// global so a test can point it at a fixture home.
    roots: SessionRoots,
    /// Everything the last scan found, before the repository filter and the search. The
    /// filtering happens when the list is drawn, so typing does not re-scan.
    sessions: Vec<Session>,
    /// How many transcripts there were to scan, before the cap.
    total: usize,
    /// Whether a scan has ever finished. Until then the panel says it is
    /// scanning rather than that there is nothing.
    scanned: bool,
    /// Carried between scans: what each transcript has already told us.
    cache: Cache,
    query: String,
    input: Option<Entity<InputState>>,
    _input_events: Option<Subscription>,
    collapsed: BTreeSet<String>,
    /// Only the current row shows its details and buttons.
    expanded: Option<String>,
    /// Whether the panel is picking sessions for a batch rather than reading
    /// them: a click on a row takes it, and the header offers the delete.
    picking: bool,
    /// The sessions a click has taken. Ids, not indexes: a scan that reorders
    /// the list must not move a choice onto another session.
    chosen: BTreeSet<String>,
    /// The running scan. Held so that a second one replaces the first.
    _scan: Option<Task<()>>,
    refresh_generation: u64,
    /// What makes the panel redraw when the state moves under it: the list is the repository
    /// on screen, and a worktree that arrives or is removed changes which sessions
    /// belong to it.
    _events: Subscription,
}

impl SessionPanel {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let events = cx.subscribe(&state, |this, _state, event: &CoreEvent, cx| match event {
            CoreEvent::ActiveWorktreeChanged { .. } => {
                this.expanded = None;
                cx.notify();
            }
            CoreEvent::WorktreeCreated { .. } | CoreEvent::WorktreeRemoved { .. } => cx.notify(),
            _ => {}
        });
        Self {
            state,
            roots: SessionRoots::default(),
            sessions: Vec::new(),
            total: 0,
            scanned: false,
            cache: Cache::default(),
            query: String::new(),
            input: None,
            _input_events: None,
            collapsed: BTreeSet::new(),
            expanded: None,
            picking: false,
            chosen: BTreeSet::new(),
            _scan: None,
            refresh_generation: 0,
            _events: events,
        }
    }

    /// Point the panel at another home. Test-only: the app reads `$HOME`.
    #[cfg(test)]
    pub fn set_roots(&mut self, roots: SessionRoots) {
        self.roots = roots;
    }

    /// What the last scan found, for a test that asserts on the scan rather than
    /// on the drawing.
    #[cfg(test)]
    pub fn sessions(&self) -> &[Session] {
        &self.sessions
    }

    /// How many rows a batch delete would take, for a test that asserts on the
    /// choice rather than on the drawing.
    #[cfg(test)]
    pub fn chosen(&self) -> usize {
        self.chosen.len()
    }

    /// Take or drop one row's checkmark.
    fn set_chosen(&mut self, id: &str, on: bool) {
        if on {
            self.chosen.insert(id.to_string());
        } else {
            self.chosen.remove(id);
        }
    }

    /// Read the transcripts again, off the UI thread.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        if self._scan.is_some() {
            // One scan at a time: a refresh while one is running would have two
            // readers folding into two caches and the slower one winning.
            return;
        }
        let roots = self.roots.clone();
        let mut cache = std::mem::take(&mut self.cache);
        let task = cx.spawn(async move |this, cx| {
            let (scan, cache) = cx
                .background_executor()
                .spawn(async move {
                    let scan = session::scan(&roots, &mut cache, DEFAULT_LIMIT);
                    (scan, cache)
                })
                .await;
            this.update(cx, |panel, cx| {
                panel.cache = cache;
                panel.sessions = scan.sessions;
                panel.total = scan.total;
                panel.scanned = true;
                panel._scan = None;
                cx.notify();
            })
            .ok();
        });
        self._scan = Some(task);
        cx.notify();
    }

    /// The groups to draw: what the repository on screen and the query keep, grouped by working
    /// directory, newest group first.
    fn groups(&self, cx: &App) -> Vec<Group> {
        let state = self.state.read(cx);
        let active = state.active();
        let worktrees = state.worktrees();
        let needle = self.query.trim().to_lowercase();

        let mut groups: Vec<Group> = Vec::new();
        let mut index: BTreeMap<String, usize> = BTreeMap::new();
        for (at, session) in self.sessions.iter().enumerate() {
            if !session::in_active_repo(session, active, worktrees) {
                continue;
            }
            if !needle.is_empty() && !matches(session, &needle) {
                continue;
            }
            let cwd = session.cwd.display().to_string();
            let group = *index.entry(cwd.clone()).or_insert_with(|| {
                groups.push(Group {
                    cwd,
                    name: session.workspace.clone(),
                    rows: Vec::new(),
                });
                groups.len() - 1
            });
            groups[group].rows.push(at);
        }
        // The scan is newest first, so a group's first row is its newest:
        // groups sort by it and so does everything inside them.
        groups.sort_by(|left, right| {
            self.sessions[right.rows[0]]
                .updated_ms
                .cmp(&self.sessions[left.rows[0]].updated_ms)
        });
        groups
    }

    /// Build the search field on the first frame with a window, and listen to
    /// it: the text lives here, the field is only where it is typed.
    fn ensure_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.input.is_some() {
            return;
        }
        let input = cx.new(|cx| InputState::new(window, cx).placeholder(t!("sessions.search")));
        let events = cx.subscribe(&input, |this, input, _event: &InputEvent, cx| {
            let query = input.read(cx).value().to_string();
            if this.query != query {
                this.query = query;
                cx.notify();
            }
        });
        self.input = Some(input);
        self._input_events = Some(events);
    }

    /// One session: a header that opens onto its details and its three ways
    /// forward — the row's own click, and the words inside it once it is open.
    fn session_row(
        &self,
        index: usize,
        colors: &ColorTokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let session = &self.sessions[index];
        // Picking is not reading: an opened row would only push the checkmarks
        // apart, and its buttons would be four ways to hit the wrong one.
        let expanded = self.expanded.as_ref() == Some(&session.id) && !self.picking;
        let mut meta = vec![
            session.agent.display().to_string(),
            t!("sessions.messages", count = session.messages).to_string(),
            session::relative_time(session::now_ms(), session.updated_ms),
        ];
        if let Some(model) = &session.model {
            meta.push(model.clone());
        }
        let toggled = session.id.clone();
        let head = div()
            .id(SharedString::from(format!("session-{}", session.id)))
            .test_support()
            .aria_label(SharedString::from(session.title.clone()))
            .v_flex()
            .w_full()
            .gap_1()
            .px_2()
            .py_1p5()
            .cursor_pointer()
            .hover(|el| el.bg(colors.muted))
            .child(
                div()
                    .w_full()
                    .overflow_hidden()
                    .text_ellipsis()
                    .text_sm()
                    .font_weight(FontWeight::MEDIUM)
                    .child(SharedString::from(session.title.clone())),
            )
            .when(!session.preview.is_empty(), |el| {
                el.child(
                    div()
                        .w_full()
                        .overflow_hidden()
                        .text_ellipsis()
                        .text_xs()
                        .text_color(colors.muted_foreground)
                        .child(SharedString::from(session.preview.clone())),
                )
            })
            .child(
                div()
                    .w_full()
                    .overflow_hidden()
                    .text_ellipsis()
                    .text_xs()
                    .text_color(colors.muted_foreground)
                    .child(SharedString::from(meta.join(" · "))),
            )
            .on_click(cx.listener(move |this, _event, _window, cx| {
                if this.picking {
                    let on = !this.chosen.contains(&toggled);
                    this.set_chosen(&toggled, on);
                } else {
                    this.expanded =
                        (this.expanded.as_ref() != Some(&toggled)).then(|| toggled.clone());
                }
                cx.notify();
            }));

        // The checkmark sits beside the row rather than inside it, so a click on
        // the box is the box's and does not also reach the row.
        let picked = self.picking.then(|| {
            let id = session.id.clone();
            Checkbox::new(SharedString::from(format!("session-{id}-choose")))
                .checked(self.chosen.contains(&session.id))
                .accessibility_label(SharedString::from(
                    t!("sessions.select_session", title = session.title).to_string(),
                ))
                .on_change(cx.listener(move |this, checked: &bool, _window, cx| {
                    this.set_chosen(&id, *checked);
                    cx.notify();
                }))
        });

        let mut row = div()
            .id(SharedString::from(format!("session-row-{}", session.id)))
            .w_full();
        row = match picked {
            Some(check) => row
                .h_flex()
                .items_center()
                .pl_2()
                .gap_1()
                .child(check)
                .child(div().flex_1().min_w_0().child(head)),
            None => row.child(head),
        };
        if expanded {
            row = row.child(self.expanded_block(session, colors, cx));
        }
        row.into_any_element()
    }

    /// What an opened row adds: where the agent worked, which transcript it is,
    /// and the three things to do with it.
    fn expanded_block(
        &self,
        session: &Session,
        colors: &ColorTokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = session.id.clone();
        let resume = Resume {
            session: session.clone(),
        };
        let for_new = session.clone();
        let for_delete = session.clone();
        div()
            .id(SharedString::from(format!("session-details-{id}")))
            .v_flex()
            .w_full()
            .gap_1()
            .px_2()
            .pb_1p5()
            .child(detail(
                format!("session-{id}-cwd"),
                session.cwd.display().to_string(),
                colors,
            ))
            .child(detail(
                format!("session-{id}-id"),
                session.id.clone(),
                colors,
            ))
            .child(
                div()
                    .h_flex()
                    .flex_wrap()
                    .items_center()
                    .gap_1()
                    .w_full()
                    .pt_1()
                    .child(
                        action_button(
                            format!("session-{id}-resume"),
                            t!("sessions.continue").to_string(),
                            colors,
                        )
                        .on_click(cx.listener(
                            move |_this, _event, window, cx| {
                                window.dispatch_action(Box::new(resume.clone()), cx);
                            },
                        )),
                    )
                    .child(
                        action_button(
                            format!("session-{id}-new"),
                            t!("sessions.new").to_string(),
                            colors,
                        )
                        .on_click(cx.listener(
                            move |this, _event, window, cx| {
                                let agent = for_new.agent.id().to_string();
                                let target = {
                                    let state = this.state.read(cx);
                                    session::resume_target(&for_new, state.worktrees())
                                        .map(|worktree| worktree.id.clone())
                                };
                                let Some(worktree_id) = target else {
                                    return;
                                };
                                // A compact handoff from the transcript, so the
                                // fresh context window starts on the work already done.
                                let seed = session::handoff(for_new.agent, &for_new.path);
                                let model = for_new
                                    .provider
                                    .as_ref()
                                    .zip(for_new.model.as_ref())
                                    .map(|(provider, model)| format!("{provider}/{model}"));
                                window.dispatch_action(
                                    Box::new(terminal::New {
                                        worktree_id,
                                        agent: Some(agent),
                                        resume: None,
                                        seed,
                                        model,
                                    }),
                                    cx,
                                );
                            },
                        )),
                    )
                    .child(
                        action_button(
                            format!("session-{id}-delete"),
                            t!("sessions.delete").to_string(),
                            colors,
                        )
                        .on_click(cx.listener(
                            move |_this, _event, window, cx| {
                                let session = for_delete.clone();
                                let path = session.path.clone();
                                let title = session.title.clone();
                                let panel = cx.weak_entity();
                                window.open_alert_dialog(cx, move |alert, _, _| {
                                    let path = path.clone();
                                    let panel = panel.clone();
                                    alert
                                        .title(t!("sessions.delete_title").to_string())
                                        .description(
                                            t!("sessions.delete_question", title = title)
                                                .to_string(),
                                        )
                                        .confirm()
                                        .ok_text(t!("sessions.delete"))
                                        .ok_variant(ButtonVariant::Danger)
                                        .on_ok(move |_, _, cx| {
                                            let _ = std::fs::remove_file(&path);
                                            let _ = panel.update(cx, |panel, cx| panel.refresh(cx));
                                            true
                                        })
                                });
                            },
                        )),
                    ),
            )
            .into_any_element()
    }

    /// A working directory and the sessions under it. Collapsible, because a
    /// project with fifty sessions would otherwise be fifty rows of scrolling
    /// before the next one.
    fn group_block(
        &self,
        group: &Group,
        colors: &ColorTokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let collapsed = self.collapsed.contains(&group.cwd);
        let cwd = group.cwd.clone();
        // The group's own checkmark, which is how a project with fifty rows is
        // cleared without fifty clicks.
        let picked = self.picking.then(|| {
            let ids: Vec<String> = group
                .rows
                .iter()
                .map(|at| self.sessions[*at].id.clone())
                .collect();
            let all = ids.iter().all(|id| self.chosen.contains(id));
            Checkbox::new(SharedString::from(format!(
                "session-group-{}-choose",
                group.cwd
            )))
            .checked(all)
            .accessibility_label(SharedString::from(
                t!("sessions.select_group", name = group.name).to_string(),
            ))
            .on_change(cx.listener(move |this, checked: &bool, _window, cx| {
                for id in &ids {
                    this.set_chosen(id, *checked);
                }
                cx.notify();
            }))
        });
        let head = div()
            .id(SharedString::from(format!(
                "session-group-head-{}",
                group.cwd
            )))
            .test_support()
            .aria_label(SharedString::from(
                t!("sessions.group", name = group.name).to_string(),
            ))
            .h_flex()
            .items_center()
            .gap_1()
            .w_full()
            .px_2()
            .py_1()
            .cursor_pointer()
            .hover(|el| el.bg(colors.muted))
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
                    .flex_1()
                    .overflow_hidden()
                    .text_ellipsis()
                    .text_xs()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(SharedString::from(group.name.clone())),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(colors.muted_foreground)
                    .child(SharedString::from(group.rows.len().to_string())),
            )
            .on_click(cx.listener(move |this, _event, _window, cx| {
                if !this.collapsed.remove(&cwd) {
                    this.collapsed.insert(cwd.clone());
                }
                cx.notify();
            }));
        let mut block = div()
            .id(SharedString::from(format!("session-group-{}", group.cwd)))
            .test_support()
            .v_flex()
            .w_full();
        block = match picked {
            Some(check) => block.child(
                div()
                    .h_flex()
                    .items_center()
                    .w_full()
                    .pl_2()
                    .gap_1()
                    .child(check)
                    .child(div().flex_1().min_w_0().child(head)),
            ),
            None => block.child(head),
        };
        if collapsed {
            return block.into_any_element();
        }
        for index in &group.rows {
            block = block.child(self.session_row(*index, colors, cx));
        }
        block.into_any_element()
    }
}

impl Render for SessionPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.ensure_input(window, cx);
        // A panel is only rendered while it is the sidebar's selected one, so
        // this frame is the first look: that is when the transcripts are read,
        // and the reason the scan costs nothing while the panel is away.
        if !self.scanned && self._scan.is_none() {
            self.refresh(cx);
        }
        let colors = Theme::global(cx).tokens.colors;
        let groups = self.groups(cx);
        let shown: usize = groups.iter().map(|group| group.rows.len()).sum();

        // The batch's two words, drawn only while picking: the checkmarks are a
        // mode, and a delete that acts on a choice nobody can see is a trap.
        let delete_chosen = action_button(
            "sessions-delete-chosen".to_string(),
            t!("sessions.delete_chosen", count = self.chosen.len()).to_string(),
            &colors,
        )
        .on_click(cx.listener(move |this, _event, window, cx| {
            let paths: Vec<std::path::PathBuf> = this
                .sessions
                .iter()
                .filter(|session| this.chosen.contains(&session.id))
                .map(|session| session.path.clone())
                .collect();
            if paths.is_empty() {
                return;
            }
            let count = paths.len();
            let panel = cx.weak_entity();
            window.open_alert_dialog(cx, move |alert, _, _| {
                let paths = paths.clone();
                let panel = panel.clone();
                alert
                    .title(t!("sessions.delete_title_many").to_string())
                    .description(t!("sessions.delete_question_many", count = count).to_string())
                    .confirm()
                    .ok_text(t!("sessions.delete"))
                    .ok_variant(ButtonVariant::Danger)
                    .on_ok(move |_, _, cx| {
                        for path in &paths {
                            let _ = std::fs::remove_file(path);
                        }
                        let _ = panel.update(cx, |panel, cx| {
                            panel.chosen.clear();
                            panel.picking = false;
                            panel.refresh(cx);
                        });
                        true
                    })
            });
        }));
        let pick = action_button(
            "sessions-pick".to_string(),
            if self.picking {
                t!("common.done").to_string()
            } else {
                t!("sessions.select").to_string()
            },
            &colors,
        )
        .on_click(cx.listener(|this, _event, _window, cx| {
            this.picking = !this.picking;
            // Leaving the mode drops the batch: a choice carried out of sight is
            // the one that deletes the wrong thing a week later.
            this.chosen.clear();
            cx.notify();
        }));

        let header = div()
            .id("sessions-header")
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
                    .child(t!("sessions.title")),
            )
            .child(
                div()
                    .h_flex()
                    .items_center()
                    .gap_1()
                    .when(self.picking, |el| el.child(delete_chosen))
                    .child(pick)
                    .child(
                        div()
                            .id("sessions-refresh")
                            .test_support()
                            .aria_label(t!("sessions.refresh"))
                            .h_flex()
                            .items_center()
                            .justify_center()
                            .size(px(22.))
                            .rounded_sm()
                            .cursor_pointer()
                            .text_color(colors.muted_foreground)
                            .hover(|el| el.bg(colors.muted))
                            .child(crate::right_sidebar::refresh_icon(
                                self.refresh_generation,
                                self._scan.is_some(),
                            ))
                            .on_click(cx.listener(|this, _event, _window, cx| {
                                if this._scan.is_none() {
                                    this.refresh_generation += 1;
                                    this.refresh(cx);
                                }
                            })),
                    ),
            );

        // The count line, which is what says the list is capped: "26 shown · newest 250".
        let summary = match self.scanned {
            false => t!("sessions.scanning").to_string(),
            true if self.total > self.sessions.len() => t!(
                "sessions.shown_newest",
                shown = shown,
                newest = self.sessions.len()
            )
            .to_string(),
            true => t!("sessions.found", count = shown).to_string(),
        };

        let mut body = div()
            .id("sessions-body")
            .test_support()
            .v_flex()
            .flex_1()
            .min_h_0()
            .w_full()
            .pb_2()
            .overflow_y_scroll();
        if groups.is_empty() {
            let empty = if !self.scanned {
                String::new()
            } else if self.sessions.is_empty() {
                t!("sessions.empty").to_string()
            } else {
                t!("sessions.no_match").to_string()
            };
            body = body.child(
                div()
                    .px_2()
                    .py_1()
                    .text_xs()
                    .text_color(colors.muted_foreground)
                    .child(SharedString::from(empty.to_string())),
            );
        } else {
            for group in &groups {
                body = body.child(self.group_block(group, &colors, cx));
            }
        }

        // The sidebar owns the width, the border and the resize handle; this is the panel's content
        // (`crate::right_sidebar`).
        div()
            .id("session-panel")
            .test_support()
            .when(self.expanded.is_some() && !self.picking, |el| {
                el.on_mouse_down_out(cx.listener(|this, _event, _window, cx| {
                    this.expanded = None;
                    cx.notify();
                }))
            })
            .v_flex()
            .w_full()
            .flex_1()
            .min_h_0()
            .child(header)
            .child(
                div()
                    .px_2()
                    .pb_1()
                    .text_xs()
                    .text_color(colors.muted_foreground)
                    .child(SharedString::from(summary)),
            )
            .when_some(self.input.clone(), |el, input| {
                el.child(
                    div()
                        .px_2()
                        .pb_1()
                        .child(Input::new(&input).focus_bordered(false)),
                )
            })
            .child(body)
    }
}

/// Whether a session answers the query: the title, the preview, the working directory, the
/// model and the id — what the row shows, and what someone would type to find it again.
fn matches(session: &Session, needle: &str) -> bool {
    let haystacks = [
        session.title.to_lowercase(),
        session.preview.to_lowercase(),
        session.cwd.display().to_string().to_lowercase(),
        session.model.clone().unwrap_or_default().to_lowercase(),
        session.id.to_lowercase(),
    ];
    haystacks.iter().any(|text| text.contains(needle))
}

/// One line of an opened row's details: quiet, small, and never wider than the
/// panel.
fn detail(id: String, text: String, colors: &ColorTokens) -> AnyElement {
    div()
        .id(SharedString::from(id))
        .test_support()
        .w_full()
        .overflow_hidden()
        .text_ellipsis()
        .text_xs()
        .text_color(colors.muted_foreground)
        .child(SharedString::from(text))
        .into_any_element()
}

/// One of the words an opened row offers, with the caller attaching what it
/// does.
fn action_button(
    id: String,
    label: String,
    colors: &ColorTokens,
) -> ObservedElement<Stateful<Div>> {
    div()
        .id(SharedString::from(id))
        .test_support()
        .aria_label(SharedString::from(label.clone()))
        .px_2()
        .py_1()
        .rounded_sm()
        .border_1()
        .border_color(colors.border)
        .text_xs()
        .text_color(colors.foreground)
        .cursor_pointer()
        .hover(|el| el.bg(colors.muted))
        .child(SharedString::from(label))
}

#[cfg(test)]
mod tests {
    use super::matches;
    use std::path::PathBuf;
    use syro_core::session::{Session, SessionAgent};

    fn session() -> Session {
        Session {
            agent: SessionAgent::Pi,
            id: "01a09f17".into(),
            path: PathBuf::from("/home/ada/.pi/agent/sessions/--x--/a.jsonl"),
            cwd: PathBuf::from("/home/ada/code/Swap-Aggregator"),
            workspace: "Swap-Aggregator".into(),
            title: "Fix the flaky test".into(),
            preview: "On it.".into(),
            model: Some("claude-opus-5".into()),
            provider: Some("anthropic".into()),
            messages: 2,
            updated_ms: 0,
        }
    }

    /// The query reaches every field the row shows, in either case.
    #[test]
    fn a_query_matches_what_the_row_shows() {
        let session = session();
        for needle in ["flaky", "on it", "swap-aggregator", "opus", "01a09f17"] {
            assert!(matches(&session, needle), "{needle} did not match");
        }
        assert!(!matches(&session, "codex"));
        assert!(!matches(&session, "fix the flake"));
    }
}
