//! The two modal forms: a new worktree, and a worktree's removal.

use std::path::PathBuf;

use gpui_kit::base::{ColorTokens, StyledExt as _, TestSupportExt as _, Theme};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::kbd::Kbd;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{Disableable as _, Icon, IconName, Sizable as _, WindowExt as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rust_i18n::t;
use syro_core::event::CoreEvent;
use syro_core::git;
use syro_core::home_dir;
use syro_core::model::{AgentId, AgentSpec, RepoId, WorktreeId};

use crate::Workspace;
use crate::state::{AppState, terminal, worktree, worktree_name_problem};

/// Close the dialog on top of `window`.
fn close(window: AnyWindowHandle, cx: &mut App) {
    cx.defer(move |cx| {
        let _ = window.update(cx, |_, window, cx| window.close_dialog(cx));
    });
}

/// Which way the Add-project form is adding one, or `None` before the user has picked a way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AddMode {
    Clone,
    New,
}

/// The Add-project form: the three ways this app can
/// add one.
pub struct AddProject {
    state: Entity<AppState>,
    /// See [`NewWorktree::window`].
    window: AnyWindowHandle,
    mode: Option<AddMode>,
    /// The URL to clone, or the name of the project to start.
    value: Entity<InputState>,
    /// The parent directory, changed only through the native folder picker.
    destination: PathBuf,
    /// Except for the clone/create call, which is a child process.
    busy: bool,
    error: Option<String>,
}

impl AddProject {
    pub fn new(state: Entity<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let value =
            cx.new(|cx| InputState::new(window, cx).placeholder(t!("add_project.url_or_name")));
        Self {
            state,
            window: window.window_handle(),
            mode: None,
            value,
            destination: home_dir(),
            busy: false,
            error: None,
        }
    }

    /// Open one of the two doors that need a field. The third — browsing — is a
    /// native picker, and closing this dialog to open it is what the platform does too.
    fn open(&mut self, mode: AddMode, window: &mut Window, cx: &mut Context<Self>) {
        self.mode = Some(mode);
        self.error = None;
        let placeholder = match mode {
            AddMode::Clone => "https://github.com/owner/repo.git",
            AddMode::New => "my-project",
        };
        self.value.update(cx, |input, cx| {
            input.set_placeholder(placeholder, window, cx)
        });
        window.focus(&self.value.read(cx).focus_handle(cx), cx);
        cx.notify();
    }

    /// Browse: the platform's own folder picker, which is the only thing on this
    /// machine that can point at a directory. The dialog closes first — the
    /// picker is a sheet on the window, and two modals at once is one too many.
    fn browse(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        close(self.window, cx);
        window.dispatch_action(Box::new(crate::state::repo::Add { path: None }), cx);
    }

    fn select_destination(&mut self, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some(t!("add_project.choose_directory").to_string().into()),
        });
        cx.spawn(async move |this, cx| {
            let result = picked
                .await
                .map_err(anyhow::Error::from)
                .and_then(|result| result);
            this.update(cx, |form, cx| {
                match result {
                    Ok(Some(paths)) => {
                        if let Some(path) = paths.into_iter().next() {
                            form.destination = path;
                            form.error = None;
                        }
                    }
                    Ok(None) => {}
                    Err(error) => form.error = Some(format!("{error:#}")),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Clone or create, on the background executor: a clone is a network round
    /// trip and the window is not waiting for it.
    fn confirm(&mut self, cx: &mut Context<Self>) {
        let Some(mode) = self.mode else {
            return;
        };
        if self.busy {
            return;
        }
        let value = self.value.read(cx).value().trim().to_string();
        if value.is_empty() {
            self.error = Some(match mode {
                AddMode::Clone => t!("add_project.needs_url").to_string(),
                AddMode::New => t!("add_project.needs_name").to_string(),
            });
            cx.notify();
            return;
        }
        let destination = match mode {
            // The URL names the project: the last path segment, without `.git`.
            AddMode::Clone => self.destination.join(project_name_from_url(&value)),
            AddMode::New => self.destination.join(value.clone()),
        };
        // Both doors into one shape: run a child process and hand back the
        // directory it made. The clone's own progress goes nowhere, so what the
        // command "returns" is only ever the path the sidebar is told about.
        let command: Box<dyn FnOnce() -> anyhow::Result<PathBuf> + Send> = match mode {
            AddMode::Clone => {
                let url = value.clone();
                let lands = destination.clone();
                Box::new(move || {
                    git::clone(&url, &lands)?;
                    Ok(lands)
                })
            }
            AddMode::New => {
                let lands = destination.clone();
                Box::new(move || {
                    git::init(&lands)?;
                    Ok(lands)
                })
            }
        };
        self.busy = true;
        self.error = None;
        cx.notify();

        let state = self.state.clone();
        let window = self.window;
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { command() })
                .await;
            match result {
                Ok(added) => {
                    this.update(cx, |form, cx| {
                        form.busy = false;
                        close(form.window, cx);
                    })
                    .ok();
                    // The added repository is what closes this, and it is the
                    // same road the browse door takes: one place where a
                    // repository arrives, whichever way it was made.
                    let _ = window.update(cx, |_, window, cx| {
                        window.dispatch_action(
                            Box::new(crate::state::repo::Add {
                                path: Some(added.clone()),
                            }),
                            cx,
                        );
                    });
                }
                Err(error) => {
                    this.update(cx, |form, cx| {
                        form.busy = false;
                        form.error = Some(format!("{error:#}"));
                        cx.notify();
                    })
                    .ok();
                }
            }
            let _ = state;
        })
        .detach();
    }
}

/// The directory a clone lands in: the last segment of the URL, without `.git`.
/// `git@github.com:owner/repo.git`, `https://host/owner/repo` and `repo` all name
/// the same project.
fn project_name_from_url(url: &str) -> String {
    let trimmed = url.trim().trim_end_matches('/');
    let last = trimmed
        .rsplit(['/', ':'])
        .next()
        .unwrap_or(trimmed)
        .trim_end_matches(".git");
    if last.is_empty() {
        trimmed.to_string()
    } else {
        last.to_string()
    }
}

impl Render for AddProject {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = Theme::global(cx).tokens.colors;
        let mut form = div()
            .id("add-project")
            .test_support()
            .v_flex()
            .w_full()
            .gap_3()
            .text_sm()
            .child(
                div()
                    .h_flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .text_xs()
                            .text_color(colors.muted_foreground)
                            .child(t!("add_project.host")),
                    )
                    .child(
                        div()
                            .id("add-project-host")
                            .test_support()
                            .aria_label(t!("add_project.local_mac"))
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .border_1()
                            .border_color(colors.border)
                            .text_xs()
                            .child(t!("add_project.local_mac")),
                    ),
            )
            .child(add_card(
                "add-project-browse",
                IconName::Folder,
                &t!("add_project.browse"),
                &t!("add_project.browse_hint"),
                &colors,
                cx.listener(|this, _event, window, cx| this.browse(window, cx)),
            ))
            .child(
                div()
                    .text_xs()
                    .text_color(colors.muted_foreground)
                    .child(t!("add_project.other_ways")),
            )
            .child(
                div()
                    .id("add-project-others")
                    .test_support()
                    .v_flex()
                    .w_full()
                    .rounded_md()
                    .border_1()
                    .border_color(colors.border)
                    .overflow_hidden()
                    .child(add_row(
                        "add-project-clone",
                        IconName::Globe,
                        &t!("add_project.clone"),
                        &t!("add_project.clone_hint"),
                        &colors,
                        self.mode == Some(AddMode::Clone),
                        cx.listener(|this, _event, window, cx| {
                            this.open(AddMode::Clone, window, cx)
                        }),
                    ))
                    .child(div().h(px(1.)).w_full().bg(colors.border))
                    .child(add_row(
                        "add-project-new",
                        IconName::Plus,
                        &t!("add_project.create"),
                        &t!("add_project.create_hint"),
                        &colors,
                        self.mode == Some(AddMode::New),
                        cx.listener(|this, _event, window, cx| this.open(AddMode::New, window, cx)),
                    )),
            );

        // The door that was opened, and its field. Only one at a time: the
        // dialog is one decision wide.
        if let Some(mode) = self.mode {
            let fields = div()
                .id("add-project-fields")
                .test_support()
                .v_flex()
                .w_full()
                .gap_2()
                .child(
                    div()
                        .text_xs()
                        .text_color(colors.muted_foreground)
                        .child(match mode {
                            AddMode::Clone => t!("add_project.repo_url"),
                            AddMode::New => t!("add_project.project_name"),
                        }),
                )
                .child(
                    div()
                        .id("add-project-value")
                        .test_support()
                        .w_full()
                        .child(Input::new(&self.value).focus_bordered(false)),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(colors.muted_foreground)
                        .child(t!("add_project.in")),
                )
                .child(
                    Button::new("add-project-destination")
                        .outline()
                        .w_full()
                        .icon(IconName::Folder)
                        .label(self.destination.display().to_string())
                        .tooltip(self.destination.display().to_string())
                        .disabled(self.busy)
                        .on_click(cx.listener(|this, _, _, cx| this.select_destination(cx))),
                )
                .child(
                    div().h_flex().gap_2().justify_end().child(
                        Button::new("add-project-confirm")
                            .small()
                            .primary()
                            .disabled(self.busy)
                            .label(match (mode, self.busy) {
                                (AddMode::Clone, true) => t!("add_project.cloning"),
                                (AddMode::New, true) => t!("add_project.creating"),
                                (AddMode::Clone, false) => t!("add_project.clone_btn"),
                                (AddMode::New, false) => t!("add_project.create_btn"),
                            })
                            .on_click(cx.listener(|this, _, _, cx| this.confirm(cx))),
                    ),
                );
            form = form.child(fields);
        }

        if let Some(error) = self.error.clone() {
            form = form.child(
                div()
                    .id("add-project-error")
                    .test_support()
                    .role(Role::Alert)
                    .aria_label(SharedString::from(error.clone()))
                    .text_xs()
                    .text_color(colors.destructive)
                    .child(SharedString::from(error)),
            );
        }

        form
    }
}

