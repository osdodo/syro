//! The diff tab: one changed file's unified diff.

use std::ops::Range;
use std::path::PathBuf;
use std::rc::Rc;

use gpui_kit::base::{TestSupportExt as _, Theme};
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rust_i18n::t;
use syro_core::git::{ChangeKind, FileChange};
use syro_core::model::{TabSpec, Worktree, WorktreeId};

use crate::opener;
use crate::pane::PaneItem;
use crate::state::AppState;

/// How strong an added/removed line's fill is. A diff is read as text, and a
/// fill that competes with the text is a fill that hides it.
const FILL_ALPHA: f32 = 0.16;
/// The line-number cells of a changed line are a shade stronger than its text,
/// the way GitHub draws them.
const GUTTER_ALPHA: f32 = 0.28;

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
    focus: FocusHandle,
    /// The raw diff, as git printed it.
    text: String,
    /// `text` parsed into what is drawn: headers dropped, line numbers counted.
    rows: Rc<Vec<Row>>,
    /// Digits in the largest line number, and the widest row: per text, not per frame.
    digits: usize,
    longest: Option<usize>,
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
        let mut view = Self {
            worktree_id: worktree.id.clone(),
            root: worktree.path.clone(),
            base_ref: worktree.base_ref.clone(),
            selected: None,
            selected_kind: None,
            commit: None,
            app,
            focus: cx.focus_handle(),
            text: String::new(),
            rows: Rc::default(),
            digits: 1,
            longest: None,
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
    pub fn text(&self, _cx: &App) -> String {
        self.text.clone()
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

    /// Replace the text and the rows drawn from it together.
    fn set_text(&mut self, text: String, _window: &mut Window, _cx: &mut Context<Self>) {
        let rows = parse(&text);
        self.digits = rows
            .iter()
            .filter_map(|row| match row {
                Row::Line { old, new, .. } => old.max(new).map(|n| n.to_string().len()),
                _ => None,
            })
            .max()
            .unwrap_or(1);
        self.longest = longest(&rows);
        self.rows = Rc::new(rows);
        self.text = text;
    }

    /// The visible rows, GitHub-style: two line-number columns, a `+`/`-`
    /// marker, and a fill that spans the whole width of the pane.
    fn render_rows(
        &mut self,
        range: Range<usize>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let theme = cx.theme();
        let (added, removed, info, muted) = (
            theme.success,
            theme.danger,
            theme.info,
            theme.muted_foreground,
        );
        let mono = Theme::global(cx).tokens.typography.mono_md;
        // Wide enough for the largest line number in the file.
        let gutter = mono.size * (0.6 * self.digits as f32) + px(16.);
        let number = move |n: Option<u32>, fill: Option<Hsla>| {
            div()
                .flex_none()
                .w(gutter)
                .h_full()
                .px_2()
                .text_right()
                .text_color(muted)
                .when_some(fill, |cell, fill| cell.bg(fill))
                .child(
                    n.map(|n| SharedString::from(n.to_string()))
                        .unwrap_or_default(),
                )
        };
        range
            .map(|ix| {
                let row = div()
                    .id(ix)
                    .flex()
                    .min_w_full()
                    .h(mono.line_height)
                    .whitespace_nowrap();
                match &self.rows[ix] {
                    Row::Hunk(text) => row
                        .bg(info.alpha(FILL_ALPHA))
                        .text_color(muted)
                        .child(number(None, Some(info.alpha(GUTTER_ALPHA))))
                        .child(number(None, Some(info.alpha(GUTTER_ALPHA))))
                        .child(div().flex_none().pl_2().child(text.clone())),
                    Row::Line {
                        kind,
                        old,
                        new,
                        text,
                    } => {
                        let (fill, gutter_fill, marker) = match kind {
                            LineKind::Added => (
                                Some(added.alpha(FILL_ALPHA)),
                                Some(added.alpha(GUTTER_ALPHA)),
                                "+",
                            ),
                            LineKind::Removed => (
                                Some(removed.alpha(FILL_ALPHA)),
                                Some(removed.alpha(GUTTER_ALPHA)),
                                "-",
                            ),
                            LineKind::Context | LineKind::Meta => (None, None, ""),
                        };
                        row.when_some(fill, |row, fill| row.bg(fill))
                            .when(*kind == LineKind::Meta, |row| row.text_color(muted))
                            .child(number(*old, gutter_fill))
                            .child(number(*new, gutter_fill))
                            .child(div().flex_none().w(px(20.)).text_center().child(marker))
                            .child(div().flex_none().pr_4().child(text.clone()))
                    }
                }
                .into_any_element()
            })
            .collect()
    }
}

impl Focusable for DiffView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
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
            .track_focus(&self.focus)
            .font_family(Theme::global(cx).tokens.typography.mono.clone())
            .text_size(Theme::global(cx).tokens.typography.mono_md.size)
            .child(
                div().id("diff-text").size_full().child(
                    uniform_list(
                        "diff-rows",
                        self.rows.len(),
                        cx.processor(Self::render_rows),
                    )
                    // Measure the widest row, so long lines scroll sideways.
                    .with_width_from_item(self.longest)
                    .with_horizontal_sizing_behavior(ListHorizontalSizingBehavior::Unconstrained)
                    .size_full(),
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

/// What a diff line is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LineKind {
    Context,
    Added,
    Removed,
    /// Neither file's content: `\ No newline at end of file`, `Binary files … differ`.
    Meta,
}

/// One drawn row of the diff.
#[derive(Debug, Clone, PartialEq)]
enum Row {
    /// The `@@ … @@` line that starts a hunk.
    Hunk(SharedString),
    /// A line with its number in the old and the new file, where it has one.
    Line {
        kind: LineKind,
        old: Option<u32>,
        new: Option<u32>,
        text: SharedString,
    },
}

/// The index of the row with the most text: what the list measures its width by.
fn longest(rows: &[Row]) -> Option<usize> {
    (0..rows.len()).max_by_key(|&ix| match &rows[ix] {
        Row::Hunk(text) | Row::Line { text, .. } => text.len(),
    })
}

/// A unified diff as rows: the per-file header (`diff --git`, `index`,
/// `---`/`+++`, modes, renames) is dropped, as GitHub does, and every content
/// line is numbered from its hunk's `@@ -old +new @@`.
fn parse(text: &str) -> Vec<Row> {
    const HEADER: &[&str] = &[
        "diff --git",
        "index ",
        "--- ",
        "+++ ",
        "new file mode",
        "deleted file mode",
        "old mode",
        "new mode",
        "similarity index",
        "dissimilarity index",
        "rename from",
        "rename to",
        "copy from",
        "copy to",
    ];
    let mut rows = Vec::new();
    // Set by `diff --git`, cleared by the first hunk: the lines between them are
    // never content, even the ones that start with `+` or `-`.
    let mut in_header = false;
    let (mut old, mut new) = (0u32, 0u32);
    for line in text.lines() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        // gpui draws a tab as nothing in particular; four spaces is what a reader expects.
        let shown = |s: &str| SharedString::from(s.replace('\t', "    "));
        if line.starts_with("diff --git") {
            in_header = true;
            continue;
        }
        if line.starts_with("@@") {
            in_header = false;
            (old, new) = hunk_start(line);
            rows.push(Row::Hunk(shown(line)));
            continue;
        }
        if in_header && HEADER.iter().any(|prefix| line.starts_with(prefix)) {
            continue;
        }
        let meta = |text| Row::Line {
            kind: LineKind::Meta,
            old: None,
            new: None,
            text,
        };
        if in_header {
            rows.push(meta(shown(line)));
            continue;
        }
        let (marker, body) = line.split_at(line.len().min(1));
        rows.push(match marker {
            "+" => {
                new += 1;
                Row::Line {
                    kind: LineKind::Added,
                    old: None,
                    new: Some(new - 1),
                    text: shown(body),
                }
            }
            "-" => {
                old += 1;
                Row::Line {
                    kind: LineKind::Removed,
                    old: Some(old - 1),
                    new: None,
                    text: shown(body),
                }
            }
            " " | "" => {
                old += 1;
                new += 1;
                Row::Line {
                    kind: LineKind::Context,
                    old: Some(old - 1),
                    new: Some(new - 1),
                    text: shown(body),
                }
            }
            _ => meta(shown(line)),
        });
    }
    rows
}

/// The first old and new line numbers of `@@ -a,b +c,d @@`.
fn hunk_start(line: &str) -> (u32, u32) {
    let mut parts = line.split_whitespace().skip(1);
    let mut start = |sign: char| {
        parts
            .next()
            .and_then(|part| part.strip_prefix(sign))
            .and_then(|part| part.split(',').next())
            .and_then(|n| n.parse().ok())
            .unwrap_or(0)
    };
    (start('-'), start('+'))
}

#[cfg(test)]
mod tests {
    // Not `use super::*`: the module above globs gpui in, and gpui's own `test`
    // attribute comes with it — under `cfg(test)` it shadows the built-in one,
    // and every `#[test]` below then expands into itself.
    use super::{LineKind, Row, parse};

    /// The rows as `(kind, old, new, text)`, hunks as `None` kinds.
    #[allow(clippy::type_complexity, reason = "a tuple reads like the diff")]
    fn rows(text: &str) -> Vec<(Option<LineKind>, Option<u32>, Option<u32>, String)> {
        parse(text)
            .into_iter()
            .map(|row| match row {
                Row::Hunk(text) => (None, None, None, text.to_string()),
                Row::Line {
                    kind,
                    old,
                    new,
                    text,
                } => (Some(kind), old, new, text.to_string()),
            })
            .collect()
    }

    #[test]
    fn header_is_dropped_and_lines_are_numbered() {
        let text = "\
diff --git a/notes.md b/notes.md
index 1111111..2222222 100644
--- a/notes.md
+++ b/notes.md
@@ -9,3 +9,3 @@ fn context
 kept
-removed line
+added line
\\ No newline at end of file
";
        use LineKind::*;
        assert_eq!(
            rows(text),
            vec![
                (None, None, None, "@@ -9,3 +9,3 @@ fn context".into()),
                (Some(Context), Some(9), Some(9), "kept".into()),
                (Some(Removed), Some(10), None, "removed line".into()),
                (Some(Added), None, Some(10), "added line".into()),
                (
                    Some(Meta),
                    None,
                    None,
                    "\\ No newline at end of file".into()
                ),
            ]
        );
    }

    #[test]
    fn a_binary_file_says_so() {
        let text = "\
diff --git a/x.png b/x.png
index 1111111..2222222 100644
Binary files a/x.png and b/x.png differ
";
        assert_eq!(
            rows(text),
            vec![(
                Some(LineKind::Meta),
                None,
                None,
                "Binary files a/x.png and b/x.png differ".into()
            )]
        );
    }

    #[test]
    fn a_new_file_counts_from_one() {
        let text = "\
diff --git a/b b/b
new file mode 100644
--- /dev/null
+++ b/b
@@ -0,0 +1,2 @@
+one
+two
";
        let rows = rows(text);
        assert_eq!(rows[1].2, Some(1));
        assert_eq!(rows[2].2, Some(2));
        assert_eq!(rows.len(), 3);
    }
}
