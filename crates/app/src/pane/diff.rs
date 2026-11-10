//! The diff tab: one changed file's unified diff.

use std::ops::Range;
use std::path::PathBuf;

use gpui_kit::base::{TestSupportExt as _, Theme};
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::input::{
    Editor, EditorState, RangeDecoration, RangeDecorationCollection, RangeDecorationStyle,
};
use gpui_kit::*;
use rust_i18n::t;
use syro_core::git::{ChangeKind, FileChange};
use syro_core::model::{TabSpec, Worktree, WorktreeId};

use crate::opener;
use crate::pane::PaneItem;
use crate::pane::editor::editor_state;
use crate::state::AppState;

/// How strong an added/removed line's fill is. A diff is read as text, and a
/// fill that competes with the text is a fill that hides it.
const FILL_ALPHA: f32 = 0.16;

/// The diff tab's contents.
pub struct DiffView {
    /// Which worktree's changes these are.
    worktree_id: WorktreeId,
    /// The worktree's directory: what the relative paths git reports are
    /// relative *to*, and what a jump to the editor has to make them absolute
    /// with.
    root: PathBuf,
    /// What the changes are relative to — `Worktree::base_ref`, which is the
    /// base the worktree was forked from.
    base_ref: String,
    /// Which file's diff is on screen, by the path git reported. The path and
    /// not an index: a refresh reorders the list, and a selection that followed
    /// the index would silently become another file.
    selected: Option<String>,
    selected_kind: Option<ChangeKind>,
    /// The commit whose file is on screen, when what is shown is one commit's
    /// change rather than the worktree's. Two comparisons come through this one
    /// tab, and this is the field that says which of them is up.
    commit: Option<String>,
    app: Entity<AppState>,
    editor: Entity<EditorState>,
    /// The added/removed fills, addressed by handle. Held, because dropping the
    /// collection would not clear it.
    fills: RangeDecorationCollection,
    /// Git's complaint, or why there is nothing on the right.
    error: Option<String>,
}

impl DiffView {
    /// Open the worktree's changes. The file list comes from the state, so the
    /// caller is expected to have refreshed it (`AppState::refresh_changes`) —
    /// the workspace does that when a worktree becomes active.
    pub fn new(
        window: &mut Window,
        cx: &mut Context<Self>,
        app: Entity<AppState>,
        worktree: &Worktree,
    ) -> Self {
        let editor = editor_state(window, cx, Some(opener::DIFF_LANGUAGE));
        let fills = editor.update(cx, |editor, cx| {
            editor.create_range_decorations_collection(Vec::new(), cx)
        });
        let mut view = Self {
            worktree_id: worktree.id.clone(),
            root: worktree.path.clone(),
            base_ref: worktree.base_ref.clone(),
            selected: None,
            selected_kind: None,
            commit: None,
            app,
            editor,
            fills,
            error: None,
        };
        view.reload(window, cx);
        view
    }

    /// Re-read the file list from the state and re-load the selected file.
    pub fn reload(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // A commit's diff is not in the worktree's list and must not be re-picked
        // from it: showing the pane again reloads what is already on screen.
        if self.commit.is_some() {
            self.load_selected(window, cx);
            cx.notify();
            return;
        }
        let (paths, error) = {
            let app = self.app.read(cx);
            (
                app.changes(&self.worktree_id)
                    .iter()
                    .map(|change| (change.path.clone(), change.kind))
                    .collect::<Vec<_>>(),
                app.changes_error(&self.worktree_id).map(str::to_string),
            )
        };
        // The list's own complaint outlives a file read: it is about the list,
        // and a file read that succeeds does not answer it.
        self.error = error;
        // A file that is no longer changed is no longer the selection; the first
        // that is left is where a reader starts.
        if self
            .selected
            .as_ref()
            .zip(self.selected_kind)
            .is_some_and(|(selected, kind)| !paths.contains(&(selected.clone(), kind)))
        {
            self.selected = None;
            self.selected_kind = None;
        }
        if self.selected.is_none()
            && let Some((path, kind)) = paths.into_iter().next()
        {
            self.selected = Some(path);
            self.selected_kind = Some(kind);
        }
        self.load_selected(window, cx);
        cx.notify();
    }