/// A big row: an icon in a rounded square, a title and a line under
/// it, and an optional glyph at the trailing end.
fn add_card(
    id: &'static str,
    icon: IconName,
    title: &str,
    detail: &str,
    colors: &ColorTokens,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    div()
        .id(id)
        .test_support()
        .aria_label(SharedString::from(title))
        .h_flex()
        .items_center()
        .gap_3()
        .w_full()
        .p_2()
        .rounded_md()
        .border_1()
        .border_color(colors.border)
        .cursor_pointer()
        .hover(|el| el.bg(colors.muted))
        .child(icon_tile(icon, colors))
        .child(
            div()
                .v_flex()
                .flex_1()
                .min_w_0()
                .gap_1()
                .child(
                    div()
                        .text_sm()
                        .font_weight(FontWeight::MEDIUM)
                        .child(SharedString::from(title)),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(colors.muted_foreground)
                        .child(SharedString::from(detail)),
                ),
        )
        .on_click(on_click)
        .into_any_element()
}

/// One row of the "other ways" group: the same shape, without the border (the
/// group has one) and with the selected state these rows have.
fn add_row(
    id: &'static str,
    icon: IconName,
    title: &str,
    detail: &str,
    colors: &ColorTokens,
    selected: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    div()
        .id(id)
        .test_support()
        .aria_label(SharedString::from(title))
        .aria_selected(selected)
        .h_flex()
        .items_center()
        .gap_3()
        .w_full()
        .p_2()
        .cursor_pointer()
        .when(selected, |el| el.bg(colors.muted))
        .hover(|el| el.bg(colors.muted))
        .child(icon_tile(icon, colors))
        .child(
            div()
                .v_flex()
                .flex_1()
                .min_w_0()
                .gap_1()
                .child(
                    div()
                        .text_sm()
                        .font_weight(FontWeight::MEDIUM)
                        .child(SharedString::from(title)),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(colors.muted_foreground)
                        .child(SharedString::from(detail)),
                ),
        )
        .on_click(on_click)
        .into_any_element()
}

/// The rounded square an icon sits in, on the leading edge of every row here.
fn icon_tile(icon: IconName, colors: &ColorTokens) -> AnyElement {
    div()
        .flex_shrink_0()
        .size(px(30.))
        .flex()
        .items_center()
        .justify_center()
        .rounded_md()
        .bg(colors.muted)
        .child(Icon::new(icon).size(px(16.)))
        .into_any_element()
}

/// The new-worktree form.
pub struct NewWorktree {
    state: Entity<AppState>,
    /// The window the form is in: what closing it needs, and the only reason
    /// this form holds a handle rather than asking for the active window.
    window: AnyWindowHandle,
    /// Who to tell once the worktree exists and has an agent to run. Weak: the
    /// workspace owns this form's lifetime, not the other way round.
    workspace: WeakEntity<Workspace>,
    repo_id: RepoId,
    repo_name: String,
    /// The repo's own path, for the "Runs on" row.
    repo_path: PathBuf,
    /// The base ref the field opened with, so "Create more" can put it back.
    base_ref: String,
    name: Entity<InputState>,
    base: Entity<InputState>,
    /// The preset the new terminal will run in the worktree. `None` is a plain
    /// shell, which is what the workspace would have started anyway.
    agent: Option<AgentId>,
    /// Every preset the picker offers, resolved once rather than per frame.
    agents: Vec<AgentSpec>,
    /// Whether the project picker is open. Inline rather than a popover: this is
    /// a dialog, and a second layer inside a modal is a layer nobody can see the
    /// edge of.
    repos_open: bool,
    /// Whether the "Advanced" fold is open. Folded by default, because the two
    /// fields in it are the two a user changes least.
    advanced_open: bool,
    /// "Create more": stay open after a worktree is made, for the second
    /// and third one the same repo wants.
    create_more: bool,
    /// What this form itself has to say (`name_problem`). git's complaint is
    /// read from the state, so that one has one owner.
    local_error: Option<String>,
    /// Set while the create is out. It is what tells an error apart from a
    /// stale one, and what stops a second Confirm from starting a second call.
    creating: bool,
    _events: Subscription,
    _inputs: Vec<Subscription>,
}

impl NewWorktree {
    pub fn new(
        state: Entity<AppState>,
        workspace: WeakEntity<Workspace>,
        repo_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let (repo_name, repo_path, base_ref, agents) = {
            let state = state.read(cx);
            let repo = state.repos().iter().find(|r| r.id == repo_id);
            (
                repo.map(|r| r.name.clone())
                    .unwrap_or_else(|| repo_id.to_string()),
                repo.map(|r| r.path.clone()).unwrap_or_default(),
                // git's answer, or empty when there is none: an empty base asks
                // the state to resolve it at create time, which is the same
                // answer one call later.
                repo.and_then(|r| git::default_base_ref(&r.path).ok())
                    .unwrap_or_default(),
                state.agents(),
            )
        };

        let handle = window.window_handle();
        let name = cx.new(|cx| InputState::new(window, cx).placeholder("my-feature"));
        let base = cx.new(|cx| InputState::new(window, cx).default_value(base_ref.clone()));
        // The branch line under the name is derived from what is typed, so the
        // form has to redraw while the field changes; the input's own event is
        // the only thing that says so.
        let inputs = vec![
            cx.subscribe(&name, |_this, _input, _event: &InputEvent, cx| cx.notify()),
            cx.subscribe(&base, |_this, _input, _event: &InputEvent, cx| cx.notify()),
        ];

        let events = cx.observe(&state, |this, state, cx| {
            let (pending, error) = {
                let state = state.read(cx);
                (
                    state.pending_for(&this.repo_id).is_some(),
                    state.error_for(&this.repo_id).map(str::to_string),
                )
            };
            // The call that was out has landed.
            if this.creating && !pending && error.is_none() {
                this.creating = false;
                this.start_agent(cx);
                if this.create_more {
                    // The fields are cleared for the next one, and the dialog stays: that is the whole point of the
                    // toggle.
                    this.local_error = None;
                    let window = this.window;
                    let name = this.name.clone();
                    let base = this.base.clone();
                    let base_ref = this.base_ref.clone();
                    cx.defer(move |cx| {
                        let _ = window.update(cx, |_, window, cx| {
                            name.update(cx, |input, cx| input.set_value("", window, cx));
                            base.update(cx, |input, cx| {
                                input.set_value(base_ref.clone(), window, cx)
                            });
                        });
                    });
                    cx.notify();
                    return;
                }
                close(this.window, cx);
                return;
            }
            if this.creating && !pending {
                this.creating = false;
            }
            cx.notify();
        });

        Self {
            state,
            window: handle,
            workspace,
            repo_id: repo_id.to_string(),
            repo_name,
            repo_path,
            base_ref,
            name,
            base,
            agent: None,
            agents,
            repos_open: false,
            advanced_open: false,
            create_more: false,
            local_error: None,
            creating: false,
            _events: events,
            _inputs: inputs,
        }
    }

    /// Run the chosen preset in the worktree that was just created.
    fn start_agent(&self, cx: &mut Context<Self>) {
        let Some(agent) = self.agent.clone() else {
            return;
        };
        let Some(worktree_id) = self.state.read(cx).active_id().cloned() else {
            return;
        };
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let action = terminal::New {
            worktree_id,
            agent: Some(agent),
            resume: None,
            seed: None,
            model: None,
        };
        cx.defer(move |cx| {
            workspace.update(cx, |workspace, cx| workspace.start_terminal(&action, cx));
        });
    }

    /// Confirm. The create goes out as the action, not as a call into the state,
    /// because the action is the one command channel: whatever a key or a
    /// menu would dispatch, this button dispatches.
    fn confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.creating {
            return;
        }
        let name = self.name.read(cx).value().to_string();
        match worktree_name_problem(&name) {
            Some(problem) => {
                self.local_error = Some(problem.to_string());
                cx.notify();
            }
            None => {
                self.local_error = None;
                self.creating = true;
                let base = self.base.read(cx).value().to_string();
                window.dispatch_action(
                    Box::new(worktree::Create {
                        repo_id: self.repo_id.clone(),
                        name,
                        // An empty field means "the repo's default", which is
                        // resolved when the call runs rather than guessed here.
                        base: (!base.trim().is_empty()).then_some(base),
                    }),
                    cx,
                );
                cx.notify();
            }
        }
    }

    fn cancel(&mut self, cx: &mut Context<Self>) {
        close(self.window, cx);
    }

    fn message(&self, cx: &App) -> Option<String> {
        self.local_error.clone().or_else(|| {
            self.state
                .read(cx)
                .error_for(&self.repo_id)
                .map(str::to_string)
        })
    }
}

