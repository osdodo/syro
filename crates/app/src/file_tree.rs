//! The file tree: the active worktree's files, one level at a time.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use gpui_kit::base::{ColorTokens, StyledExt as _, TestSupportExt as _, Theme};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{Icon, IconName};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rust_i18n::t;
use syro_core::event::CoreEvent;
use syro_core::fs_tree::{self, FileEntry};
use syro_core::model::AgentStatus;

use crate::file_icons;
use crate::state::{AppState, editor};
use crate::status_color;

/// The panel's header, and the pane-area strip the toggle floats in while the
/// panel is down. The two line up at 36 px on purpose: the icon would jump a few
/// pixels every time the panel opened otherwise.
const HEADER_HEIGHT: f32 = 36.;

/// What a tree row carries while it is being dragged: the file's own path, and nothing else.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DraggedPath(pub PathBuf);

/// The chip that follows the pointer while a row is dragged.
pub(crate) struct DragChip {
    pub(crate) name: String,
}

impl Render for DragChip {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = Theme::global(cx).tokens.colors.clone();
        div()
            .px_2()
            .py_1()
            .rounded_md()
            .border_1()
            .border_color(colors.border)
            .bg(colors.surface)
            .text_xs()
            .text_color(colors.foreground)
            .child(SharedString::from(self.name.clone()))
    }
}

/// One item of the row menu: the five things this panel can do plus the one the
/// desktop does.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum MenuItem {
    NewFile,
    NewDir,
    Rename,
    CopyName,
    Delete,
    Reveal,
}

impl MenuItem {
    const ALL: [MenuItem; 6] = [
        MenuItem::Reveal,
        MenuItem::NewFile,
        MenuItem::NewDir,
        MenuItem::Rename,
        MenuItem::CopyName,
        MenuItem::Delete,
    ];

    /// What the item says, and what a screen reader and a test read.
    fn label(self) -> String {
        match self {
            MenuItem::NewFile => t!("tree.new_file"),
            MenuItem::NewDir => t!("tree.new_folder"),
            MenuItem::Rename => t!("tree.rename"),
            MenuItem::CopyName => t!("tree.copy_name"),
            MenuItem::Delete => t!("tree.delete"),
            MenuItem::Reveal => t!("tree.reveal"),
        }
        .to_string()
    }

    /// Stable across renders, so the element a test clicks is the one drawn.
    fn id(self) -> &'static str {
        match self {
            MenuItem::NewFile => "tree-menu-new-file",
            MenuItem::NewDir => "tree-menu-new-folder",
            MenuItem::Rename => "tree-menu-rename",
            MenuItem::CopyName => "tree-menu-copy-name",
            MenuItem::Delete => "tree-menu-delete",
            MenuItem::Reveal => "tree-menu-reveal",
        }
    }
}

/// The row menu, open on one path and anchored where it was opened.
#[derive(Clone)]
struct Menu {
    path: PathBuf,
    is_dir: bool,
    /// Where the right-click was, in window coordinates. The menu is drawn in a
    /// layer above the panel (see `menu_overlay`), because the entries scroll
    /// and a menu drawn inside them would be clipped at the panel's edge.
    at: Point<Pixels>,
}

/// Which of the three things a typed name can be for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum EditKind {
    NewFile,
    NewDir,
    Rename,
}

impl EditKind {
    /// What the row says it is doing. Also what a test reads.
    fn label(self) -> String {
        match self {
            EditKind::NewFile => t!("tree.new_file_name"),
            EditKind::NewDir => t!("tree.new_folder_name"),
            EditKind::Rename => t!("tree.new_name"),
        }
        .to_string()
    }
}

/// A name being typed.
struct Edit {
    kind: EditKind,
    /// The directory a new entry goes in, or the path being renamed.
    target: PathBuf,
    input: Entity<InputState>,
}

/// Why a typed name cannot be used, if it cannot.
fn name_problem(name: &str) -> Option<String> {
    if name.is_empty() {
        return Some(t!("tree.need_name").to_string());
    }
    if name.contains('/') || name.contains('\\') {
        return Some(t!("tree.no_path_separator").to_string());
    }
    if name == "." || name == ".." {
        return Some(t!("tree.not_a_name", name = name).to_string());
    }
    None
}

/// One line of the tree, flattened for rendering. Built fresh each frame from
/// what is expanded, which is why nothing keeps a node tree.
struct Row {
    path: PathBuf,
    pub(crate) name: String,
    depth: usize,
    is_dir: bool,
    ignored: bool,
    expanded: bool,
    /// Git's status letter for this path, when git has one: the same list the
    /// diff tab shows, read from the state so both come from one `git status`.
    status: Option<char>,
}

pub struct FileTreeView {
    state: Entity<AppState>,
    root: Option<PathBuf>,
    /// Which directories are open. A `BTreeSet` because rendering walks it in
    /// path order and a directory can only be in it once.
    expanded: BTreeSet<PathBuf>,
    /// The entries of every directory that has been read. Only expanded ones
    /// are here, and a refresh throws the map away.
    children: BTreeMap<PathBuf, Vec<FileEntry>>,
    /// The keyboard's place inside the tree.
    focus: FocusHandle,
    /// The row menu, open on one path.
    menu: Option<Menu>,
    /// A name being typed, in the row it belongs to.
    editing: Option<Edit>,
    /// The path a delete is waiting to hear about.
    confirming_delete: Option<PathBuf>,
    /// What the last thing this tree did had to say — a refusal, or the note
    /// that a reload happened. A tree that swallows "already exists" looks
    /// broken rather than busy, so it is drawn until something replaces it.
    notice: Option<String>,
    /// The window's activation observer, registered on the first frame: the
    /// window is what the tree is refreshed by, and construction has no window.
    _watch: Option<Subscription>,
    /// Re-reads on a worktree switch, and on the active agent going idle: those
    /// are the moments the files under this panel are expected to have moved.
    _events: Subscription,
}