    /// Show one file's diff. What the source control panel's rows dispatch, and
    /// the reason this view has no list of its own.
    pub fn show(
        &mut self,
        path: &str,
        kind: ChangeKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.selected.as_deref() == Some(path)
            && self.selected_kind == Some(kind)
            && self.commit.is_none()
        {
            return;
        }
        self.commit = None;
        self.selected = Some(path.to_string());
        self.selected_kind = Some(kind);
        self.load_selected(window, cx);
        cx.notify();
    }

    /// Show one file as one commit changed it: the other comparison this tab
    /// makes. The file is not in the worktree's list, so the selection is
    /// whatever the caller named.
    pub fn show_commit(
        &mut self,
        commit: &str,
        path: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.commit.as_deref() == Some(commit) && self.selected.as_deref() == Some(path) {
            return;
        }
        self.commit = Some(commit.to_string());
        self.selected_kind = None;
        self.selected = Some(path.to_string());
        self.load_selected(window, cx);
        cx.notify();
    }

    /// Back to the worktree's changes, which is what `diff::Open` means and the
    /// way out of a commit's file.
    pub fn show_worktree(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.commit.is_none() {
            return;
        }
        self.commit = None;
        self.selected = None;
        self.selected_kind = None;
        self.reload(window, cx);
    }

    /// The change on screen, looked up in the state's list rather than kept
    /// here: one list, one place it can be stale.
    fn change(&self, cx: &App) -> Option<FileChange> {
        let selected = self.selected.as_deref()?;
        self.app
            .read(cx)
            .changes(&self.worktree_id)
            .iter()
            .find(|change| change.path == selected && Some(change.kind) == self.selected_kind)
            .cloned()
    }

    /// The selected file's path, relative to the worktree.
    #[allow(
        dead_code,
        reason = "the view's read surface: what the window tests assert on"
    )]
    pub fn selected(&self) -> Option<&str> {
        self.selected.as_deref()
    }

    pub fn shows_commit(&self, commit: &str, path: &str) -> bool {
        self.commit.as_deref() == Some(commit) && self.selected.as_deref() == Some(path)
    }

    /// The diff text the right-hand side is showing.
    #[allow(
        dead_code,
        reason = "the view's read surface: what the window tests assert on"
    )]
    pub fn text(&self, cx: &App) -> String {
        self.editor.read(cx).value().to_string()
    }

    /// The language the diff body is parsed as, as the state reports it. A diff
    /// is not a file with an extension, so this is the one language in the app
    /// that does not come from `opener::language_for` — and this is what says so.
    #[allow(
        dead_code,
        reason = "the view's read surface: what the window tests assert on"
    )]
    pub fn language(&self, cx: &App) -> SharedString {
        self.editor.read(cx).language_name()
    }

    fn load_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = self.selected.clone() else {
            self.set_text(String::new(), window, cx);
            return;
        };
        let text = match &self.commit {
            // A commit's file: what that commit did to it, not what the worktree
            // has done since.
            Some(commit) => match syro_core::git::commit_file_diff(&self.root, commit, &path) {
                Ok(text) => text,
                Err(error) => {
                    self.error = Some(format!("{error:#}"));
                    self.set_text(String::new(), window, cx);
                    return;
                }
            },
            None => {
                let Some(change) = self.change(cx) else {
                    self.set_text(String::new(), window, cx);
                    return;
                };
                match syro_core::git::diff_text(&self.root, &self.base_ref, &change) {
                    Ok(text) => text,
                    Err(error) => {
                        self.error = Some(format!("{error:#}"));
                        self.set_text(String::new(), window, cx);
                        return;
                    }
                }
            }
        };
        // The same ceiling the editor's files go through, for the same reason: a diff of a huge
        // file is twice the huge file, and it is the editor that would be holding it.
        if text.len() as u64 > opener::MAX_TEXT_BYTES {
            self.error = Some(
                opener::Refusal::TooLarge {
                    bytes: text.len() as u64,
                }
                .message(&self.root.join(&path)),
            );
            self.set_text(String::new(), window, cx);
            return;
        }
        self.error = None;
        self.set_text(text, window, cx);
    }

    /// Replace the text and its fills together. The fills are byte ranges into
    /// that text, so setting one without the other is a decoration over the
    /// wrong line.
    fn set_text(&mut self, text: String, window: &mut Window, cx: &mut Context<Self>) {
        let fills = changed_lines(&text);
        let (added, removed) = {
            let theme = cx.theme();
            (
                theme.success.alpha(FILL_ALPHA),
                theme.danger.alpha(FILL_ALPHA),
            )
        };
        // The text first: `set_value` replaces the rope the tracked ranges were
        // measured against, so ranges set before it would be clipped to nothing.
        self.editor
            .update(cx, |editor, cx| editor.set_value(text, window, cx));
        let decorations = fills
            .into_iter()
            .map(|(kind, range)| {
                let color = match kind {
                    Changed::Added => added,
                    Changed::Removed => removed,
                };
                RangeDecoration::new(range)
                    .with_style(RangeDecorationStyle::Fill)
                    .with_color(color)
            })
            .collect();
        self.fills.set(decorations, cx);
    }
}