impl Render for NewWorktree {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = Theme::global(cx).tokens.colors;
        let name_text = self.name.read(cx).value().to_string();
        let branch = git::branch_from_name(&name_text);
        let error = self.message(cx);
        let selected = self.agent.clone();
        // Where this worktree will be, live: the field under it is a name, and
        // the path is what the name makes. A "runs on" row is the same idea
        // with a host in front of it, and this app has one host.
        let target = self
            .state
            .read(cx)
            .worktree_target(&self.repo_id, name_text.trim())
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| self.repo_path.display().to_string());

        // Capped and scrolling: the dialog has a height of its own choosing, and
        // a form taller than it is a form whose button cannot be clicked — which
        // is how this cap was found. Twenty configured agents still fit.
        let mut form = div()
            .id("new-worktree")
            .test_support()
            .v_flex()
            .w_full()
            .max_h(px(400.))
            .overflow_y_scroll()
            .gap_2()
            .text_sm()
            // The key the footer's chip names.
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if event.keystroke.modifiers.platform
                    && (event.keystroke.key == "enter" || event.keystroke.key == "return")
                {
                    cx.stop_propagation();
                    this.confirm(window, cx);
                }
            }))
            .child(field(
                &t!("new_worktree.project"),
                div()
                    .v_flex()
                    .gap_1()
                    .w_full()
                    .child(self.project_row(&colors, cx))
                    // Where it will run, folded into this row as a line.
                    .child(
                        div()
                            .id("new-worktree-runs-on")
                            .test_support()
                            .aria_label(SharedString::from(
                                t!("new_worktree.runs_on", target = target).to_string(),
                            ))
                            .h_flex()
                            .items_center()
                            .gap_1()
                            .w_full()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .text_xs()
                            .text_color(colors.muted_foreground)
                            .child(Icon::new(IconName::HardDrive).size(px(12.)))
                            .child(SharedString::from(
                                t!("new_worktree.runs_on", target = target).to_string(),
                            )),
                    )
                    .into_any_element(),
                &colors,
            ));

        if self.repos_open {
            form = form.child(self.project_picker(&colors, cx));
        }

        form = form
            // No label over the name field: the placeholder is one ("my-feature")
            // and the branch line under it says what the name is for, which is
            // the whole of what a label would add.
            .child(
                div()
                    .v_flex()
                    .gap_1()
                    .w_full()
                    .child(
                        Input::new(&self.name)
                            .focus_bordered(false)
                            .id("new-worktree-name")
                            .w_full(),
                    )
                    .child(
                        div()
                            .id("new-worktree-branch")
                            .test_support()
                            // The branch is what git will be told to create, so
                            // it is shown as it is derived rather than after the
                            // fact.
                            .aria_label(SharedString::from(
                                t!("new_worktree.branch", branch = branch).to_string(),
                            ))
                            .text_xs()
                            .text_color(colors.muted_foreground)
                            .child(SharedString::from(
                                t!("new_worktree.branch", branch = branch).to_string(),
                            )),
                    )
                    .into_any_element(),
            )
            .child(field(
                &t!("new_worktree.agent"),
                self.agent_picker(&colors, selected.as_deref(), cx),
                &colors,
            ))
            .child(self.advanced(&colors, cx));

        if let Some(error) = error {
            form = form.child(
                div()
                    .id("new-worktree-error")
                    .test_support()
                    .role(Role::Alert)
                    .aria_label(SharedString::from(error.clone()))
                    .text_xs()
                    .text_color(colors.destructive)
                    .child(SharedString::from(error)),
            );
        }

        // The footer: the toggle on the left, the primary action on the
        // right, with the key that does the same thing beside it.
        form.child(
            div()
                .h_flex()
                .items_center()
                .justify_between()
                .w_full()
                .child(
                    Switch::new("new-worktree-create-more")
                        .checked(self.create_more)
                        .label(t!("new_worktree.create_more").to_string())
                        .on_click(cx.listener(|this, checked, _, cx| {
                            this.create_more = *checked;
                            cx.notify();
                        })),
                )
                .child(
                    div()
                        .h_flex()
                        .items_center()
                        .gap_2()
                        .child(
                            Button::new("new-worktree-cancel")
                                .small()
                                .outline()
                                .label(t!("common.cancel"))
                                .on_click(cx.listener(|this, _, _, cx| this.cancel(cx))),
                        )
                        .child(
                            Button::new("new-worktree-create")
                                .small()
                                .primary()
                                .label(if self.creating {
                                    t!("new_worktree.creating")
                                } else {
                                    t!("new_worktree.create")
                                })
                                .disabled(self.creating)
                                .on_click(
                                    cx.listener(|this, _, window, cx| this.confirm(window, cx)),
                                ),
                        )
                        .child(div().text_xs().text_color(colors.muted_foreground).child(
                            SharedString::from(Kbd::format(
                                &Keystroke::parse(SUBMIT).expect("a keystroke"),
                            )),
                        )),
                ),
        )
    }
}

/// The key that presses the primary button, and the spelling both the keymap
/// and the footer chip use.
const SUBMIT: &str = "cmd-enter";

impl NewWorktree {
    /// The project row: which repository the worktree is of. A row rather than a
    /// dropdown for the same reason the agent list is one — this is a modal, and
    /// a modal that spawns a second layer is a modal with two edges.
    fn project_row(&self, colors: &ColorTokens, cx: &mut Context<Self>) -> AnyElement {
        // One repository is not a choice: the row says which one and does not
        // pretend there is another.
        let pickable = self.state.read(cx).repos().len() > 1;
        let row = div()
            .id("new-worktree-project")
            .test_support()
            .aria_label(SharedString::from(self.repo_name.clone()))
            .h_flex()
            .items_center()
            .gap_2()
            .w_full()
            .px_2()
            .py_1()
            .rounded_md()
            .border_1()
            .border_color(colors.border)
            .when(pickable, |el| {
                el.cursor_pointer().hover(|el| el.bg(colors.muted))
            })
            .child(icon_tile(IconName::Folder, colors))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .text_ellipsis()
                    .child(SharedString::from(self.repo_name.clone())),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .max_w(px(180.))
                    .overflow_hidden()
                    .text_ellipsis()
                    .text_xs()
                    .text_color(colors.muted_foreground)
                    .child(SharedString::from(self.repo_path.display().to_string())),
            )
            .child(
                Icon::new(if self.repos_open {
                    IconName::ChevronUp
                } else {
                    IconName::ChevronDown
                })
                .size(px(14.)),
            )
            .when(pickable, |el| {
                el.on_click(cx.listener(|this, _, _, cx| {
                    this.repos_open = !this.repos_open;
                    cx.notify();
                }))
            });
        row.into_any_element()
    }

    /// The repository list, inline under the row.
    fn project_picker(&self, colors: &ColorTokens, cx: &mut Context<Self>) -> AnyElement {
        let repos: Vec<(String, String, PathBuf)> = self
            .state
            .read(cx)
            .repos()
            .iter()
            .map(|repo| (repo.id.clone(), repo.name.clone(), repo.path.clone()))
            .collect();
        let mut list = div()
            .id("new-worktree-repos")
            .test_support()
            .v_flex()
            .w_full()
            .rounded_md()
            .border_1()
            .border_color(colors.border)
            .overflow_hidden();
        for (index, (id, name, path)) in repos.into_iter().enumerate() {
            let chosen = id == self.repo_id;
            if index > 0 {
                list = list.child(div().h(px(1.)).w_full().bg(colors.border));
            }
            list = list.child(
                div()
                    .id(SharedString::from(format!("new-worktree-repo-{id}")))
                    .test_support()
                    .aria_label(SharedString::from(name.clone()))
                    .aria_selected(chosen)
                    .h_flex()
                    .items_center()
                    .gap_2()
                    .w_full()
                    .px_2()
                    .py_1()
                    .cursor_pointer()
                    .text_xs()
                    .when(chosen, |el| el.bg(colors.muted))
                    .hover(|el| el.bg(colors.muted))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .child(SharedString::from(name.clone())),
                    )
                    .child(
                        div()
                            .text_color(colors.muted_foreground)
                            .child(SharedString::from(path.display().to_string())),
                    )
                    .when(chosen, |el| {
                        el.child(Icon::new(IconName::Check).size(px(12.)))
                    })
                    .on_click(
                        cx.listener(move |this, _, window, cx| this.choose_repo(&id, window, cx)),
                    ),
            );
        }
        list.into_any_element()
    }

    /// Point the form at another repository: the name, the path and the base ref
    /// all come from the repo that was picked, and the base field is reset
    /// because the old ref belongs to the old repository.
    fn choose_repo(&mut self, repo_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let (name, path, base) = {
            let state = self.state.read(cx);
            let Some(repo) = state.repos().iter().find(|repo| repo.id == repo_id) else {
                return;
            };
            (
                repo.name.clone(),
                repo.path.clone(),
                git::default_base_ref(&repo.path).unwrap_or_default(),
            )
        };
        self.repo_id = repo_id.to_string();
        self.repo_name = name;
        self.repo_path = path;
        self.base_ref = base.clone();
        self.base
            .update(cx, |input, cx| input.set_value(base, window, cx));
        self.repos_open = false;
        self.local_error = None;
        cx.notify();
    }

    /// The "Advanced" fold: the one field a user changes least, and the reason
    /// the form is short enough to read at a glance.
    fn advanced(&self, colors: &ColorTokens, cx: &mut Context<Self>) -> AnyElement {
        let mut section = div()
            .id("new-worktree-advanced-section")
            .test_support()
            .v_flex()
            .gap_2()
            .w_full()
            .child(
                div()
                    .id("new-worktree-advanced")
                    .test_support()
                    .aria_label(t!("new_worktree.advanced"))
                    .h_flex()
                    .items_center()
                    .gap_1()
                    .cursor_pointer()
                    .text_xs()
                    .font_weight(FontWeight::MEDIUM)
                    .hover(|el| el.text_color(colors.foreground))
                    .child(t!("new_worktree.advanced"))
                    .child(
                        Icon::new(if self.advanced_open {
                            IconName::ChevronDown
                        } else {
                            IconName::ChevronRight
                        })
                        .size(px(12.)),
                    )
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.advanced_open = !this.advanced_open;
                        cx.notify();
                    })),
            );
        if self.advanced_open {
            // One row: the label, the field, and what an empty field means. The
            // dialog is a fixed height, and a stacked field costs the line the
            // footer needs.
            section = section.child(
                div()
                    .h_flex()
                    .items_center()
                    .gap_2()
                    .w_full()
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_xs()
                            .text_color(colors.muted_foreground)
                            .child(t!("new_worktree.start_from")),
                    )
                    .child(
                        div().flex_1().min_w_0().child(
                            Input::new(&self.base)
                                .focus_bordered(false)
                                .id("new-worktree-base"),
                        ),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_xs()
                            .text_color(colors.muted_foreground)
                            .child(t!("new_worktree.start_from_hint")),
                    ),
            );
        }
        section.into_any_element()
    }
}