impl FileTreeView {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let events = cx.subscribe(&state, |this, _state, event: &CoreEvent, cx| {
            match event {
                CoreEvent::ActiveWorktreeChanged { .. } => this.follow_active_worktree(cx),
                // An agent that has just gone idle is an agent whose turn ended, and a turn that ended is when
                // files appeared on disk.
                CoreEvent::AgentStatusChanged {
                    worktree_id,
                    status: Some(AgentStatus::Idle),
                    ..
                } => {
                    if this.state.read(cx).active_id() == Some(worktree_id) {
                        this.refresh(cx);
                    }
                }
                _ => {}
            }
        });
        let mut tree = Self {
            state,
            root: None,
            expanded: BTreeSet::new(),
            children: BTreeMap::new(),
            focus: cx.focus_handle(),
            menu: None,
            editing: None,
            confirming_delete: None,
            notice: None,
            _watch: None,
            _events: events,
        };
        tree.follow_active_worktree(cx);
        tree
    }

    /// Re-read every directory that is open.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        let open: Vec<PathBuf> = self.expanded.iter().cloned().collect();
        self.children.clear();
        for dir in open {
            if !dir.is_dir() {
                self.expanded.remove(&dir);
                continue;
            }
            let entries = fs_tree::read_dir_ignored(&dir).unwrap_or_default();
            self.children.insert(dir, entries);
        }
        cx.notify();
    }

    /// Say something to the reader, in the line under the header. Cleared by the
    /// next thing that has nothing to say.
    fn set_notice(&mut self, notice: Option<String>, cx: &mut Context<Self>) {
        if self.notice != notice {
            self.notice = notice;
            cx.notify();
        }
    }

    /// Open the menu for `path`, at the point it was right-clicked.
    fn open_menu(
        &mut self,
        path: PathBuf,
        is_dir: bool,
        at: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.menu = Some(Menu { path, is_dir, at });
        self.editing = None;
        self.confirming_delete = None;
        // The keyboard goes to the panel, so Escape reaches the handler that
        // closes this — a menu is a thing you can back out of.
        window.focus(&self.focus, cx);
        cx.notify();
    }

    /// Start typing a name: a new entry in `dir`, or the new name for `path`.
    fn start_edit(
        &mut self,
        kind: EditKind,
        target: PathBuf,
        existing: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A directory that is closed cannot show the row it is about to create.
        if kind != EditKind::Rename && target.is_dir() {
            self.expand(target.clone());
        }
        self.menu = None;
        self.confirming_delete = None;
        let input =
            cx.new(|cx| InputState::new(window, cx).default_value(existing.unwrap_or_default()));
        // Enter finishes the name — the field's own business, not this panel's:
        // the keys that type into it belong to the input, and the only thing the
        // panel still wants from the keyboard is Escape (below).
        cx.subscribe(&input, |this, _input, event: &InputEvent, cx| {
            if matches!(event, InputEvent::PressEnter { .. }) {
                this.commit_edit(cx);
            }
            cx.notify();
        })
        .detach();
        // The whole name is selected: renaming is nearly always a replacement,
        // and a field that opens with the old name ready to be typed over is the
        // one every file manager shows.
        input.update(cx, |input, cx| {
            input.focus(window, cx);
            input.select_all(window, cx);
        });
        self.editing = Some(Edit {
            kind,
            target,
            input,
        });
        self.set_notice(None, cx);
        cx.notify();
    }

    /// Finish the name that is being typed: make the file, make the directory, or
    /// rename. A refusal stays on screen; a success re-reads the directory it
    /// happened in.
    fn commit_edit(&mut self, cx: &mut Context<Self>) {
        let Some(edit) = self.editing.take() else {
            return;
        };
        let value = edit.input.read(cx).value().to_string();
        if let Some(problem) = name_problem(&value) {
            self.editing = Some(edit);
            self.set_notice(Some(problem), cx);
            return;
        }
        let outcome = match edit.kind {
            EditKind::NewFile => fs_tree::create_file(&edit.target.join(&value)),
            EditKind::NewDir => fs_tree::create_dir(&edit.target.join(&value)),
            EditKind::Rename => {
                let to = edit.target.with_file_name(&value);
                fs_tree::rename(&edit.target, &to)
            }
        };
        match outcome {
            Ok(()) => {
                self.set_notice(None, cx);
                self.refresh(cx);
            }
            // The filesystem's own words: "already exists" and "permission
            // denied" are the two a reader actually hits, and both are clearer
            // than anything this panel would paraphrase them into.
            Err(error) => {
                self.editing = Some(edit);
                self.set_notice(Some(format!("{error:#}")), cx);
            }
        }
    }

    /// What is left of the keyboard for this panel: Escape.
    fn handle_edit_key(
        &mut self,
        event: &KeyDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.keystroke.key != "escape" {
            return;
        }
        // Escape backs out of whatever the panel has open: a name being typed is dropped, and the menu
        // or the delete question is closed.
        if self.editing.is_some() {
            self.cancel_edit(cx);
        } else {
            self.cancel_overlays(cx);
        }
        cx.stop_propagation();
    }

    /// Ask before removing anything.
    fn ask_delete(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.menu = None;
        self.editing = None;
        self.confirming_delete = Some(path);
        cx.notify();
    }

    /// Do it. `fs_tree::remove` takes a directory's contents with it, which is
    /// what the confirmation says in words before this runs.
    fn confirm_delete(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.confirming_delete.take() else {
            return;
        };
        match fs_tree::remove(&path) {
            Ok(()) => {
                self.set_notice(None, cx);
                self.refresh(cx);
            }
            Err(error) => self.set_notice(Some(format!("{error:#}")), cx),
        }
    }

    /// Close the menu and the confirmation. A name being typed is left alone:
    fn cancel_overlays(&mut self, cx: &mut Context<Self>) {
        let had = self.menu.take().is_some() | self.confirming_delete.take().is_some();
        if had {
            cx.notify();
        }
    }

    /// Drop a name being typed without writing it: the mouse's Escape.
    fn cancel_edit(&mut self, cx: &mut Context<Self>) {
        if self.editing.take().is_some() {
            self.set_notice(None, cx);
            cx.notify();
        }
    }

    /// Point the tree at the active worktree's directory, forgetting what was read for the last
    /// one: those entries are rows for a repository nobody is looking at.
    fn follow_active_worktree(&mut self, cx: &mut Context<Self>) {
        let root = self
            .state
            .read(cx)
            .active()
            .map(|worktree| worktree.path.clone());
        let root = root.filter(|path| path.is_dir());
        if self.root == root {
            return;
        }
        self.children.clear();
        self.expanded.clear();
        self.root = root;
        // The overlays belong to the rows that were on screen: the menu's path, the name being typed
        // and the delete waiting to be confirmed are all paths inside the worktree that just
        // went away.
        self.menu = None;
        self.editing = None;
        self.confirming_delete = None;
        self.notice = None;
        if let Some(root) = self.root.clone() {
            self.expand(root);
        }
        cx.notify();
    }

    /// Read one directory into the cache. Called on expand and on expand only:
    /// `render` reads the map, it does not fill it.
    fn expand(&mut self, dir: PathBuf) {
        if self.children.contains_key(&dir) {
            return;
        }
        // A directory that cannot be read is an empty directory here, and an
        // empty cache entry keeps it from being re-read every frame.
        let entries = fs_tree::read_dir_ignored(&dir).unwrap_or_default();
        self.children.insert(dir.clone(), entries);
        self.expanded.insert(dir);
    }

    fn toggle(&mut self, dir: PathBuf, cx: &mut Context<Self>) {
        if self.expanded.remove(&dir) {
            // Closing drops the entries as well as the flag, so opening it
            // again is a re-read: that is how a file added on disk while the
            // directory was closed gets to show up.
            self.children.remove(&dir);
        } else {
            self.expand(dir);
        }
        cx.notify();
    }

    /// Flatten the tree the way a reader sees it: the root's entries, then the contents of every
    /// directory under it that is open, in order.
    fn rows(&self, cx: &App) -> Vec<Row> {
        let mut rows = Vec::new();
        let Some(root) = &self.root else {
            return rows;
        };
        let worktree_id = self.state.read(cx).active_id().cloned();
        let status_of = |path: &Path| -> Option<char> {
            let worktree_id = worktree_id.as_ref()?;
            let relative = path.strip_prefix(root).ok()?;
            self.state.read(cx).change_status(worktree_id, relative)
        };
        self.push_rows(root, 0, &status_of, &mut rows);
        rows
    }

    fn push_rows(
        &self,
        dir: &Path,
        depth: usize,
        status_of: &dyn Fn(&Path) -> Option<char>,
        rows: &mut Vec<Row>,
    ) {
        let Some(entries) = self.children.get(dir) else {
            return;
        };
        for entry in entries {
            let expanded = self.expanded.contains(&entry.path);
            rows.push(Row {
                path: entry.path.clone(),
                name: entry.name.clone(),
                depth,
                is_dir: entry.is_dir,
                ignored: entry.ignored,
                expanded,
                // Only files carry a status: a directory's own path is not what
                // git reports for a change inside it.
                status: (!entry.is_dir).then(|| status_of(&entry.path)).flatten(),
            });
            if entry.is_dir && expanded {
                self.push_rows(&entry.path, depth + 1, status_of, rows);
            }
        }
    }

    fn row(&self, row: Row, colors: &ColorTokens, cx: &mut Context<Self>) -> impl IntoElement {
        let path = row.path.clone();
        // Three listeners want the path and each is a `move` closure, so each
        // gets its own clone rather than the last one silently winning.
        let path_for_click = path.clone();
        let path_for_menu = path.clone();
        let path_for_drag = path.clone();
        let is_dir = row.is_dir;
        // The row the menu is open on stays marked while it is up: a right-click
        // says *which* file the menu is about, and a menu floating over an
        // unmarked row leaves the reader guessing which one the Delete would take.
        let selected = self.menu.as_ref().is_some_and(|menu| menu.path == path);
        let focus = self.focus.clone();
        // The row: the twisty, then what the entry *is*, then the name. Both
        // glyphs are muted so the name is what the eye lands on — the icons are
        // there to be read at a glance, not to be looked at.
        let muted = colors.muted_foreground;
        let twisty = if is_dir {
            Icon::new(if row.expanded {
                IconName::ChevronDown
            } else {
                IconName::ChevronRight
            })
            .size(px(12.))
            .text_color(muted)
            .into_any_element()
        } else {
            // A file is not a thing that opens, so it has no twisty: the same
            // 12 px of nothing keeps its name lined up with a directory's.
            div().size(px(12.)).flex_shrink_0().into_any_element()
        };
        let icon = if is_dir {
            Icon::new(if row.expanded {
                IconName::FolderOpen
            } else {
                IconName::Folder
            })
            .size(px(12.))
            .text_color(muted)
            .into_any_element()
        } else {
            file_icons::file_icon(&row.name)
                .size(px(12.))
                .text_color(muted)
                .into_any_element()
        };
        // What a screen reader reads and what a test asserts: the name, the
        // ignore note, and the status letter as it is drawn beside it.
        let label = format!(
            "{}{}{}",
            row.name,
            if row.ignored {
                t!("tree.ignored")
            } else {
                "".into()
            },
            match row.status {
                Some(status) => format!(" {status}"),
                None => String::new(),
            },
        );
        let status_color = row.status.map(|status| status_color(status, &colors, cx));
        let mut el = div().id(SharedString::from(format!("tree-{}", row.path.display())));
        if is_dir {
            // A file is not a thing that opens and closes; a directory is, and
            // a screen reader has to be told which it is.
            el = el.aria_expanded(row.expanded);
        }
        let mut el = el
            .test_support()
            // Stated, not only dimmed: a colour is not something a screen
            // reader can read out, and it is not something a test can assert.
            .aria_label(SharedString::from(label))
            .track_focus(&self.focus)
            .h_flex()
            .gap_1()
            .w_full()
            // Depth is the whole of the indentation: no connector lines, which
            // would each be an element for a drawing nobody reads.
            .pl(px(8. + row.depth as f32 * 12.))
            .pr_2()
            .py_1()
            .rounded_md()
            .cursor_pointer()
            .hover(|el| el.bg(colors.muted))
            .when(selected, |el| el.bg(colors.accent))
            .text_xs()
            // Git-ignored entries stay readable but visibly not ours, which is
            // what makes `node_modules` stop looking like source.
            .text_color(if row.ignored {
                colors.muted_foreground
            } else {
                colors.foreground
            })
            // Git-ignored entries lean as well as fade: "not ours", without
            // taking the name away.
            .when(row.ignored, |el| el.italic())
            .child(twisty)
            .child(icon)
            .child(SharedString::from(row.name.clone()));
        // The letter, coloured by what happened to the file — the file tree's
        // half of the diff tab's status, from the same list.
        if let (Some(status), Some(color)) = (row.status, status_color) {
            el = el.child(
                div()
                    .flex_shrink_0()
                    .text_color(color)
                    .child(SharedString::from(status.to_string())),
            );
        }
        el.on_click(cx.listener(move |this, _event, window, cx| {
            // A click anywhere in the panel means the menu is no longer the
            // thing being looked at.
            this.cancel_overlays(cx);
            // Before the action, so the dispatch has a path that includes
            // the workspace (see the field's note).
            window.focus(&focus, cx);
            if is_dir {
                this.toggle(path_for_click.clone(), cx);
            } else {
                window.dispatch_action(
                    Box::new(editor::Open {
                        path: path_for_click.clone(),
                    }),
                    cx,
                );
            }
        }))
        // The row's menu. `on_aux_click` rather than a left-click listener: the
        // right button is what opens it everywhere else, and the left button is
        // already taken by what the row does.
        .on_aux_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
            let at = match event {
                ClickEvent::Mouse(mouse) => mouse.up.position,
                ClickEvent::Touch(touch) => touch.position,
                // A keyboard-generated click has no place to open a menu at, and
                // opening one in a corner would be worse than not opening it.
                ClickEvent::Keyboard(_) => return,
            };
            this.open_menu(path_for_menu.clone(), is_dir, at, window, cx);
        }))
        // A file row is a drag source. A directory is not: what a drop of a directory would mean is a
        // question nobody has asked, and the drop target would have to answer it.
        .when(!is_dir, |el| {
            el.on_drag(
                DraggedPath(path_for_drag),
                |dragged, _offset, _window, cx| {
                    let name = dragged
                        .0
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_else(|| dragged.0.display().to_string());
                    cx.new(|_| DragChip { name })
                },
            )
        })
    }

    /// The row a name is being typed in.
    fn edit_row(
        &self,
        kind: EditKind,
        target: &Path,
        input: &Entity<InputState>,
        depth: usize,
        colors: &ColorTokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let value = input.read(cx).value().to_string();
        // The icon column, so the name being typed sits where the name it is replacing sits: the
        // twisty's own 12 px (a name has nothing to twist), then what the entry is.
        let is_dir = match kind {
            EditKind::NewDir => true,
            EditKind::NewFile => false,
            EditKind::Rename => target.is_dir(),
        };
        div()
            .id("tree-edit")
            .test_support()
            // What is being typed, as one line: the field draws the text itself,
            // and this is what a screen reader and a test read back.
            .aria_label(SharedString::from(format!("{}: {value}", kind.label())))
            // A click in the field is a click on the field, not on whatever is
            // behind it: without this, the panel's own click-away would drop the
            // name the moment the caret was placed.
            .on_mouse_down(MouseButton::Left, |_event, _window, cx| {
                cx.stop_propagation()
            })
            .h_flex()
            .items_center()
            .w_full()
            .pl(px(8. + depth as f32 * 12.))
            .pr_2()
            .py_0p5()
            .rounded_md()
            .bg(colors.muted)
            .border_1()
            .border_color(colors.ring)
            .text_xs()
            .text_color(colors.foreground)
            .child(div().size(px(12.)).flex_shrink_0())
            .child(
                Icon::new(if is_dir {
                    IconName::Folder
                } else {
                    IconName::File
                })
                .size(px(12.))
                .text_color(colors.muted_foreground),
            )
            // The component draws its own caret and selection; what it does not
            // need here is the box it draws inside a form.
            .child(
                Input::new(input)
                    .id("tree-edit-input")
                    .appearance(false)
                    .bordered(false)
                    .cleanable(false)
                    .h(px(20.))
                    // What is left after the two columns above, rather than the
                    // whole row: a field asking for all of it would push them
                    // off the edge.
                    .flex_1()
                    .min_w_0(),
            )
            .into_any_element()
    }

    /// The line under the header: what the last thing this panel did had to say.
    fn notice_line(&self, colors: &ColorTokens) -> AnyElement {
        div()
            .id("tree-notice")
            .test_support()
            .aria_label(SharedString::from(self.notice.clone().unwrap_or_default()))
            .px_2()
            .pb_1()
            .text_xs()
            .text_color(colors.muted_foreground)
            .child(SharedString::from(self.notice.clone().unwrap_or_default()))
            .into_any_element()
    }

    /// The button that re-reads every open directory by hand.
    fn refresh_button(&self, colors: &ColorTokens, cx: &mut Context<Self>) -> AnyElement {
        div()
            .id("tree-refresh")
            .test_support()
            .aria_label(t!("tree.refresh"))
            .h_flex()
            .items_center()
            .justify_center()
            .size(px(22.))
            .rounded_sm()
            .cursor_pointer()
            .text_color(colors.muted_foreground)
            .hover(|el| el.bg(colors.muted))
            .child(Icon::new(IconName::RefreshCw).size(px(14.)))
            .on_click(cx.listener(|this, _event, _window, cx| {
                this.set_notice(None, cx);
                this.refresh(cx);
            }))
            .into_any_element()
    }

    /// One item of the row menu.
    fn menu_item(
        &self,
        item: MenuItem,
        colors: &ColorTokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let this = cx.entity().downgrade();
        div()
            .id(SharedString::from(item.id()))
            .test_support()
            .aria_label(item.label())
            .px_2()
            .py_1()
            .rounded_sm()
            .text_xs()
            .text_color(if item == MenuItem::Delete {
                colors.destructive
            } else {
                colors.foreground
            })
            .cursor_pointer()
            .hover(|el| el.bg(colors.muted))
            .child(item.label())
            .on_click(move |_event, window, cx| {
                this.update(cx, |tree, cx| tree.run_menu_item(item, window, cx))
                    .ok();
            })
            .into_any_element()
    }

    /// Do what the item says to the path the menu is open on.
    fn run_menu_item(&mut self, item: MenuItem, window: &mut Window, cx: &mut Context<Self>) {
        let Some(menu) = self.menu.clone() else {
            return;
        };
        // A new entry belongs in a directory: the row's own directory when the row is a file, and the
        // row itself when it is a directory — which is what every file explorer does, and what
        // makes "new file" on a folder row mean "in here".
        let dir = if menu.is_dir {
            menu.path.clone()
        } else {
            menu.path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| menu.path.clone())
        };
        match item {
            MenuItem::NewFile => self.start_edit(EditKind::NewFile, dir, None, window, cx),
            MenuItem::NewDir => self.start_edit(EditKind::NewDir, dir, None, window, cx),
            MenuItem::Rename => {
                let existing = menu
                    .path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned());
                self.start_edit(EditKind::Rename, menu.path.clone(), existing, window, cx)
            }
            MenuItem::CopyName => {
                self.menu = None;
                if let Some(name) = menu.path.file_name() {
                    cx.write_to_clipboard(ClipboardItem::new_string(
                        name.to_string_lossy().into_owned(),
                    ));
                }
            }
            MenuItem::Delete => self.ask_delete(menu.path.clone(), cx),
            MenuItem::Reveal => {
                self.menu = None;
                crate::opener::reveal(&menu.path);
            }
        }
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
        let mut panel = div()
            .id("tree-menu")
            .test_support()
            .v_flex()
            .w(px(180.))
            .p_1()
            .gap_0p5()
            .rounded_md()
            .border_1()
            .border_color(colors.border)
            // Solid at any window opacity, like every menu: a list of entries
            // over the tree is not the place to read the wallpaper through.
            .bg(crate::settings_view::solid(colors.surface))
            // The whole panel is the menu, padding and the few pixels between two items included: a click
            // there is a click on the menu, not on the row it happens to be drawn over.
            .occlude()
            // The menu is above the tree in a layer, so a wheel event would go
            // to the entries underneath it.
            .on_scroll_wheel(|_event, _window, cx| cx.stop_propagation());
        for item in MenuItem::ALL {
            panel = panel.child(self.menu_item(item, colors, cx));
        }

        deferred(
            // The scrim: a click anywhere outside the menu closes it, on either button — which is what
            // makes the menu feel like a menu.
            anchored().position(point(px(0.), px(0.))).child(
                div()
                    .id("tree-menu-scrim")
                    .test_support()
                    .w(window_size.width)
                    .h(window_size.height)
                    // Blocks the mouse from the tree behind it: a row under
                    // the menu is not hovered while the menu is up, and a
                    // click on one dismisses the menu instead of acting.
                    .occlude()
                    .on_mouse_down(MouseButton::Left, move |_event, _window, cx| {
                        this.update(cx, |tree, cx| tree.cancel_overlays(cx)).ok();
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

    /// The confirmation a delete waits behind, drawn in the panel's own corner.
    fn confirm_panel(
        &self,
        path: &Path,
        colors: &ColorTokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let is_dir = path.is_dir();
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        let question = if is_dir {
            t!("tree.delete_dir", name = name).to_string()
        } else {
            t!("tree.delete_file", name = name).to_string()
        };
        div()
            .id("tree-confirm")
            .test_support()
            .aria_label(SharedString::from(question.clone()))
            .absolute()
            .top(px(HEADER_HEIGHT + 8.))
            .left(px(8.))
            .right(px(8.))
            .v_flex()
            .gap_2()
            .p_2()
            .rounded_md()
            .border_1()
            .border_color(colors.border)
            // Solid at any window opacity, like the menu it answers: a question
            // over the tree is not the place to read the wallpaper through.
            .bg(crate::settings_view::solid(colors.surface))
            .text_xs()
            .text_color(colors.foreground)
            .child(SharedString::from(question))
            .child(
                div()
                    .h_flex()
                    .gap_2()
                    .child(self.confirm_button(
                        "tree-delete-confirm",
                        &t!("tree.delete"),
                        true,
                        colors,
                        cx,
                    ))
                    .child(self.confirm_button(
                        "tree-delete-cancel",
                        &t!("common.cancel"),
                        false,
                        colors,
                        cx,
                    )),
            )
            .into_any_element()
    }

    fn confirm_button(
        &self,
        id: &'static str,
        label: &str,
        destructive: bool,
        colors: &ColorTokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let label = SharedString::from(label);
        div()
            .id(id)
            .test_support()
            .aria_label(label.clone())
            .px_2()
            .py_0p5()
            .rounded_sm()
            .border_1()
            .border_color(colors.border)
            .cursor_pointer()
            .text_color(if destructive {
                colors.destructive
            } else {
                colors.foreground
            })
            .hover(|el| el.bg(colors.muted))
            .child(label)
            .on_click(cx.listener(move |this, _event, _window, cx| {
                if destructive {
                    this.confirm_delete(cx);
                } else {
                    this.cancel_overlays(cx);
                }
            }))
            .into_any_element()
    }
}

impl Render for FileTreeView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The window is what this panel is refreshed by, and construction has no window — so the
        // observer is registered on the first frame and kept.
        if self._watch.is_none() {
            self._watch = Some(cx.observe_window_activation(window, |this, window, cx| {
                if window.is_window_active() {
                    this.refresh(cx);
                }
            }));
        }

        let colors = Theme::global(cx).tokens.colors.clone();
        let header = div()
            .id("tree-header")
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
                    .child(t!("tree.files")),
            )
            .child(
                div()
                    .h_flex()
                    .items_center()
                    .gap_1()
                    .child(self.refresh_button(&colors, cx)),
            );

        // The header holds still; only the entries scroll.
        let mut body = div()
            .id("tree-body")
            .test_support()
            .v_flex()
            .flex_1()
            .min_h_0()
            .w_full()
            // The same gutter the sidebar's cards float in, so the two panels
            // of the window have one set of edges between them.
            .px_1()
            .pb_2()
            .overflow_y_scroll()
            // Blank space under the rows is the mouse's Escape: a click there
            // drops a name being typed, which is what every file manager does.
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _event, _window, cx| this.cancel_edit(cx)),
            );

        if self.root.is_none() {
            body = body.child(
                div()
                    .id("tree-no-worktree")
                    .test_support()
                    // The panel saying it has nothing to show, with an id: a worktree arriving is supposed to take
                    // this line away, and "the tree is empty" is otherwise indistinguishable
                    // from "the tree is showing an empty directory".
                    .aria_label(t!("tree.no_worktree"))
                    .px_2()
                    .text_xs()
                    .text_color(colors.muted_foreground)
                    .child(t!("tree.no_worktree")),
            );
        } else {
            // What is being typed, if anything, and the row it belongs to: the
            // edit is drawn in place, so the loop has to know both.
            let editing = self
                .editing
                .as_ref()
                .map(|edit| (edit.kind, edit.target.clone(), edit.input.clone()));
            // A new entry in the worktree's own root has no row to hang under —
            // the root is not a row — so it goes at the top of the list, which is
            // where the directory it belongs to is.
            let starts_at_root = editing.as_ref().is_some_and(|(kind, target, _)| {
                *kind != EditKind::Rename && self.root.as_deref() == Some(target.as_path())
            });
            if starts_at_root {
                if let Some((kind, target, input)) = &editing {
                    body = body.child(self.edit_row(*kind, target, input, 0, &colors, cx));
                }
            }
            for row in self.rows(cx) {
                if let Some((kind, target, input)) = &editing {
                    if *kind == EditKind::Rename && *target == row.path {
                        body =
                            body.child(self.edit_row(*kind, target, input, row.depth, &colors, cx));
                        continue;
                    }
                }
                let new_entry_here = editing.as_ref().is_some_and(|(kind, target, _)| {
                    *kind != EditKind::Rename && *target == row.path
                });
                let depth = row.depth;
                body = body.child(self.row(row, &colors, cx));
                // A new entry goes under the directory it is going into, which
                // is where the reader is looking when they typed the name.
                if new_entry_here {
                    if let Some((kind, target, input)) = &editing {
                        body =
                            body.child(self.edit_row(*kind, target, input, depth + 1, &colors, cx));
                    }
                }
            }
        }

        let notice = self.notice.clone();
        let menu = self.menu.clone();
        let confirming = self.confirming_delete.clone();
        // The panel is a view of its own inside the right sidebar, which owns
        // the width, the border and the resize handle (`crate::right_sidebar`).
        let mut root = div()
            .id("file-tree")
            .test_support()
            .relative()
            .v_flex()
            .w_full()
            .flex_1()
            .min_h_0()
            // Escape backs out of whatever is open.
            .on_key_down(cx.listener(Self::handle_edit_key))
            .child(header);
        if notice.is_some() {
            root = root.child(self.notice_line(&colors));
        }
        root = root.child(body);
        if let Some(path) = confirming {
            root = root.child(self.confirm_panel(&path, &colors, cx));
        }
        if let Some(menu) = &menu {
            root = root.child(self.menu_overlay(menu, window.bounds().size, &colors, cx));
        }
        root
    }
}