impl Focusable for DiffView {
    /// The editor's own focus handle: the state is what takes the keyboard, so
    /// focusing the tab has to mean focusing what is inside it.
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.read(cx).focus_handle(cx)
    }
}

impl PaneItem for DiffView {
    /// The file it is showing, like the editor tab: a tab bar with two tabs
    /// called "Diff" says nothing about which is which.
    fn title(&self, _cx: &App) -> SharedString {
        self.selected
            .as_deref()
            .and_then(|path| std::path::Path::new(path).file_name())
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| t!("pane.diff").to_string())
            .into()
    }

    fn to_spec(&self, _cx: &App) -> Option<TabSpec> {
        Some(TabSpec::Diff {
            worktree_id: self.worktree_id.clone(),
        })
    }
}

impl Render for DiffView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = Theme::global(cx).tokens.colors;
        let mut body = div()
            .id("diff-pane")
            .test_support()
            .size_full()
            .min_h_0()
            // The empty state and a git failure are drawn over the pane, so the
            // pane is what they are positioned against.
            .relative()
            .child(
                div().id("diff-text").size_full().child(
                    Editor::new(&self.editor)
                        .bordered(false)
                        .readonly(true)
                        .h(relative(1.))
                        .into_any_element(),
                ),
            );

        if let Some(error) = self.error.clone() {
            body = body.child(
                div()
                    .id("diff-error")
                    .test_support()
                    .role(Role::Alert)
                    .aria_label(SharedString::from(error.clone()))
                    .absolute()
                    .inset_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_xs()
                    .text_color(colors.destructive)
                    .child(SharedString::from(error)),
            );
        } else if self.selected.is_none() {
            body = body.child(
                div()
                    .id("diff-empty")
                    .test_support()
                    .aria_label(t!("pane.no_changes"))
                    .absolute()
                    .inset_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_xs()
                    .text_color(colors.muted_foreground)
                    .child(t!("pane.no_changes")),
            );
        }

        body
    }
}

/// Which way a line changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Changed {
    Added,
    Removed,
}

/// The `+`/`-` lines of a unified diff, as byte ranges into it.
fn changed_lines(text: &str) -> Vec<(Changed, Range<usize>)> {
    let mut out = Vec::new();
    let mut offset = 0;
    // Set by `diff --git`, cleared by the first hunk: the lines between them are
    // never content.
    let mut in_header = false;
    for line in text.split_inclusive('\n') {
        let start = offset;
        offset += line.len();
        let body = line.trim_end_matches('\n');
        if body.starts_with("diff --git") {
            in_header = true;
            continue;
        }
        if body.starts_with("@@") {
            in_header = false;
            continue;
        }
        if in_header {
            continue;
        }
        let kind = match body.as_bytes().first() {
            Some(b'+') => Changed::Added,
            Some(b'-') => Changed::Removed,
            _ => continue,
        };
        out.push((kind, start..start + body.len()));
    }
    out
}

#[cfg(test)]
mod tests {
    // Not `use super::*`: the module above globs gpui in, and gpui's own `test`
    // attribute comes with it — under `cfg(test)` it shadows the built-in one,
    // and every `#[test]` below then expands into itself.
    use super::{Changed, changed_lines};

    /// The ranges, as `(kind, the text they cover)`, so the assertions read like
    /// the diff they came from.
    fn lines(text: &str) -> Vec<(Changed, String)> {
        changed_lines(text)
            .into_iter()
            .map(|(kind, range)| (kind, text[range].to_string()))
            .collect()
    }

    const SAMPLE: &str = "\
diff --git a/notes.md b/notes.md
index 1111111..2222222 100644
--- a/notes.md
+++ b/notes.md
@@ -1,3 +1,3 @@
 kept
-removed line
+added line
";