impl NewWorktree {
    /// What the worktree's first terminal will run: a plain shell, or one of the configured
    /// presets.
    fn agent_picker(
        &self,
        colors: &ColorTokens,
        selected: Option<&str>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // A wrapped row of chips inside one bordered group, not a list of rows:
        let mut group = div()
            .id("new-worktree-agents")
            .test_support()
            .h_flex()
            .flex_wrap()
            .gap_1()
            .w_full()
            .p_1()
            .rounded_md()
            .border_1()
            .border_color(colors.border);
        // A plain shell first, then the presets in configuration order.
        let mut choices: Vec<(String, String, bool)> = vec![(
            "new-worktree-agent-shell".to_string(),
            t!("new_worktree.shell").to_string(),
            selected.is_none(),
        )];
        for agent in &self.agents {
            choices.push((
                format!("new-worktree-agent-{}", agent.id),
                agent.name.clone(),
                selected == Some(agent.id.as_str()),
            ));
        }
        for (id, label, chosen) in choices {
            let picked = id.trim_start_matches("new-worktree-agent-").to_string();
            let is_shell = picked == "shell";
            group = group.child(
                div()
                    .id(SharedString::from(id))
                    .test_support()
                    .aria_label(SharedString::from(match is_shell {
                        true => label.clone(),
                        false => t!("new_worktree.run_agent", name = label).to_string(),
                    }))
                    .aria_selected(chosen)
                    .h_flex()
                    .items_center()
                    .gap_1()
                    .px_2()
                    .py_0p5()
                    .rounded_sm()
                    .border_1()
                    .border_color(if chosen {
                        colors.primary
                    } else {
                        colors.border
                    })
                    .cursor_pointer()
                    .text_xs()
                    .when(chosen, |el| el.bg(colors.muted))
                    .hover(|el| el.bg(colors.muted))
                    .child(
                        Icon::new(match is_shell {
                            true => IconName::SquareTerminal,
                            false => IconName::Bot,
                        })
                        .size(px(12.)),
                    )
                    .child(SharedString::from(label))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.agent = (!is_shell).then(|| picked.clone());
                        cx.notify();
                    })),
            );
        }
        group.into_any_element()
    }
}

/// A labelled field: the name, and what it holds. Every field in this form has
/// one, which is what makes the form read as a list of decisions.
fn field(label: &str, body: AnyElement, colors: &ColorTokens) -> AnyElement {
    div()
        .v_flex()
        .gap_1()
        .w_full()
        .child(
            div()
                .text_xs()
                .text_color(colors.muted_foreground)
                .child(SharedString::from(label)),
        )
        .child(body)
        .into_any_element()
}

/// The removal confirmation.
pub struct RemoveWorktree {
    state: Entity<AppState>,
    workspace: WeakEntity<Workspace>,
    /// See [`NewWorktree::window`].
    window: AnyWindowHandle,
    repo_id: RepoId,
    worktree_id: WorktreeId,
    name: String,
    branch: String,
    /// Set by the first removal. Until then git's error, if there is one, is
    /// about something else and does not belong under this dialog.
    asked: bool,
    /// The repository's own checkout, which git will not remove. The dialog asks
    /// a different question for it — remove the *project* — because the forced
    /// retry it otherwise offers is one that can never work.
    own_checkout: bool,
    _events: Subscription,
}

impl RemoveWorktree {
    pub fn new(
        state: Entity<AppState>,
        workspace: WeakEntity<Workspace>,
        worktree_id: &str,
        window: AnyWindowHandle,
        cx: &mut Context<Self>,
    ) -> Self {
        let (target, own_checkout) = {
            let state = state.read(cx);
            (
                state.worktree(worktree_id).cloned(),
                state.is_own_checkout(worktree_id),
            )
        };
        let events = cx.subscribe(&state, |this, _state, event: &CoreEvent, cx| {
            if let CoreEvent::WorktreeRemoved { worktree } = event
                && worktree.id == this.worktree_id
            {
                close(this.window, cx);
            }
            cx.notify();
        });
        Self {
            state,
            workspace,
            window,
            repo_id: target
                .as_ref()
                .map(|w| w.repo_id.clone())
                .unwrap_or_default(),
            worktree_id: worktree_id.to_string(),
            name: target
                .as_ref()
                .map(|w| w.name.clone())
                .unwrap_or_else(|| worktree_id.to_string()),
            branch: target.map(|w| w.branch).unwrap_or_default(),
            asked: false,
            own_checkout,
            _events: events,
        }
    }

    fn ask(&mut self, force: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.asked = true;
        // A terminal's current directory prevents Windows from deleting the worktree.
        if let Some(workspace) = self.workspace.upgrade() {
            workspace.update(cx, |workspace, cx| {
                workspace.forget_worktree(&self.worktree_id, cx)
            });
        }
        window.dispatch_action(
            Box::new(worktree::Remove {
                worktree_id: self.worktree_id.clone(),
                force,
            }),
            cx,
        );
        cx.notify();
    }
}

impl Render for RemoveWorktree {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = Theme::global(cx).tokens.colors;
        let pending = self.state.read(cx).pending_for(&self.repo_id).is_some();
        let refusal = if self.asked && !pending {
            self.state
                .read(cx)
                .error_for(&self.repo_id)
                .map(str::to_string)
        } else {
            None
        };
        let name = self.name.clone();
        let branch = self.branch.clone();
        let own_checkout = self.own_checkout;

        let mut form =
            div()
                .id("remove-worktree")
                .test_support()
                .v_flex()
                .w_full()
                .gap_2()
                .text_sm()
                .child(
                    div()
                        .id("remove-worktree-what")
                        .test_support()
                        .aria_label(SharedString::from(
                            t!("new_worktree.remove_aria", name = name).to_string(),
                        ))
                        .child(SharedString::from(
                            t!("new_worktree.remove_question", name = name).to_string(),
                        )),
                )
                .child(div().text_xs().text_color(colors.muted_foreground).child(
                    SharedString::from(if own_checkout {
                        // What git will not do, said plainly, with the one thing
                        // that can be done about this row instead.
                        t!(
                            "new_worktree.remove_own_checkout",
                            name = name,
                            branch = branch
                        )
                        .to_string()
                    } else {
                        t!("new_worktree.remove_with_branch", branch = branch).to_string()
                    }),
                ));

        if let Some(refusal) = refusal.as_ref() {
            form = form.child(
                div()
                    .id("remove-worktree-error")
                    .test_support()
                    .role(Role::Alert)
                    .aria_label(SharedString::from(refusal.clone()))
                    .text_xs()
                    .text_color(colors.destructive)
                    .child(SharedString::from(refusal)),
            );
        }

        let mut buttons = div().h_flex().gap_2().justify_end().child(
            Button::new("remove-worktree-cancel")
                .small()
                .outline()
                .label(t!("common.cancel"))
                .on_click(cx.listener(|this, _, _, cx| close(this.window, cx))),
        );

        // A refusal is the moment the forced retry means something: git says why it will not go, and
        // the caller decides whether it goes anyway.
        if own_checkout {
            buttons = buttons.child(
                Button::new("remove-worktree-project")
                    .small()
                    .danger()
                    .label(t!("dialog.remove_project"))
                    .on_click(cx.listener(|this, _, window, cx| {
                        window.dispatch_action(
                            Box::new(crate::state::repo::Remove {
                                repo_id: this.repo_id.clone(),
                            }),
                            cx,
                        );
                    })),
            );
        } else if refusal.is_some() {
            buttons = buttons.child(
                Button::new("remove-worktree-force")
                    .small()
                    .danger()
                    .label(t!("new_worktree.remove_anyway"))
                    .on_click(cx.listener(|this, _, window, cx| this.ask(true, window, cx))),
            );
        } else {
            buttons = buttons.child(
                Button::new("remove-worktree-remove")
                    .small()
                    .danger()
                    .disabled(pending)
                    .label(if pending {
                        t!("new_worktree.removing")
                    } else {
                        t!("new_worktree.remove")
                    })
                    .on_click(cx.listener(|this, _, window, cx| this.ask(false, window, cx))),
            );
        }

        form.child(buttons)
    }
}

#[cfg(test)]
mod tests {
    /// The rule lives in core and the sentence in the state; this pins that the
    /// form refuses what `git worktree add` would otherwise be handed.
    use crate::state::worktree_name_problem as name_problem;

    #[test]
    fn a_name_that_is_not_a_directory_name_is_refused() {
        assert_eq!(name_problem("my-feature"), None);
        assert_eq!(name_problem("  spaced  "), None);
        // Not a name: a path, the two dots that mean a directory, and the
        // separators a Windows user would type.
        assert!(name_problem("///").is_some());
        assert!(name_problem("a/b").is_some());
        assert!(name_problem("a\\b").is_some());
        assert!(name_problem(".").is_some());
        assert!(name_problem("..").is_some());
        // Nothing to slug a branch out of.
        assert!(name_problem("---").is_some());
        assert!(name_problem("").is_some());
        assert!(name_problem("   ").is_some());
    }
}

/// The forms in a real (headless) window, driven the way a user drives them: a click on `+`,
/// typing into a field, a click on a button.
#[cfg(test)]
mod window_tests {
    use std::path::PathBuf;

    use gpui_kit::base::Root;
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{
        App, AppContext, Bounds, Entity, Point, TestAppContext, WindowBounds, WindowHandle,
        WindowOptions, px, size,
    };
    use syro_core::config::Settings;
    use syro_core::model::{AgentSpec, State};
    use syro_core::store::Store;