#[cfg(test)]
mod tests {
    // Not `use super::*`: the module above globs gpui in, and gpui's own `test`
    // attribute comes with it — under `cfg(test)` it shadows the built-in one,
    // and every `#[test]` below then expands into itself.
    use std::cell::RefCell;
    use std::path::{Path, PathBuf};
    use std::rc::Rc;

    use gpui_kit::base::{Root, StyledExt as _, TestSupportExt as _, Theme};
    use gpui_kit::test::TestWindowExt as _;
    // The element traits by name rather than by glob: a glob of gpui brings its
    // own `test` attribute, which shadows the built-in one under `cfg(test)`.
    use gpui_kit::{
        AppContext as _, Context, Entity, InputEvent as _, InteractiveElement as _, IntoElement,
        ParentElement as _, Render, Styled as _, TestAppContext, Window, WindowHandle, div, px,
    };
    use syro_core::config::Settings;
    use syro_core::model::State;
    use syro_core::store::Store;

    use super::{DraggedPath, FileTreeView, MenuItem, name_problem};
    use crate::pane::editor::tests::open;
    use crate::state::AppState;
    use crate::state::fixture::{scratch_repo, temp_store};

    /// A window holding the tree, pointed at a scratch repository's own checkout.
    struct Fixture {
        window: WindowHandle<Root>,
        /// The active worktree's directory, canonical — which is the path git
        /// reports and therefore the one every row id is built from.
        root: PathBuf,
    }