    #[test]
    fn only_content_lines_are_ranges() {
        assert_eq!(
            lines(SAMPLE),
            vec![
                (Changed::Removed, "-removed line".to_string()),
                (Changed::Added, "+added line".to_string()),
            ],
            "the file header and the hunk line are not changes"
        );
    }

    #[test]
    fn a_hunk_line_is_not_a_change_anywhere() {
        // A one-line file: the `@@` comes straight after the header, and `+`
        // and `-` appear in the header itself.
        let text = "\
diff --git a/x b/x
--- a/x
+++ b/x
@@ -1 +1 @@
-a
+b
";
        assert_eq!(
            lines(text),
            vec![
                (Changed::Removed, "-a".to_string()),
                (Changed::Added, "+b".to_string()),
            ]
        );
    }

    #[test]
    fn two_files_in_one_diff_are_both_read() {
        let text = "\
diff --git a/a b/a
--- a/a
+++ b/a
@@ -1 +1 @@
-one
+two
diff --git a/b b/b
--- a/b
+++ b/b
@@ -0,0 +1 @@
+fresh
";
        assert_eq!(
            lines(text),
            vec![
                (Changed::Removed, "-one".to_string()),
                (Changed::Added, "+two".to_string()),
                (Changed::Added, "+fresh".to_string()),
            ],
            "the second file's header is skipped again"
        );
    }

    #[test]
    fn a_context_only_diff_has_no_ranges() {
        let text = "\
diff --git a/x b/x
--- a/x
+++ b/x
@@ -1,2 +1,2 @@
 same
 also the same
";
        assert!(lines(text).is_empty());
    }

    #[test]
    fn ranges_are_byte_offsets_into_the_whole_text() {
        // The point of the ranges: they are what the editor's decorations are
        // measured against, so they have to be offsets into this string and not
        // into a line.
        for (kind, range) in changed_lines(SAMPLE) {
            let expected = match kind {
                Changed::Added => "+added line",
                Changed::Removed => "-removed line",
            };
            assert_eq!(&SAMPLE[range], expected);
        }
    }
}

/// The view-level half of the highlighting: which language the diff body is
/// built in. The pure half — that the name resolves to a grammar — is
/// `opener::tests::names_only_languages_the_registry_has`.
#[cfg(test)]
mod view_tests {
    use gpui_kit::{AppContext as _, Entity};

    use super::DiffView;
    use crate::pane::editor::tests::open;
    use crate::state::AppState;
    use syro_core::config::Settings;
    use syro_core::model::State;
    use syro_core::store::Store;

    /// A real workspace's state over a scratch repo, because the view reads its
    /// file list from it: a diff view over a made-up state would be a test of a
    /// view nobody ships.
    fn app(cx: &mut gpui_kit::TestAppContext, tag: &str) -> (Entity<AppState>, std::path::PathBuf) {
        let (dir, repo, _worktrees) = crate::state::fixture::scratch_repo(tag);
        let (state_path, _store_dir) = crate::state::fixture::temp_store(tag);
        Store::new(state_path.clone())
            .save(&State {
                repos: vec![repo],
                ..Default::default()
            })
            .unwrap();

        let workspaces = dir.join("workspaces");
        let state = cx.update(|cx| {
            cx.new(|cx| {
                AppState::open(Store::new(state_path), workspaces, cx)
                    // Not the developer's own settings file: this test is about
                    // the diff view, and the machine's config is not its input.
                    .with_settings(Settings::default())
            })
        });
        (state, dir)
    }

    #[gpui_kit::test]
    fn the_diff_body_is_parsed_as_a_diff(cx: &mut gpui_kit::TestAppContext) {
        let (state, _dir) = app(cx, "diff-language");
        let worktree = cx.update(|cx| {
            state
                .read(cx)
                .active()
                .cloned()
                .expect("the scratch repo has a worktree")
        });
        let (window, view) = open(cx, |window, cx| {
            cx.new(|cx| DiffView::new(window, cx, state.clone(), &worktree))
        });
        // Drawing runs the frame the highlighter would be built in; the language
        // assertion is what the colours are a function of.
        cx.update_window(window.into(), |_, window, cx| {
            window.draw(cx).clear(cx);
        })
        .expect("draw a frame");
        assert_eq!(
            cx.update(|cx| view.read(cx).language(cx)),
            crate::opener::DIFF_LANGUAGE
        );
    }
}