    use crate::Workspace;
    use crate::pane::PaneItem as _;
    use crate::state::fixture::{scratch_repo, temp_store};
    use crate::state::{AppState, register_actions, terminal};

    struct Fixture {
        window: WindowHandle<Root>,
        workspace: Entity<Workspace>,
        /// The state the window was built over: what a test asks about a
        /// repository that arrived through the dialog.
        state: Entity<AppState>,
        repo_id: String,
        dir: PathBuf,
        worktrees: Vec<PathBuf>,
        _store_dir: PathBuf,
    }

    fn open(cx: &mut TestAppContext, tag: &str) -> Fixture {
        open_with(cx, tag, Settings::default())
    }

    fn open_with(cx: &mut TestAppContext, tag: &str, settings: Settings) -> Fixture {
        let (dir, repo, mut worktrees) = scratch_repo(tag);
        let (state_path, store_dir) = temp_store(tag);
        Store::new(state_path.clone())
            .save(&State {
                repos: vec![repo.clone()],
                ..Default::default()
            })
            .unwrap();
        let repo_id = repo.id.clone();

        let mut handle: Option<Entity<Workspace>> = None;
        let mut built_state: Option<Entity<AppState>> = None;
        let (window, _) = cx.update(|cx| {
            // The component layer owns the window's dialog stack; without it
            // `open_dialog` has nowhere to put one.
            gpui_kit::init(cx);
            let bounds = Bounds {
                origin: Point::default(),
                size: size(px(900.), px(600.)),
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
                    built_state = Some(state.clone());
                    let workspace = cx.new(|cx| Workspace::new(cx, state.clone()));
                    // The workspace's own action, registered the way `main`
                    // registers it: a click on Restart has no other listener.
                    cx.on_action({
                        let workspace = workspace.clone();
                        move |action: &terminal::New, cx: &mut App| {
                            workspace
                                .update(cx, |workspace, cx| workspace.start_terminal(action, cx));
                        }
                    });
                    cx.on_action({
                        let workspace = workspace.clone();
                        move |action: &terminal::Restart, cx: &mut App| {
                            workspace
                                .update(cx, |workspace, cx| workspace.restart_terminal(action, cx));
                        }
                    });
                    handle = Some(workspace.clone());
                    workspace
                },
            )
            .expect("open test window");
            (window.downcast::<Root>().expect("Base Root"), workspace)
        });