    /// The scratch repository and the state file both fixtures below need.
    fn scratch(tag: &str) -> (PathBuf, PathBuf) {
        scratch_seeded(tag, |_| {})
    }

    /// The same scratch repository, over a `state.json` this test wrote: what a
    /// restart finds is the file, so a test about a restart has to write one.
    fn scratch_seeded(tag: &str, state: impl FnOnce(&mut State)) -> (PathBuf, PathBuf) {
        let (dir, repo, _worktrees) = scratch_repo(tag);
        let root = dir.join("main");
        // A directory to expand and a file to rename, so the tests are about
        // rows that exist rather than about the fixture.
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src").join("lib.rs"), "pub fn x() {}\n").unwrap();
        std::fs::write(root.join("README.md"), "scratch\n").unwrap();

        let (state_path, _store_dir) = temp_store(tag);
        let mut saved = State {
            repos: vec![repo],
            active_worktree: Some(
                root.canonicalize()
                    .unwrap_or_else(|_| root.clone())
                    .display()
                    .to_string(),
            ),
            ..Default::default()
        };
        state(&mut saved);
        Store::new(state_path.clone()).save(&saved).unwrap();
        (dir, state_path)
    }

    /// The tree as the right sidebar mounts it: filling a window's whole height.
    /// A bare root is only as tall as its rows, and then the blank space below
    /// them — the space this panel is clicked on — is not in the window at all.
    struct Fill {
        tree: Entity<FileTreeView>,
    }