        let mut fixture = Fixture {
            window,
            workspace: handle.expect("the workspace was built"),
            state: built_state.expect("the state was built"),
            repo_id,
            dir,
            worktrees: Vec::new(),
            _store_dir: store_dir,
        };
        draw(cx, &fixture);
        worktrees.sort();
        fixture.worktrees = worktrees;
        fixture
    }

    /// Type into a field until an observable effect says the text landed.
    fn type_until(
        cx: &mut TestAppContext,
        fixture: &Fixture,
        id: &str,
        text: &str,
        landed: impl Fn(&mut TestAppContext, &Fixture) -> bool,
    ) {
        for _ in 0..3 {
            type_into(cx, fixture, id, text);
            if landed(cx, fixture) {
                return;
            }
        }
        panic!("typing {text:?} into {id} never landed");
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

    /// Draw frames until `done`, the way a real event loop would: an action is
    /// dispatched on the next cycle and a git call lands after the background
    /// executor has run.
    fn draw_until(
        cx: &mut TestAppContext,
        fixture: &Fixture,
        done: impl Fn(&mut TestAppContext, &Fixture) -> bool,
    ) -> bool {
        // The budget is wall clock, not a frame count: a frame count is a budget that shrinks as the
        // machine gets busier, which is how a test that passes alone fails in a full parallel
        // run.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while std::time::Instant::now() < deadline {
            cx.background_executor
                .advance_clock(std::time::Duration::from_millis(20));
            cx.run_until_parked();
            draw(cx, fixture);
            if done(cx, fixture) {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        false
    }

    /// Press a key at the window, then draw: a key goes to the focused element
    /// and bubbles from there, so the field the form is being filled in has to
    /// have the focus for the form's own handler to see it.
    fn press(cx: &mut TestAppContext, fixture: &Fixture, key: &str) {
        let key = key.to_string();
        cx.update_window(fixture.window.into(), move |_, window, cx| {
            window.press(&key, cx)
        })
        .expect("press");
        draw(cx, fixture);
    }

    /// Click an element the frame reports clickable, waiting for the frame that
    /// says so: `window.click` refuses one the last frame called invisible, and
    /// an element inside a scrolling list is that until it has been scrolled to.
    fn click_visible(cx: &mut TestAppContext, fixture: &Fixture, id: &str) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            draw(cx, fixture);
            let clickable = cx
                .update_window(fixture.window.into(), {
                    let id = id.to_string();
                    move |_, window, _| window.try_find(id).is_some_and(|element| element.visible())
                })
                .expect("read the frame");
            if clickable {
                click(cx, fixture, id);
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("{id} was never reported clickable");
    }

    /// Click, then draw: an element listener runs on the next cycle, and a
    /// dialog opened from one is in the frame after that.
    fn click(cx: &mut TestAppContext, fixture: &Fixture, id: &str) {
        cx.update_window(fixture.window.into(), |_, window, cx| {
            window.click(id.to_string(), cx);
        })
        .expect("click");
        draw(cx, fixture);
    }

    /// Open a row's menu: the right button is what does it, and `right_click`
    /// sends the platform's own aux click.
    fn right_click(cx: &mut TestAppContext, fixture: &Fixture, id: String) {
        cx.update_window(fixture.window.into(), |_, window, cx| {
            window.right_click(id, cx);
        })
        .expect("right click");
        draw(cx, fixture);
    }

    fn type_into(cx: &mut TestAppContext, fixture: &Fixture, id: &str, text: &str) {
        cx.update_window(fixture.window.into(), |_, window, cx| {
            window.click(id.to_string(), cx);
            window.input(text, cx);
        })
        .expect("type");
        draw(cx, fixture);
    }

    /// Click `id` until `done`, a few times at most.
    fn click_until(
        cx: &mut TestAppContext,
        fixture: &Fixture,
        id: &str,
        done: impl Fn(&mut TestAppContext, &Fixture) -> bool,
    ) -> bool {
        for _ in 0..3 {
            click(cx, fixture, id);
            if draw_until(cx, fixture, &done) {
                return true;
            }
        }
        false
    }

    fn pick_project_directory(cx: &mut TestAppContext, fixture: &Fixture, path: Option<PathBuf>) {
        for _ in 0..3 {
            click(cx, fixture, "add-project-destination");
            if cx.did_prompt_for_paths() {
                break;
            }
        }
        assert!(cx.did_prompt_for_paths(), "the folder picker did not open");
        cx.simulate_path_prompt_response(|options| {
            assert!(options.directories);
            assert!(!options.files);
            assert!(!options.multiple);
            path.map(|path| vec![path])
        });
        draw(cx, fixture);
    }

    fn found(cx: &mut TestAppContext, fixture: &Fixture, id: &str) -> bool {
        cx.update_window(fixture.window.into(), |_, window, _| {
            window.try_find(id.to_string()).is_some()
        })
        .expect("read the frame")
    }

    /// What an element says it is, which is how a sentence drawn for a reader is
    /// read back by a test.
    fn label(cx: &mut TestAppContext, fixture: &Fixture, id: &str) -> Option<String> {
        cx.update_window(fixture.window.into(), |_, window, _| {
            window
                .try_find(id.to_string())
                .and_then(|el| el.label().map(str::to_string))
        })
        .expect("read the frame")
    }

    fn row_is_listed(cx: &mut TestAppContext, fixture: &Fixture, path: &std::path::Path) -> bool {
        found(cx, fixture, &format!("worktree-{}", path.display()))
    }

    /// The worktree the form would create for `name`, spelled the way git does.
    fn destination(fixture: &Fixture, name: &str) -> PathBuf {
        fixture
            .dir
            .canonicalize()
            .unwrap_or_else(|_| fixture.dir.clone())
            .join("workspaces")
            .join("main")
            .join(name)
    }

    /// Open the form the way a user does: the `...` on the repo's heading, and
    /// then the entry that makes a worktree.
    fn open_form(cx: &mut TestAppContext, fixture: &Fixture) {
        let menu = format!("repo-menu-{}", fixture.repo_id);
        click(cx, fixture, &menu);
        assert!(
            found(cx, fixture, "repo-menu-new"),
            "the heading's menu did not open"
        );
        click(cx, fixture, "repo-menu-new");
        assert!(
            found(cx, fixture, "new-worktree-name"),
            "the form did not open"
        );
    }

    /// The form's footer: the "Create more" toggle and the key its chip names.
    #[gpui_kit::test]
    fn create_more_keeps_the_form_up_and_cmd_enter_creates(cx: &mut TestAppContext) {
        let fixture = open(cx, "dialog-create-more");
        open_form(cx, &fixture);

        // The chip is the key: it is on screen, and the key does the same thing.
        assert!(
            found(cx, &fixture, "new-worktree-create"),
            "the form has no primary button"
        );
        type_into(cx, &fixture, "new-worktree-name", "first");
        press(cx, &fixture, "cmd-enter");
        assert!(
            draw_until(cx, &fixture, |_cx, fixture| destination(fixture, "first")
                .exists()),
            "cmd-enter did not create the worktree"
        );
        assert!(
            !found(cx, &fixture, "new-worktree"),
            "the form stayed up without Create more"
        );

        // With the toggle on, the form stays and is ready for the next one.
        let mut kept = false;
        for attempt in 0..3 {
            let name = format!("more-{attempt}");
            open_form(cx, &fixture);
            click_visible(cx, &fixture, "new-worktree-create-more");
            draw(cx, &fixture);
            type_into(cx, &fixture, "new-worktree-name", &name);
            press(cx, &fixture, "cmd-enter");
            let made = draw_until(cx, &fixture, |_cx, fixture| {
                destination(fixture, &name).exists()
            });
            assert!(made, "the worktree {name} was not made");
            if found(cx, &fixture, "new-worktree") {
                kept = true;
                break;
            }
        }
        assert!(kept, "Create more did not keep the form up");
    }

    /// The Add-project dialog's create door: a name, a place, and the repository
    /// is made and added — the same road the folder picker takes, so all three
    /// doors end in one place.
    #[gpui_kit::test]
    fn the_add_project_dialog_makes_and_adds_a_project(cx: &mut TestAppContext) {
        let fixture = open(cx, "add-project-create");
        click(cx, &fixture, "repo-add");
        assert!(
            draw_until(cx, &fixture, |cx, fixture| found(
                cx,
                fixture,
                "add-project"
            )),
            "the dialog did not open"
        );
        // The dialog: browse, clone, create — and the two that need a
        // field reveal it rather than opening a second dialog.
        assert!(found(cx, &fixture, "add-project-browse"));
        assert!(!found(cx, &fixture, "add-project-cancel"));
        assert!(
            !found(cx, &fixture, "add-project-value"),
            "a field before a door"
        );

        click(cx, &fixture, "add-project-new");
        assert!(
            draw_until(cx, &fixture, |cx, fixture| found(
                cx,
                fixture,
                "add-project-value"
            )),
            "the create door did not open its fields"
        );
        let place = fixture.dir.join("projects");
        // The directory exists before the dialog's `git init` runs in it, with a file in it: what the
        // file tree shows afterwards is then checkable, and "init a directory that is not
        // empty" is the normal case (a folder somebody already has).
        let made = place.join("made-by-test");
        std::fs::create_dir_all(&made).unwrap();
        std::fs::write(made.join("README.md"), "made by the test\n").unwrap();
        type_into(cx, &fixture, "add-project-value", "made-by-test");
        assert!(!found(cx, &fixture, "add-project-hint"));
        assert_eq!(
            label(cx, &fixture, "add-project-destination"),
            Some(syro_core::home_dir().display().to_string())
        );
        pick_project_directory(cx, &fixture, None);
        assert_eq!(
            label(cx, &fixture, "add-project-destination"),
            Some(syro_core::home_dir().display().to_string())
        );
        pick_project_directory(cx, &fixture, Some(place.clone()));
        assert_eq!(
            label(cx, &fixture, "add-project-destination"),
            Some(place.display().to_string())
        );
        click(cx, &fixture, "add-project-clone");
        assert!(!found(cx, &fixture, "add-project-hint"));
        assert!(!found(cx, &fixture, "add-project-cancel"));
        assert_eq!(
            label(cx, &fixture, "add-project-destination"),
            Some(place.display().to_string())
        );
        pick_project_directory(cx, &fixture, None);
        assert_eq!(
            label(cx, &fixture, "add-project-destination"),
            Some(place.display().to_string())
        );
        click(cx, &fixture, "add-project-new");
        click(cx, &fixture, "add-project-confirm");

        // Canonical, because that is the spelling git and `repo_root` report
        // (macOS resolves `/var` to `/private/var`) and therefore the one the
        // state keeps.
        let made = place.join("made-by-test");
        let made = made.canonicalize().unwrap_or(made);
        assert!(
            draw_until(cx, &fixture, |cx, fixture| cx.update(|cx| fixture
                .state
                .read(cx)
                .repos()
                .iter()
                .any(|repo| repo.path == made))),
            "the project never reached the sidebar"
        );
        assert!(
            syro_core::git::is_git_repo(&made),
            "the project was not initialised"
        );
        // And everything that follows the active worktree followed it: the file tree shows the new
        // project's files rather than "No worktree", and the worktree has the terminal it opens
        // with.
        assert!(
            found(
                cx,
                &fixture,
                &format!("tree-{}", made.join("README.md").display())
            ),
            "the file tree did not follow the project that was just added"
        );
        assert!(
            !found(cx, &fixture, "tree-no-worktree"),
            "the file tree still says it has no worktree"
        );
        assert!(
            draw_until(cx, &fixture, |cx, fixture| cx.update(|cx| fixture
                .workspace
                .read(cx)
                .terminal_tab(cx)
                .is_some())),
            "adding a project did not start the worktree's terminal"
        );
        assert!(
            draw_until(cx, &fixture, |cx, fixture| !found(
                cx,
                fixture,
                "add-project"
            )),
            "the dialog stayed open after adding"
        );
    }

    #[gpui_kit::test]
    fn the_add_project_dialog_clones_into_the_selected_directory(cx: &mut TestAppContext) {
        let fixture = open(cx, "add-project-clone");
        click(cx, &fixture, "repo-add");
        click(cx, &fixture, "add-project-clone");
        let source = fixture.dir.join("main");
        type_into(cx, &fixture, "add-project-value", &source.to_string_lossy());
        let place = fixture.dir.join("clone projects");
        std::fs::create_dir_all(&place).unwrap();
        pick_project_directory(cx, &fixture, Some(place.clone()));
        click(cx, &fixture, "add-project-confirm");

        let cloned = place.join("main");
        assert!(
            draw_until(cx, &fixture, |cx, fixture| cx.update(|cx| fixture
                .state
                .read(cx)
                .repos()
                .iter()
                .any(
                    |repo| repo.path == cloned.canonicalize().unwrap_or(cloned.clone())
                ))),
            "the clone never reached the sidebar"
        );
        assert!(syro_core::git::is_git_repo(&cloned));
        assert!(
            draw_until(cx, &fixture, |cx, fixture| !found(
                cx,
                fixture,
                "add-project"
            )),
            "the dialog stayed open after cloning"
        );
    }

    #[gpui_kit::test]
    fn the_form_makes_a_worktree_and_the_row_arrives(cx: &mut TestAppContext) {
        let fixture = open(cx, "dialog-create");
        open_form(cx, &fixture);
        type_into(cx, &fixture, "new-worktree-name", "from the form");
        let expected = destination(&fixture, "from the form");
        assert!(
            click_until(cx, &fixture, "new-worktree-create", |cx, fixture| {
                row_is_listed(cx, fixture, &expected)
            }),
            "the created worktree never reached the sidebar at {}",
            expected.display()
        );
        assert!(expected.is_dir(), "git never made the directory");
        assert!(
            !found(cx, &fixture, "new-worktree-name"),
            "the form is still up after the worktree appeared"
        );
    }

    #[gpui_kit::test]
    fn a_name_that_is_not_a_name_keeps_the_form_open_and_creates_nothing(cx: &mut TestAppContext) {
        let fixture = open(cx, "dialog-badname");
        open_form(cx, &fixture);
        type_into(cx, &fixture, "new-worktree-name", "///");
        // Nothing is dispatched — the name is refused before it gets that far — but the refusal still
        // has to reach the screen, and a `notify` repaints on the *next* frame.
        assert!(
            click_until(cx, &fixture, "new-worktree-create", |cx, fixture| label(
                cx,
                fixture,
                "new-worktree-error"
            )
            .is_some()),
            "the form said nothing about the name it refused"
        );
        let problem = label(cx, &fixture, "new-worktree-error").expect("a message under the field");
        assert!(
            problem.contains("not a path"),
            "the message does not say what is wrong: {problem}"
        );
        assert!(
            found(cx, &fixture, "new-worktree-name"),
            "the form closed on a name it refused"
        );
        assert!(!fixture.dir.join("workspaces").exists());

        // And Cancel really closes it, rather than being a button that only looks like one.
        assert!(
            click_until(cx, &fixture, "new-worktree-cancel", |cx, fixture| !found(
                cx,
                fixture,
                "new-worktree-name"
            )),
            "Cancel left the form up"
        );
    }

    #[gpui_kit::test]
    fn a_base_ref_git_does_not_know_keeps_the_form_open_with_gits_words(cx: &mut TestAppContext) {
        let fixture = open(cx, "dialog-badref");
        open_form(cx, &fixture);
        // The branch line under the field is derived from the name, which makes
        // it the observable that says the name landed.
        type_until(
            cx,
            &fixture,
            "new-worktree-name",
            "bad-ref",
            |cx, fixture| {
                label(cx, fixture, "new-worktree-branch").as_deref() == Some("branch bad-ref")
            },
        );
        // The base ref lives under "Advanced", which is folded until it is
        // asked for: the form is short enough to read at a glance, and this is
        // the field a user visits least.
        assert!(
            click_until(cx, &fixture, "new-worktree-advanced", |cx, fixture| found(
                cx,
                fixture,
                "new-worktree-base"
            )),
            "the advanced fold did not open"
        );
        // The base field starts on the repo's default, so it is cleared first.
        type_into(cx, &fixture, "new-worktree-base", "");
        type_into(cx, &fixture, "new-worktree-base", "no-such-ref");
        assert!(
            click_until(cx, &fixture, "new-worktree-create", |cx, fixture| label(
                cx,
                fixture,
                "new-worktree-error"
            )
            .is_some()),
            "git refused and the form said nothing"
        );
        let refusal = label(cx, &fixture, "new-worktree-error").expect("the message");
        assert!(
            refusal.contains("no-such-ref"),
            "the form does not repeat what git said: {refusal}"
        );
        assert!(
            found(cx, &fixture, "new-worktree-name"),
            "the form closed on a failed create"
        );
        assert!(
            !destination(&fixture, "bad-ref").exists(),
            "a worktree was made from a ref git does not know"
        );
    }

    #[gpui_kit::test]
    fn a_refused_removal_offers_the_forced_one(cx: &mut TestAppContext) {
        let fixture = open(cx, "dialog-remove");
        let doomed = fixture.worktrees.first().expect("two worktrees").clone();
        let name = doomed
            .file_name()
            .expect("a name")
            .to_string_lossy()
            .into_owned();
        // Windows cannot remove a directory while this terminal has it as its
        // current directory. Confirming removal closes that terminal first.
        click(cx, &fixture, &format!("worktree-{}", doomed.display()));
        // A file git did not have: `worktree remove` refuses to throw that away.
        std::fs::write(doomed.join("scratch.txt"), "x").unwrap();

        right_click(cx, &fixture, format!("worktree-{}", doomed.display()));
        click(cx, &fixture, "worktree-menu-delete");
        assert!(
            found(cx, &fixture, "remove-worktree-remove"),
            "the confirmation did not open"
        );
        let asked = label(cx, &fixture, "remove-worktree-what").expect("what it asks");
        assert!(
            asked.contains(&name),
            "the confirmation does not name the worktree: {asked}"
        );

        assert!(
            click_until(cx, &fixture, "remove-worktree-remove", |cx, fixture| found(
                cx,
                fixture,
                "remove-worktree-force"
            )),
            "git refused and no forced retry was offered"
        );
        assert!(doomed.exists(), "the worktree went away without force");
        assert!(
            label(cx, &fixture, "remove-worktree-error").is_some(),
            "the refusal has no reason under it"
        );

        click(cx, &fixture, "remove-worktree-force");
        assert!(
            draw_until(cx, &fixture, |cx, fixture| !row_is_listed(
                cx, fixture, &doomed
            )),
            "the forced removal never took the row away"
        );
        assert!(!doomed.exists(), "the directory is still there");
    }

    /// The repo's own checkout is the one worktree git will not remove, so the
    /// confirmation for it asks the project question — the only thing that can
    /// be done about that row — rather than offering a retry that cannot work.
    #[gpui_kit::test]
    fn the_repos_own_checkout_is_removed_as_a_project(cx: &mut TestAppContext) {
        let fixture = open(cx, "dialog-own-checkout");
        // The path the row is keyed by: what git reports, which is the canonical
        // one — so this is not `fixture.dir.join("main")` on macOS.
        let own = cx.update(|cx| fixture.state.read(cx).repos()[0].path.clone());

        right_click(cx, &fixture, format!("worktree-{}", own.display()));
        click(cx, &fixture, "worktree-menu-delete");
        assert!(
            found(cx, &fixture, "remove-worktree-project"),
            "the own checkout offered no way out"
        );
        assert!(
            !found(cx, &fixture, "remove-worktree-remove")
                && !found(cx, &fixture, "remove-worktree-force"),
            "the own checkout was offered a removal git refuses"
        );

        click(cx, &fixture, "remove-worktree-project");
        assert!(
            draw_until(cx, &fixture, |cx, fixture| !row_is_listed(
                cx, fixture, &own
            )),
            "the project is still in the sidebar"
        );
        assert!(
            cx.update(|cx| fixture.state.read(cx).repos().is_empty()),
            "the repo is still in the state"
        );
        // The point of asking the project question instead: the files stay.
        assert!(
            own.is_dir(),
            "removing the project took the checkout with it"
        );
    }

    #[gpui_kit::test]
    fn activating_a_worktree_starts_a_terminal_in_it(cx: &mut TestAppContext) {
        let fixture = open(cx, "dialog-activate");
        let other = fixture.worktrees.last().expect("two worktrees").clone();

        click(cx, &fixture, &format!("worktree-{}", other.display()));
        let cwd = cx.update(|cx| {
            fixture
                .workspace
                .read(cx)
                .active_terminal(cx)
                .expect("the worktree's terminal")
                .read(cx)
                .to_spec(cx)
        });
        let syro_core::model::TabSpec::Terminal { cwd, .. } = cwd.expect("a terminal spec") else {
            panic!("a terminal tab described itself as something else")
        };
        assert_eq!(
            cwd, other,
            "the terminal is not in the worktree that was clicked"
        );
    }

    /// A preset whose child prints something and stops: what a finished agent
    /// looks like, and what leaves the tab with a restart to offer.
    fn quitting_agent() -> AgentSpec {
        AgentSpec {
            id: "quitter".to_string(),
            name: "Quitter".to_string(),
            command: "/bin/sh".to_string(),
            args: vec!["-c".to_string(), "echo run-$$; exit 0".to_string()],
            resume_args: Vec::new(),
            new_args: Vec::new(),
            env: Default::default(),
            status: Vec::new(),
        }
    }

    #[gpui_kit::test]
    fn a_restart_after_the_child_exited_runs_a_new_one(cx: &mut TestAppContext) {
        let settings = Settings {
            agents: vec![quitting_agent()],
            ..Default::default()
        };
        let fixture = open_with(cx, "dialog-restart", settings);
        open_form(cx, &fixture);
        type_into(cx, &fixture, "new-worktree-name", "restarted");
        // The picker, which is what puts the agent in the worktree instead of the plain shell the
        // workspace would have started.
        cx.update_window(fixture.window.into(), |_, window, cx| {
            window.scroll(
                "new-worktree-agents",
                gpui_kit::ScrollDelta::Lines(gpui_kit::point(0., -200.)),
                cx,
            );
        })
        .expect("scroll the agent list");
        draw(cx, &fixture);
        click_visible(cx, &fixture, "new-worktree-agent-quitter");
        assert!(
            click_until(cx, &fixture, "new-worktree-create", |cx, fixture| found(
                cx,
                fixture,
                "tab-exited-0"
            )),
            "the tab never said its child had exited"
        );
        let before = cx.update(|cx| {
            fixture
                .workspace
                .read(cx)
                .active_terminal(cx)
                .expect("the worktree's terminal")
        });

        click(cx, &fixture, "tab-restart-0");
        let restarted = draw_until(cx, &fixture, |cx, _| {
            cx.update(|cx| {
                fixture
                    .workspace
                    .read(cx)
                    .active_terminal(cx)
                    .is_some_and(|now| now.entity_id() != before.entity_id())
            })
        });
        assert!(restarted, "the restart never replaced the terminal");
        let after = cx.update(|cx| {
            fixture
                .workspace
                .read(cx)
                .active_terminal(cx)
                .expect("the new terminal")
        });
        // A grid that only the new child could have filled: the view was built
        // empty, one entity later.
        assert!(
            draw_until(cx, &fixture, |cx, _| cx.update(|cx| after
                .read(cx)
                .screen()
                .text()
                .contains("run-"))),
            "the restarted child never printed: {:?}",
            cx.update(|cx| after.read(cx).screen())
        );
    }
}

/// A project's shared paths: what every new worktree of it gets from the main
/// checkout (see `git::share_paths`). Each change is saved as it is made, like the
/// settings form: there is no half-edited list to lose.
pub struct SharedPaths {
    state: Entity<AppState>,
    /// See [`NewWorktree::window`].
    window: AnyWindowHandle,
    repo_id: RepoId,
    /// The main checkout's top level, `(name, is a directory)`: what the picker
    /// offers, read once when the form opens.
    entries: Vec<(String, bool)>,
    /// Whether the picker under "Add path" is open. Inline rather than a popover,
    /// for the reason [`NewWorktree::repos_open`] gives.
    picker_open: bool,
    query: Entity<InputState>,
    /// What was typed that is not a path inside the checkout.
    error: Option<String>,
    _query: Subscription,
}

/// The most entries the picker lists: a checkout's top level, not a file search.
const MAX_SUGGESTIONS: usize = 50;

/// What the picker shows for `query`: the entries whose name contains it, and
/// whether the query itself is offered as a path of its own — when it names
/// nothing listed and nothing already shared.
fn suggestions<'a>(
    entries: &'a [(String, bool)],
    query: &str,
    existing: &[String],
) -> (Vec<&'a (String, bool)>, Option<String>) {
    let query = query.trim().trim_start_matches('/');
    let needle = query.to_lowercase();
    let listed: Vec<_> = entries
        .iter()
        .filter(|(name, _)| name.to_lowercase().contains(&needle))
        .take(MAX_SUGGESTIONS)
        .collect();
    let literal = (!query.is_empty()
        && !listed.iter().any(|(name, _)| name == query)
        && !existing.iter().any(|path| path == query))
    .then(|| query.to_string());
    (listed, literal)
}

impl SharedPaths {
    pub fn new(
        state: Entity<AppState>,
        repo_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let root = state
            .read(cx)
            .repos()
            .iter()
            .find(|repo| repo.id == repo_id)
            .map(|repo| repo.path.clone());
        let mut entries: Vec<(String, bool)> = root
            .and_then(|root| std::fs::read_dir(root).ok())
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| {
                let is_dir = entry.file_type().is_ok_and(|kind| kind.is_dir());
                (entry.file_name().to_string_lossy().into_owned(), is_dir)
            })
            // git's own directory is the one thing a worktree must not share.
            .filter(|(name, _)| name != ".git")
            .collect();
        // Folders first, then by name: the order the file tree lists them in.
        entries.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

        let query = cx.new(|cx| {
            InputState::new(window, cx).placeholder(t!("shared_paths.placeholder").to_string())
        });
        let subscription = cx.subscribe(&query, |this, input, event: &InputEvent, cx| {
            // Enter adds what was typed, whether or not the list has it.
            if matches!(event, InputEvent::PressEnter { .. }) {
                let typed = input.read(cx).value().to_string();
                this.add(&typed, cx);
            }
            cx.notify();
        });
        Self {
            state,
            window: window.window_handle(),
            repo_id: repo_id.to_string(),
            entries,
            picker_open: false,
            query,
            error: None,
            _query: subscription,
        }
    }

    fn paths(&self, cx: &App) -> Vec<String> {
        self.state
            .read(cx)
            .repos()
            .iter()
            .find(|repo| repo.id == self.repo_id)
            .map(|repo| repo.shared_paths.clone())
            .unwrap_or_default()
    }

    fn save(&self, paths: Vec<String>, cx: &mut Context<Self>) {
        let repo_id = self.repo_id.clone();
        self.state
            .update(cx, |state, cx| state.set_shared_paths(&repo_id, paths, cx));
    }

    /// Open or close the picker; opened, the field has the keyboard.
    fn toggle_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.picker_open = !self.picker_open;
        self.error = None;
        if self.picker_open {
            self.query.update(cx, |input, cx| input.focus(window, cx));
        }
        cx.notify();
    }

    /// Share `path`, then close the picker: one path per pick, like Orca's.
    fn add(&mut self, path: &str, cx: &mut Context<Self>) {
        if path.trim().is_empty() {
            return;
        }
        let Some(path) = git::shared_path(path) else {
            self.error = Some(t!("shared_paths.invalid", path = path.trim()).to_string());
            cx.notify();
            return;
        };
        let mut paths = self.paths(cx);
        paths.push(path);
        self.save(paths, cx);
        self.error = None;
        self.picker_open = false;
        let query = self.query.clone();
        let window = self.window;
        cx.defer(move |cx| {
            let _ = window.update(cx, |_, window, cx| {
                query.update(cx, |input, cx| input.set_value("", window, cx));
            });
        });
        cx.notify();
    }

    fn remove(&mut self, path: &str, cx: &mut Context<Self>) {
        let mut paths = self.paths(cx);
        paths.retain(|p| p != path);
        self.save(paths, cx);
        cx.notify();
    }

    /// The picker: the field, the typed path as an entry of its own, and the
    /// checkout's top level filtered by it.
    fn picker(&self, paths: &[String], colors: &ColorTokens, cx: &mut Context<Self>) -> AnyElement {
        let query = self.query.read(cx).value().to_string();
        let (listed, literal) = suggestions(&self.entries, &query, paths);
        let nothing = listed.is_empty() && literal.is_none();

        let row = |id: SharedString| {
            div()
                .id(id)
                .test_support()
                .h_flex()
                .items_center()
                .gap_2()
                .px_2()
                .py_1()
                .rounded_sm()
                .text_xs()
        };
        let mut list = div()
            .id("shared-paths-suggestions")
            .test_support()
            .v_flex()
            .max_h(px(220.))
            .overflow_y_scroll()
            .gap_0p5();
        if let Some(literal) = literal {
            let add = literal.clone();
            list = list.child(
                row("shared-paths-literal".into())
                    .cursor_pointer()
                    .hover(|el| el.bg(colors.muted))
                    .child(
                        Icon::new(IconName::Plus)
                            .size(px(12.))
                            .text_color(colors.muted_foreground),
                    )
                    .child(t!("shared_paths.add_literal"))
                    .child(
                        div()
                            .px_1()
                            .rounded_sm()
                            .bg(colors.muted)
                            .font_family(Theme::global(cx).tokens.typography.mono.clone())
                            .child(SharedString::from(literal)),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| this.add(&add, cx))),
            );
        }
        for (name, is_dir) in listed {
            let added = paths.contains(name);
            let icon = if *is_dir {
                Icon::new(IconName::Folder)
            } else {
                crate::file_icons::file_icon(name)
            };
            let pick = name.clone();
            list = list.child(
                row(SharedString::from(format!("shared-paths-entry-{name}")))
                    .aria_label(SharedString::from(name.clone()))
                    .child(icon.size(px(12.)).text_color(colors.muted_foreground))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .child(SharedString::from(name.clone())),
                    )
                    .when(added, |el| {
                        el.opacity(0.5).child(
                            div()
                                .text_color(colors.muted_foreground)
                                .child(t!("shared_paths.added")),
                        )
                    })
                    .when(!added, |el| {
                        el.cursor_pointer()
                            .hover(|el| el.bg(colors.muted))
                            .on_click(cx.listener(move |this, _, _, cx| this.add(&pick, cx)))
                    }),
            );
        }
        if nothing {
            list = list.child(
                div()
                    .px_2()
                    .text_xs()
                    .text_color(colors.muted_foreground)
                    .child(t!("shared_paths.no_matches")),
            );
        }

        div()
            .id("shared-paths-picker")
            .test_support()
            .v_flex()
            .gap_1()
            .p_1()
            .rounded_md()
            .border_1()
            .border_color(colors.border)
            .child(
                div()
                    .id("shared-paths-input")
                    .test_support()
                    .child(Input::new(&self.query).focus_bordered(false).small()),
            )
            .child(list)
            .into_any_element()
    }
}