    impl Render for Fill {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div().size_full().v_flex().child(self.tree.clone())
        }
    }

    fn open_tree(cx: &mut TestAppContext, tag: &str) -> Fixture {
        let (dir, state_path) = scratch_seeded(tag, |_| {});
        let root = dir.join("main");
        let state = cx.update(|cx| {
            cx.new(|cx| {
                AppState::open(Store::new(state_path.clone()), dir.join("workspaces"), cx)
                    .with_settings(Settings::default())
            })
        });
        let (window, _tree) = open(cx, move |_window, cx| {
            let tree = cx.new(|cx| FileTreeView::new(state, cx));
            cx.new(|_| Fill { tree })
        });
        Fixture {
            window,
            root: root.canonicalize().unwrap_or(root),
        }
    }

    fn draw(cx: &mut TestAppContext, fixture: &Fixture) {
        cx.update_window(fixture.window.into(), |_, window, cx| {
            window.draw(cx).clear(cx);
        })
        .expect("draw a frame");
    }

    /// A row's element id, from the path it stands for.
    fn row_id(path: &Path) -> String {
        format!("tree-{}", path.display())
    }

    fn found(cx: &mut TestAppContext, fixture: &Fixture, id: impl Into<String>) -> bool {
        let id = id.into();
        cx.update_window(fixture.window.into(), move |_, window, _cx| {
            window.try_find(id).is_some()
        })
        .unwrap_or(false)
    }

    fn label(cx: &mut TestAppContext, fixture: &Fixture, id: impl Into<String>) -> Option<String> {
        let id = id.into();
        cx.update_window(fixture.window.into(), move |_, window, _cx| {
            window
                .try_find(id)
                .and_then(|found| found.label().map(str::to_string))
        })
        .unwrap_or_default()
    }

    fn click(cx: &mut TestAppContext, fixture: &Fixture, id: String) {
        cx.update_window(fixture.window.into(), |_, window, cx| {
            window.click(id, cx);
        })
        .expect("click");
    }

    fn right_click(cx: &mut TestAppContext, fixture: &Fixture, id: String) {
        cx.update_window(fixture.window.into(), |_, window, cx| {
            window.right_click(id, cx);
        })
        .expect("right click");
    }

    fn press(cx: &mut TestAppContext, fixture: &Fixture, key: &str) {
        cx.update_window(fixture.window.into(), |_, window, cx| {
            window.press(key, cx);
        })
        .expect("press");
    }

    fn type_text(cx: &mut TestAppContext, fixture: &Fixture, text: &str) {
        cx.update_window(fixture.window.into(), |_, window, cx| {
            window.input(text, cx);
        })
        .expect("type");
    }

    /// Open a directory's row, so its children are rows too.
    fn expand(cx: &mut TestAppContext, fixture: &Fixture, name: &str) {
        click(cx, fixture, row_id(&fixture.root.join(name)));
        draw(cx, fixture);
    }

    /// Open the row menu on `path` and click one of its items.
    fn menu(cx: &mut TestAppContext, fixture: &Fixture, path: &Path, item: MenuItem) {
        let row = row_id(path);
        right_click(cx, fixture, row);
        draw(cx, fixture);
        assert!(
            found(cx, fixture, item.id()),
            "the menu did not open on {}",
            path.display()
        );
        click(cx, fixture, item.id().to_string());
        draw(cx, fixture);
    }

    /// Type `name` into the field the menu opened and press Enter.
    fn name_it(cx: &mut TestAppContext, fixture: &Fixture, name: &str) {
        assert!(found(cx, fixture, "tree-edit"), "no name field was drawn");
        type_text(cx, fixture, name);
        draw(cx, fixture);
        press(cx, fixture, "enter");
        draw(cx, fixture);
    }

    #[test]
    fn a_name_is_a_path_segment_and_nothing_else() {
        assert_eq!(name_problem("notes.md"), None);
        assert!(name_problem("").is_some());
        assert!(name_problem("a/b.md").is_some());
        assert!(name_problem("a\\b.md").is_some());
        assert!(name_problem(".").is_some());
        assert!(name_problem("..").is_some());
        // The sharpest of them: a name that walks out of the directory is the
        // one thing this must never allow, whatever the filesystem would do.
        assert!(name_problem("../outside.md").is_some());
    }

    #[gpui_kit::test]
    fn a_new_file_is_created_and_shows_up(cx: &mut TestAppContext) {
        let fixture = open_tree(cx, "tree-new-file");
        draw(cx, &fixture);
        // The menu on a file means "new entry in the directory this file is in",
        // which is where every file explorer puts it.
        menu(
            cx,
            &fixture,
            &fixture.root.join("README.md"),
            MenuItem::NewFile,
        );
        name_it(cx, &fixture, "notes.md");

        assert!(
            fixture.root.join("notes.md").is_file(),
            "the file was never created"
        );
        assert!(
            found(cx, &fixture, &row_id(&fixture.root.join("notes.md"))),
            "the new file is not in the tree"
        );
        // And nothing is left saying otherwise.
        assert!(!found(cx, &fixture, "tree-notice"));
    }

    #[gpui_kit::test]
    fn a_new_folder_is_created(cx: &mut TestAppContext) {
        let fixture = open_tree(cx, "tree-new-dir");
        draw(cx, &fixture);
        menu(
            cx,
            &fixture,
            &fixture.root.join("README.md"),
            MenuItem::NewDir,
        );
        name_it(cx, &fixture, "docs");
        assert!(
            fixture.root.join("docs").is_dir(),
            "the directory was never created"
        );
    }

    #[gpui_kit::test]
    fn a_name_that_already_exists_is_refused(cx: &mut TestAppContext) {
        let fixture = open_tree(cx, "tree-refuse");
        draw(cx, &fixture);
        let existing = fixture.root.join("README.md");
        let before = std::fs::read(&existing).unwrap();

        menu(cx, &fixture, &existing, MenuItem::NewFile);
        name_it(cx, &fixture, "README.md");

        // The refusal is on screen and the file is what it was: a "new file"
        // that quietly replaced one is the worst thing this could do.
        let notice = label(cx, &fixture, "tree-notice");
        assert!(
            notice
                .as_deref()
                .is_some_and(|text| text.contains("already exists")),
            "no refusal was shown: {notice:?}"
        );
        assert_eq!(std::fs::read(&existing).unwrap(), before);
        // And the field stays open, so the name can be corrected instead of
        // typed again from the menu.
        assert!(
            found(cx, &fixture, "tree-edit"),
            "the field the name was typed in did not stay open"
        );
    }

    #[gpui_kit::test]
    fn a_rename_moves_the_file_and_the_row_follows(cx: &mut TestAppContext) {
        let fixture = open_tree(cx, "tree-rename");
        draw(cx, &fixture);
        expand(cx, &fixture, "src");
        let from = fixture.root.join("src").join("lib.rs");
        let to = fixture.root.join("src").join("main.rs");

        menu(cx, &fixture, &from, MenuItem::Rename);
        // The field opens holding the current name, selected: what is typed
        // replaces it, which is what a rename field does everywhere.
        name_it(cx, &fixture, "main.rs");

        assert!(!from.exists(), "the old name is still there");
        assert!(to.is_file(), "the file was never renamed");
        assert!(found(cx, &fixture, &row_id(&to)), "the row did not follow");
        assert!(!found(cx, &fixture, &row_id(&from)));
    }

    #[gpui_kit::test]
    fn a_rename_onto_a_name_that_exists_is_refused(cx: &mut TestAppContext) {
        let fixture = open_tree(cx, "tree-rename-refuse");
        draw(cx, &fixture);
        expand(cx, &fixture, "src");
        let from = fixture.root.join("src").join("lib.rs");

        menu(cx, &fixture, &from, MenuItem::Rename);
        // A name with a separator in it: refused before the filesystem is asked,
        // because the filesystem would happily write outside `src`.
        name_it(cx, &fixture, "../elsewhere.rs");

        assert!(from.is_file(), "the file went somewhere");
        assert!(
            !fixture.root.join("elsewhere.rs").exists(),
            "a name escaped its directory"
        );
        assert!(
            label(cx, &fixture, "tree-notice").is_some(),
            "no refusal was shown"
        );
    }

    #[gpui_kit::test]
    fn copying_a_name_puts_it_on_the_clipboard(cx: &mut TestAppContext) {
        let fixture = open_tree(cx, "tree-copy-name");
        draw(cx, &fixture);
        let file = fixture.root.join("README.md");

        menu(cx, &fixture, &file, MenuItem::CopyName);

        let copied = cx.update(|cx| cx.read_from_clipboard().and_then(|item| item.text()));
        assert_eq!(copied.as_deref(), Some("README.md"));
        assert!(!found(cx, &fixture, "tree-menu"), "the menu stayed open");
    }

    #[gpui_kit::test]
    fn deleting_a_file_asks_first_and_then_removes_it(cx: &mut TestAppContext) {
        let fixture = open_tree(cx, "tree-delete-file");
        draw(cx, &fixture);
        expand(cx, &fixture, "src");
        let doomed = fixture.root.join("src").join("lib.rs");

        menu(cx, &fixture, &doomed, MenuItem::Delete);
        // Nothing happens until the confirmation is answered.
        assert!(
            doomed.is_file(),
            "the delete happened before it was confirmed"
        );
        let question = label(cx, &fixture, "tree-confirm");
        assert!(
            question
                .as_deref()
                .is_some_and(|text| text.contains("lib.rs")),
            "the confirmation does not name what it removes: {question:?}"
        );

        click(cx, &fixture, "tree-delete-confirm".to_string());
        draw(cx, &fixture);
        assert!(!doomed.exists(), "the file survived the confirmation");
        assert!(!found(cx, &fixture, &row_id(&doomed)));
    }

    /// The sequence a real window sends: press, a frame, release.
    #[gpui_kit::test]
    fn a_menu_items_click_survives_the_frame_between_down_and_up(cx: &mut TestAppContext) {
        let fixture = open_tree(cx, "tree-menu-frame");
        draw(cx, &fixture);
        let doomed = fixture.root.join("README.md");
        right_click(cx, &fixture, row_id(&doomed));
        draw(cx, &fixture);
        assert!(found(cx, &fixture, MenuItem::Delete.id()));

        // Where the item is, from the frame that drew it.
        let at = cx
            .update_window(fixture.window.into(), |_, window, _| {
                window
                    .try_find(MenuItem::Delete.id())
                    .map(|element| element.bounds().center())
            })
            .expect("read the frame")
            .expect("the item is drawn");
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
            // The frame between the two halves of a click.
            window.render_frame(cx);
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
        .expect("click the item");

        // The delete question is up, which is what the item meant — and the row
        // it was drawn over was not what got clicked.
        assert!(
            found(cx, &fixture, "tree-confirm"),
            "the menu item's click was lost to the row under it"
        );
        assert!(doomed.is_file(), "the file went without a confirmation");
    }

    /// A click at a window position, through the platform's own events: press and
    /// release with the frame a real window draws between them.
    fn click_at_point(
        cx: &mut TestAppContext,
        fixture: &Fixture,
        at: gpui_kit::Point<gpui_kit::Pixels>,
    ) {
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
            for event in [true, false] {
                if event {
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
                } else {
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
                }
                window.render_frame(cx);
            }
        })
        .expect("click at a point");
    }

    /// The menu's own padding and the pixels between two items belong to the menu:
    #[gpui_kit::test]
    fn a_click_on_the_menus_own_padding_does_not_reach_the_tree(cx: &mut TestAppContext) {
        let fixture = open_tree(cx, "tree-menu-padding");
        draw(cx, &fixture);
        right_click(cx, &fixture, row_id(&fixture.root.join("README.md")));
        draw(cx, &fixture);
        let menu = cx
            .update_window(fixture.window.into(), |_, window, _| {
                window.try_find("tree-menu").map(|element| element.bounds())
            })
            .expect("read the frame")
            .expect("the menu is drawn");
        assert!(menu.size.width > px(0.));

        click_at_point(cx, &fixture, menu.origin + gpui_kit::point(px(1.), px(1.)));

        assert!(
            found(cx, &fixture, "tree-menu"),
            "a click on the menu's own padding closed it and went to the row under it"
        );
    }

    /// A click anywhere else closes the menu: the scrim is what makes a menu a menu.
    #[gpui_kit::test]
    fn a_click_outside_the_menu_closes_it(cx: &mut TestAppContext) {
        let fixture = open_tree(cx, "tree-menu-scrim");
        draw(cx, &fixture);
        right_click(cx, &fixture, row_id(&fixture.root.join("README.md")));
        draw(cx, &fixture);
        assert!(found(cx, &fixture, "tree-menu"));

        // Below the last row, where no row can hear the click.
        click_at_point(cx, &fixture, gpui_kit::point(px(20.), px(450.)));

        assert!(
            !found(cx, &fixture, "tree-menu"),
            "a click outside the menu did not close it"
        );
    }

    /// A click in the panel's blank space is the mouse's Escape for a name
    /// being typed: the field goes away and nothing is written.
    #[gpui_kit::test]
    fn a_click_on_the_blank_space_drops_the_name_being_typed(cx: &mut TestAppContext) {
        let fixture = open_tree(cx, "tree-edit-blank");
        draw(cx, &fixture);
        menu(
            cx,
            &fixture,
            &fixture.root.join("README.md"),
            MenuItem::NewFile,
        );
        assert!(
            found(cx, &fixture, "tree-edit"),
            "the name field did not open"
        );

        // Below the last row, where no row can hear the click.
        click_at_point(cx, &fixture, gpui_kit::point(px(20.), px(450.)));
        draw(cx, &fixture);

        assert!(
            !found(cx, &fixture, "tree-edit"),
            "a click on the blank space left the name field open"
        );
        assert!(!fixture.root.join("untitled").exists());
    }

    /// The field itself is not blank space: a click inside it keeps the name
    /// being typed, so the caret can be placed without losing what was typed.
    #[gpui_kit::test]
    fn a_click_in_the_name_field_keeps_it_open(cx: &mut TestAppContext) {
        let fixture = open_tree(cx, "tree-edit-click");
        draw(cx, &fixture);
        menu(
            cx,
            &fixture,
            &fixture.root.join("README.md"),
            MenuItem::NewFile,
        );
        let at = cx
            .update_window(fixture.window.into(), |_, window, _| {
                window
                    .try_find("tree-edit-input")
                    .map(|element| element.bounds().center())
            })
            .expect("read the frame")
            .expect("the field is drawn");

        click_at_point(cx, &fixture, at);
        draw(cx, &fixture);

        assert!(
            found(cx, &fixture, "tree-edit"),
            "a click in the field dropped the name being typed"
        );
    }

    #[gpui_kit::test]
    fn escape_closes_the_row_menu(cx: &mut TestAppContext) {
        let fixture = open_tree(cx, "tree-menu-escape");
        draw(cx, &fixture);
        right_click(cx, &fixture, row_id(&fixture.root.join("README.md")));
        draw(cx, &fixture);
        assert!(
            found(cx, &fixture, MenuItem::NewFile.id()),
            "the menu did not open"
        );

        press(cx, &fixture, "escape");
        draw(cx, &fixture);
        assert!(
            !found(cx, &fixture, "tree-menu"),
            "escape did not close the menu"
        );

        // And the same key backs out of the delete question, which is the other
        // thing the menu opens.
        menu(
            cx,
            &fixture,
            &fixture.root.join("README.md"),
            MenuItem::Delete,
        );
        assert!(found(cx, &fixture, "tree-confirm"));
        press(cx, &fixture, "escape");
        draw(cx, &fixture);
        assert!(
            !found(cx, &fixture, "tree-confirm"),
            "escape did not back out of the delete"
        );

        // And out of a name being typed, which is the third thing Escape is for.
        menu(
            cx,
            &fixture,
            &fixture.root.join("README.md"),
            MenuItem::Rename,
        );
        assert!(found(cx, &fixture, "tree-edit"));
        press(cx, &fixture, "escape");
        draw(cx, &fixture);
        assert!(
            !found(cx, &fixture, "tree-edit"),
            "escape did not back out of the rename"
        );
    }

    #[gpui_kit::test]
    fn deleting_a_directory_says_its_contents_go_too(cx: &mut TestAppContext) {
        let fixture = open_tree(cx, "tree-delete-dir");
        draw(cx, &fixture);
        let doomed = fixture.root.join("src");

        menu(cx, &fixture, &doomed, MenuItem::Delete);
        let question = label(cx, &fixture, "tree-confirm").unwrap_or_default();
        assert!(
            question.contains("everything in it"),
            "a directory delete has to say what it takes: {question:?}"
        );

        click(cx, &fixture, "tree-delete-cancel".to_string());
        draw(cx, &fixture);
        assert!(doomed.is_dir(), "cancelling still deleted it");
        assert!(!found(cx, &fixture, "tree-confirm"));
    }

    #[gpui_kit::test]
    fn a_delete_that_fails_is_a_message(cx: &mut TestAppContext) {
        let fixture = open_tree(cx, "tree-delete-refused");
        draw(cx, &fixture);
        expand(cx, &fixture, "src");
        let doomed = fixture.root.join("src").join("lib.rs");

        menu(cx, &fixture, &doomed, MenuItem::Delete);
        // Gone before the answer, which is the case this has to survive: the
        // file was deleted outside the app while the question was open.
        std::fs::remove_file(&doomed).unwrap();
        click(cx, &fixture, "tree-delete-confirm".to_string());
        draw(cx, &fixture);

        let notice = label(cx, &fixture, "tree-notice");
        assert!(
            notice.as_deref().is_some_and(|text| !text.is_empty()),
            "a failed delete said nothing: {notice:?}"
        );
    }

    #[gpui_kit::test]
    fn a_refresh_keeps_what_is_open_and_finds_what_is_new(cx: &mut TestAppContext) {
        let fixture = open_tree(cx, "tree-refresh");
        draw(cx, &fixture);
        let src = fixture.root.join("src");
        let lib = src.join("lib.rs");

        // Open the directory, so the refresh has a place to lose.
        click(cx, &fixture, row_id(&src));
        draw(cx, &fixture);
        assert!(
            found(cx, &fixture, &row_id(&lib)),
            "the directory did not expand"
        );

        // Something appears on disk — an agent wrote a file, which is the whole
        // reason the panel re-reads at all.
        std::fs::write(src.join("fresh.rs"), "// fresh\n").unwrap();
        click(cx, &fixture, "tree-refresh".to_string());
        draw(cx, &fixture);

        assert!(
            found(cx, &fixture, &row_id(&src.join("fresh.rs"))),
            "the refresh missed a new file"
        );
        assert!(
            found(cx, &fixture, &row_id(&lib)),
            "the refresh closed the directory it was told to keep open"
        );
    }

    #[gpui_kit::test]
    fn the_window_regaining_focus_refreshes(cx: &mut TestAppContext) {
        let fixture = open_tree(cx, "tree-focus-refresh");
        draw(cx, &fixture);
        let appeared = fixture.root.join("appeared.md");

        std::fs::write(&appeared, "written while the window was away\n").unwrap();
        assert!(
            !found(cx, &fixture, &row_id(&appeared)),
            "the file was there before the window went away"
        );

        // Away and back, the way the platform reports it: the observer fires on
        // both edges, so becoming active is the moment that re-reads.
        let mut visual = gpui_kit::VisualTestContext::from_window(fixture.window.into(), cx);
        visual.deactivate_window();
        // The platform reports the change from a task, so the executor has to
        // run before the window believes it is away.
        cx.run_until_parked();
        draw(cx, &fixture);
        cx.update_window(fixture.window.into(), |_, window, _cx| {
            window.activate_window();
        })
        .expect("activate the window");
        cx.run_until_parked();
        draw(cx, &fixture);

        assert!(
            found(cx, &fixture, &row_id(&appeared)),
            "coming back to the window did not re-read the directory"
        );
    }

    /// A view that does nothing but record what is dropped on it, so the tree's
    /// half of the drag can be asserted without a terminal in the window.
    struct Catcher {
        got: Rc<RefCell<Vec<PathBuf>>>,
    }

    impl Render for Catcher {
        fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let got = self.got.clone();
            let colors = Theme::global(cx).tokens.colors.clone();
            div()
                .id("catcher")
                .test_support()
                .size(px(120.))
                .bg(colors.surface)
                .on_drop(move |dragged: &DraggedPath, _window, _cx| {
                    got.borrow_mut().push(dragged.0.clone());
                })
        }
    }

    /// Two panes side by side, the way the workspace arranges a tree and a
    /// terminal. A test-only view: the real one is `Workspace`, which is not
    /// this module's to build.
    struct Pair {
        tree: Entity<FileTreeView>,
        catcher: Entity<Catcher>,
    }

    impl Render for Pair {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .h_flex()
                .child(self.tree.clone())
                .child(self.catcher.clone())
        }
    }

    #[gpui_kit::test]
    fn dragging_a_file_row_carries_its_path(cx: &mut TestAppContext) {
        let (dir, state_path) = scratch("tree-drag");
        let root = dir.join("main");
        let file = root.join("my notes.md");
        std::fs::write(&file, "notes\n").unwrap();

        let state = cx.update(|cx| {
            cx.new(|cx| {
                AppState::open(Store::new(state_path.clone()), dir.join("workspaces"), cx)
                    .with_settings(Settings::default())
            })
        });
        let got = Rc::new(RefCell::new(Vec::new()));
        let catcher_got = got.clone();
        let (window, _pair) = open(cx, move |_window, cx| {
            let tree = cx.new(|cx| FileTreeView::new(state.clone(), cx));
            let catcher = cx.new(|_| Catcher { got: catcher_got });
            cx.new(|_| Pair { tree, catcher })
        });
        let fixture = Fixture {
            window,
            root: root.canonicalize().unwrap_or(root),
        };
        draw(cx, &fixture);

        let from = row_id(&fixture.root.join("my notes.md"));
        assert!(found(cx, &fixture, &from), "the row is not in the tree");
        cx.update_window(fixture.window.into(), |_, window, cx| {
            window.drag_to(from, "catcher", cx);
        })
        .expect("drag");

        assert_eq!(
            got.borrow().as_slice(),
            [fixture.root.join("my notes.md")],
            "the row did not carry its own path"
        );
    }
}