impl Render for SharedPaths {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = Theme::global(cx).tokens.colors;
        let paths = self.paths(cx);
        let mono = Theme::global(cx).tokens.typography.mono.clone();

        let mut chips = div().h_flex().flex_wrap().gap_1p5();
        for (index, path) in paths.iter().enumerate() {
            let remove = path.clone();
            chips = chips.child(
                div()
                    .id(SharedString::from(format!("shared-path-{index}")))
                    .test_support()
                    .aria_label(SharedString::from(path.clone()))
                    .h_flex()
                    .items_center()
                    .gap_1()
                    .pl_2()
                    .pr_1()
                    .py_0p5()
                    .rounded_md()
                    .border_1()
                    .border_color(colors.border)
                    .bg(colors.muted)
                    .text_xs()
                    .font_family(mono.clone())
                    .child(SharedString::from(path.clone()))
                    .child(
                        Button::new(SharedString::from(format!("shared-path-remove-{index}")))
                            .ghost()
                            .xsmall()
                            .icon(IconName::Close)
                            .tooltip(t!("shared_paths.remove"))
                            .on_click(cx.listener(move |this, _, _, cx| this.remove(&remove, cx))),
                    ),
            );
        }

        let list = if paths.is_empty() {
            div()
                .id("shared-paths-list")
                .test_support()
                .px_4()
                .py_6()
                .rounded_lg()
                .border_1()
                .border_dashed()
                .border_color(colors.border)
                .text_sm()
                .text_color(colors.muted_foreground)
                .child(t!("shared_paths.empty"))
        } else {
            div()
                .id("shared-paths-list")
                .test_support()
                .h_flex()
                .items_start()
                .gap_3()
                .px_4()
                .py_3()
                .rounded_lg()
                .border_1()
                .border_color(colors.border)
                .child(
                    div()
                        .flex_shrink_0()
                        .size(px(32.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded_md()
                        .border_1()
                        .border_color(colors.border)
                        .bg(colors.muted)
                        .child(
                            Icon::default()
                                .data(include_bytes!("../assets/icons/link-2.svg"))
                                .size(px(16.))
                                .text_color(colors.muted_foreground),
                        ),
                )
                .child(
                    div()
                        .v_flex()
                        .flex_1()
                        .min_w_0()
                        .gap_2()
                        .child(
                            div()
                                .h_flex()
                                .items_center()
                                .gap_2()
                                .child(div().text_sm().child(t!("shared_paths.linked")))
                                .child(
                                    div()
                                        .id("shared-paths-count")
                                        .test_support()
                                        .text_xs()
                                        .text_color(colors.muted_foreground)
                                        .child(t!("shared_paths.count", count = paths.len())),
                                ),
                        )
                        .child(chips),
                )
        };

        let window = self.window;
        div()
            .id("shared-paths")
            .test_support()
            .v_flex()
            .w_full()
            .gap_3()
            .text_sm()
            // The description, with "Add path" beside it: the button is the way
            // in, and the picker it opens is where a path is chosen.
            .child(
                div()
                    .h_flex()
                    .items_start()
                    .justify_between()
                    .gap_4()
                    .child(
                        // Shrinks and wraps: unwrapped, the line is wider than the
                        // dialog and pushes the button out of it.
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_xs()
                            .text_color(colors.muted_foreground)
                            .child(t!("shared_paths.description")),
                    )
                    .child(
                        div().flex_shrink_0().child(
                            Button::new("shared-paths-add")
                                .small()
                                .outline()
                                .icon(IconName::Plus)
                                .label(t!("shared_paths.add"))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.toggle_picker(window, cx)
                                })),
                        ),
                    ),
            )
            .when(self.picker_open, |el| {
                el.child(self.picker(&paths, &colors, cx))
            })
            .when_some(self.error.clone(), |el, error| {
                el.child(
                    div()
                        .id("shared-paths-error")
                        .test_support()
                        .role(Role::Alert)
                        .text_xs()
                        .text_color(colors.destructive)
                        .child(SharedString::from(error)),
                )
            })
            .child(list)
            .child(
                div().h_flex().justify_end().child(
                    Button::new("shared-paths-done")
                        .small()
                        .label(t!("common.done"))
                        .on_click(move |_, _, cx| close(window, cx)),
                ),
            )
    }
}

#[cfg(test)]
mod shared_paths_tests {
    use super::suggestions;

    #[test]
    fn the_picker_filters_the_checkout_and_offers_what_was_typed() {
        let entries = vec![
            ("data".to_string(), true),
            ("node_modules".to_string(), true),
            (".env".to_string(), false),
        ];
        let names = |(listed, _): &(Vec<&(String, bool)>, Option<String>)| {
            listed
                .iter()
                .map(|(name, _)| name.clone())
                .collect::<Vec<_>>()
        };

        // Nothing typed: the whole top level, and nothing of its own to add.
        let all = suggestions(&entries, "", &[]);
        assert_eq!(names(&all), ["data", "node_modules", ".env"]);
        assert_eq!(all.1, None);

        // Case-insensitive, and the typed text is offered when it names nothing listed.
        let some = suggestions(&entries, "MOD", &[]);
        assert_eq!(names(&some), ["node_modules"]);
        assert_eq!(some.1.as_deref(), Some("MOD"));

        // An exact match is the entry, not a second copy of it.
        assert_eq!(suggestions(&entries, ".env", &[]).1, None);
        // Nor is what is already shared offered again.
        assert_eq!(suggestions(&entries, "a/b", &["a/b".into()]).1, None);
        assert_eq!(
            suggestions(&entries, " /a/b ", &[]).1.as_deref(),
            Some("a/b")
        );
    }
}
