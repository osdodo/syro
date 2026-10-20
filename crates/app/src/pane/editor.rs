//! The editor tab: a file from the tree, open for editing.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use gpui_kit::base::{
    StyledExt as _, TextView as BaseTextView, TextViewStyle as BaseTextViewStyle, Theme,
};
use gpui_kit::component::input::{Editor, EditorState, InputEvent};
use gpui_kit::*;
use rust_i18n::t;
use syro_core::model::TabSpec;

use crate::opener;
use crate::pane::PaneItem;

/// The state a block of text is shown in: the rope, the undo history and the grammar to parse
/// it with.
pub(crate) fn editor_state(
    window: &mut Window,
    cx: &mut App,
    language: Option<&str>,
) -> Entity<EditorState> {
    cx.new(|cx| {
        let mut state = EditorState::new(window, cx).line_number(true);
        if let Some(language) = language {
            state = state.language(language);
        }
        state
    })
}

/// One file, open.
pub struct EditorView {
    path: PathBuf,
    /// The rope and everything around it. `gpui-kit`'s, not ours: a second copy
    /// of the text would be a copy the editor never updates.
    state: Entity<EditorState>,
    /// Whether there are edits the file on disk does not have.
    dirty: bool,
    /// The file's modification time as of the last read or write. What the next
    /// focus is compared against.
    mtime: Option<SystemTime>,
    /// What to say about the file, when there is something: a reload, or edits
    /// kept against a file that moved on.
    notice: Option<String>,
    /// Whether the rendered document is on screen instead of its source. Only
    /// markdown has a rendering, so the switch is offered for nothing else.
    preview: bool,
    _change: Subscription,
    /// The window's activation observer: the moment this view looks at the file
    /// again.
    _watch: Subscription,
}

impl EditorView {
    /// Open `text` as `path`. `text` comes from [`opener::open`], so a file
    /// that is too large or not text never reaches here.
    pub fn new(window: &mut Window, cx: &mut Context<Self>, path: PathBuf, text: String) -> Self {
        // `opener` answers with a registry identifier; `None` — an extension
        // nothing names — leaves the editor's own default, which is the empty
        // name rather than a language called "text".
        let language = opener::language_for(&path);
        let state = editor_state(window, cx, language);
        // Read before the text lands, so the comparison the first focus makes is
        // against the file as it was read. `ponytail:` a write between the
        // caller's read and this line is missed until the next focus.
        let mtime = std::fs::metadata(&path)
            .and_then(|meta| meta.modified())
            .ok();
        // Through `set_value`, not `default_value`: the value has to land in the
        // rope the editor reads, and the builder's version goes through the same
        // call with the same window.
        state.update(cx, |state, cx| state.set_value(text, window, cx));

        // The dirty flag is set on the first edit and cleared by a save, rather than recomputed by
        // comparing the whole rope with what was read: a comparison on every keystroke is
        // O(file) per key, and "has edits" is the question the tab marker asks.
        let change = cx.subscribe(&state, |this, _state, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) && !this.dirty {
                this.dirty = true;
                // An edit is the answer to whatever the file had to say: the
                // reader is working, and a notice from the last focus is stale.
                this.notice = None;
                cx.notify();
            }
        });

        // Registered here, where the window is: the observer fires on both
        // edges of activation, so whether the window is active is what decides
        // whether this is a moment to look at the file.
        let watch = cx.observe_window_activation(window, |this, window, cx| {
            if window.is_window_active() {
                this.check_the_file_on_disk(window, cx);
            }
        });

        Self {
            path,
            state,
            dirty: false,
            mtime,
            notice: None,
            preview: false,
            _change: change,
            _watch: watch,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The buffer as text. What a test reads to see whether a reload happened;
    /// `save` is what writes it, so this is also what a save is about to write.
    #[allow(
        dead_code,
        reason = "the view's read surface: what the window tests assert on"
    )]
    pub fn text(&self, cx: &App) -> SharedString {
        self.state.read(cx).value()
    }

    /// The language the editor is parsing as, exactly as the state reports it:
    /// a registry identifier like `"rust"`, or empty for a file nothing names.
    /// This is what a test asserts on — the colours are a function of it.
    #[allow(
        dead_code,
        reason = "the view's read surface: what the window tests assert on"
    )]
    pub fn language(&self, cx: &App) -> SharedString {
        self.state.read(cx).language_name()
    }

    /// What to say about the file on disk, if anything: a reload that happened,
    /// or edits that were kept against it.
    #[allow(
        dead_code,
        reason = "the view's read surface: what the window tests assert on"
    )]
    pub fn notice(&self) -> Option<String> {
        self.notice.clone()
    }

    /// Look at the file again, and take one of three roads.
    fn check_the_file_on_disk(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let now = std::fs::metadata(&self.path)
            .and_then(|meta| meta.modified())
            .ok();
        if now == self.mtime {
            if self.notice.take().is_some() {
                cx.notify();
            }
            return;
        }

        // It moved on. A buffer with edits of its own is not replaced: two
        // writers is a conflict, and the one who can see both sides is the
        // reader, so the file stays where it is and the sentence says so.
        if self.dirty {
            self.set_notice(Some(t!("editor.changed_on_disk").to_string()), cx);
            return;
        }

        match opener::open(&self.path) {
            Ok(opener::Opened::Text(text)) => {
                self.state
                    .update(cx, |state, cx| state.set_value(text, window, cx));
                self.mtime = now;
                self.set_notice(Some(t!("editor.reloaded").to_string()), cx);
            }
            // Too large or binary now, when it was text before: the buffer is
            // left as it was and the refusal is what the reader is told.
            Ok(opener::Opened::Refused(refusal)) => {
                self.set_notice(Some(refusal.message(&self.path)), cx)
            }
            // An editor's path never changes, so a file that was text when the
            // tab opened is still text under this name. The arm is here because
            // the enum says so, and it says the same thing a binary would.
            Ok(opener::Opened::Image) => {
                self.set_notice(Some(opener::Refusal::Binary.message(&self.path)), cx)
            }
            // Gone, most often: a file deleted outside the app.
            Err(error) => self.set_notice(Some(format!("{error:#}")), cx),
        }
    }

    /// Whether the rendered document is on screen rather than the source.
    #[allow(
        dead_code,
        reason = "the view's read surface: what the window tests assert on"
    )]
    pub fn previewing(&self) -> bool {
        self.preview
    }

    /// Switch between the source and the rendered document. Nothing is written
    /// either way: the buffer is the source, and the preview only reads it.
    fn toggle_preview(&mut self, cx: &mut Context<Self>) {
        self.preview = !self.preview;
        cx.notify();
    }

    fn set_notice(&mut self, notice: Option<String>, cx: &mut Context<Self>) {
        if self.notice != notice {
            self.notice = notice;
            cx.notify();
        }
    }

    /// The file name, which is what a tab has room for.
    fn file_name(&self) -> SharedString {
        self.path
            .file_name()
            .map(|name| SharedString::from(name.to_string_lossy().into_owned()))
            .unwrap_or_else(|| SharedString::from(self.path.display().to_string()))
    }
}

impl Focusable for EditorView {
    /// The editor's own focus handle: the state is what receives the keys, so
    /// focusing the tab has to mean focusing the state inside it.
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.state.read(cx).focus_handle(cx)
    }
}

impl PaneItem for EditorView {
    fn title(&self, _cx: &App) -> SharedString {
        self.file_name()
    }

    fn is_dirty(&self, _cx: &App) -> bool {
        self.dirty
    }

    /// Write the buffer back over the file it came from. The whole file, not a
    /// diff: the editor has no idea what changed, and a full write is what
    /// makes `git diff` afterwards show what the user sees.
    fn save(&mut self, cx: &mut Context<Self>) -> anyhow::Result<()> {
        let text = self.state.read(cx).value();
        std::fs::write(&self.path, text.as_bytes())?;
        // What we just wrote is the file's state as far as this view is
        // concerned, so the next focus is compared against it rather than
        // reloading our own write.
        self.mtime = std::fs::metadata(&self.path)
            .and_then(|meta| meta.modified())
            .ok();
        self.notice = None;
        if self.dirty {
            self.dirty = false;
        }
        cx.notify();
        Ok(())
    }

    fn to_spec(&self, _cx: &App) -> Option<TabSpec> {
        Some(TabSpec::File {
            path: self.path.clone(),
        })
    }
}

fn markdown_image_source(uri: &SharedUri, base: Option<&Path>) -> ImageSource {
    let source = uri.as_ref();
    if source.starts_with("data:") {
        let Ok(data_url) = data_url::DataUrl::process(source) else {
            return ImageSource::Resource(Resource::Uri(uri.clone()));
        };
        let mime = data_url.mime_type();
        let Some(format) = ImageFormat::from_mime_type(&format!("{}/{}", mime.type_, mime.subtype))
        else {
            return ImageSource::Resource(Resource::Uri(uri.clone()));
        };
        let Ok((bytes, _)) = data_url.decode_to_vec() else {
            return ImageSource::Resource(Resource::Uri(uri.clone()));
        };
        return ImageSource::Image(Arc::new(Image::from_bytes(format, bytes)));
    }
    if source.contains("://") {
        return ImageSource::Resource(Resource::Uri(uri.clone()));
    }

    let path = Path::new(source);
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else if let Some(base) = base {
        base.join(path)
    } else {
        return ImageSource::Resource(Resource::Uri(uri.clone()));
    };
    ImageSource::Resource(Resource::Path(path.into()))
}

impl Render for EditorView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = Theme::global(cx).tokens.colors.clone();
        // Markdown is the one language with something to render, so it is the
        // one the switch is offered for.
        let previewable = self.state.read(cx).language_name().as_ref() == "markdown";
        let mut root = div()
            .id("editor-pane")
            .test_support()
            .v_flex()
            .size_full()
            .min_h_0();
        // One header row, drawn only when it has something to hold: what is
        // said about the file on the left, the source/preview switch on the
        // right. A pane with neither is the whole editor.
        if self.notice.is_some() || previewable {
            let mut header = div()
                .id("editor-header")
                .test_support()
                .h_flex()
                .w_full()
                .px_2()
                .py_1()
                .justify_between()
                .text_xs()
                .text_color(colors.muted_foreground);
            header = header.child(match &self.notice {
                Some(notice) => div()
                    .id("editor-notice")
                    .test_support()
                    .aria_label(SharedString::from(notice.clone()))
                    .child(SharedString::from(notice.clone())),
                None => div().id("editor-notice").test_support(),
            });
            if previewable {
                let label = if self.preview {
                    t!("editor.edit")
                } else {
                    t!("editor.preview")
                }
                .to_string();
                header = header.child(
                    div()
                        .id("editor-preview-toggle")
                        .test_support()
                        .aria_label(SharedString::from(label.clone()))
                        .cursor_pointer()
                        .px_1()
                        .text_color(colors.foreground)
                        .hover(|style| style.text_color(colors.muted_foreground))
                        .on_click(cx.listener(|this, _, _, cx| this.toggle_preview(cx)))
                        .child(SharedString::from(label)),
                );
            }
            root = root.child(header);
        }
        if self.preview && previewable {
            let text = self.state.read(cx).value();
            // The wrapper is what gives the document a bounded height to scroll
            // inside; `scrollable` is the text view's own scroll for a long
            // document.
            root.child(
                div()
                    .id("editor-preview")
                    .test_support()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    // A document reads better with a margin around it than
                    // against the pane's own edges.
                    .p_4()
                    // Keep Markdown typography at the document's default body size.
                    .child(
                        BaseTextView::markdown("editor-preview-text", text)
                            .image_source({
                                let base = self.path.parent().map(Path::to_path_buf);
                                move |uri| markdown_image_source(uri, base.as_deref())
                            })
                            .style(
                                BaseTextViewStyle::from_theme(&Theme::global(cx))
                                    .with_paragraph_gap(rems(0.75)),
                            )
                            .scrollable(true),
                    ),
            )
        } else {
            root.child(
                Editor::new(&self.state)
                    // The pane is the frame; a second border inside it is one
                    // too many. `h(relative(1.))` is what gives it the pane's
                    // height — an editor sizes to its content otherwise.
                    .bordered(false)
                    // And square, for the same reason: the component rounds itself
                    // by the theme's radius, and a rounded rectangle that reaches
                    // all four edges is a frame drawn inside a frame. This refines
                    // over the component's own style, which is applied first.
                    .rounded_none()
                    .h(relative(1.)),
            )
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    // The module above globs gpui in, and gpui's own `test` attribute comes with
    // it — under `cfg(test)` it shadows the built-in one, so nothing here is
    // glob-imported and every `#[test]` below means Rust's.
    use std::path::PathBuf;

    use gpui_kit::base::Root;
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{
        App, AppContext as _, Bounds, Entity, Focusable as _, ImageSource, Point, Render, Resource,
        SharedUri, TestAppContext, Window, WindowBounds, WindowHandle, WindowOptions, px, size,
    };

    use super::{EditorView, editor_state};
    use crate::pane::PaneItem as _;

    /// A window holding one view, with the component layer initialized.
    pub(crate) fn open<V: Render>(
        cx: &mut TestAppContext,
        build: impl FnOnce(&mut Window, &mut App) -> Entity<V>,
    ) -> (WindowHandle<Root>, Entity<V>) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            let bounds = Bounds {
                origin: Point::default(),
                size: size(px(800.), px(600.)),
            };
            let (window, content) = gpui_kit::open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    ..Default::default()
                },
                cx,
                build,
            )
            .expect("open test window");
            (window.downcast::<Root>().expect("Base Root"), content)
        })
    }

    /// One frame, so anything the editor builds lazily — the highlighter the
    /// language names — has run before the assertion.
    fn drawn(cx: &mut TestAppContext, window: WindowHandle<Root>) {
        cx.update_window(window.into(), |_, window, cx| {
            window.draw(cx).clear(cx);
        })
        .expect("draw a frame");
    }

    /// A file holding `source`, open in a real window under `path`.
    pub(crate) fn opened(
        cx: &mut TestAppContext,
        path: &str,
        source: &str,
    ) -> (WindowHandle<Root>, Entity<EditorView>) {
        let path = PathBuf::from(path);
        let text = source.to_string();
        let (window, view) = open(cx, |window, cx| {
            cx.new(|cx| EditorView::new(window, cx, path.clone(), text.clone()))
        });
        drawn(cx, window);
        (window, view)
    }

    /// A file on disk, with its own scratch name, for the tests that compare
    /// against the filesystem rather than only against a string.
    fn temp_file(tag: &str, text: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "syro-editor-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, text).unwrap();
        path
    }

    /// Away from the window and back, which is when the editor looks at the file
    /// again. The platform reports the change from a task, so the executor has to
    /// run for the window to believe it left.
    fn refocus(cx: &mut TestAppContext, window: WindowHandle<Root>) {
        let mut visual = gpui_kit::VisualTestContext::from_window(window.into(), cx);
        visual.deactivate_window();
        cx.run_until_parked();
        cx.update_window(window.into(), |_, window, _cx| {
            window.activate_window();
        })
        .expect("activate the window");
        cx.run_until_parked();
        drawn(cx, window);
    }

    /// Type into the editor, which is what makes it dirty.
    fn type_into(
        cx: &mut TestAppContext,
        window: WindowHandle<Root>,
        view: &Entity<EditorView>,
        text: &str,
    ) {
        cx.update_window(window.into(), |_, window, cx| {
            window.draw(cx).clear(cx);
            let focus = view.read(cx).focus_handle(cx);
            window.focus(&focus, cx);
            window.input(text, cx);
        })
        .expect("type into the editor");
    }

    #[test]
    fn markdown_relative_image_resolves_from_document_directory() {
        let root = std::env::temp_dir().join(format!("syro-markdown-image-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let uri: SharedUri = "images/logo.png".into();
        let source = super::markdown_image_source(&uri, Some(&root));
        assert!(matches!(
            source,
            ImageSource::Resource(Resource::Path(path))
                if path.as_ref() == root.join("images/logo.png").as_path()
        ));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[gpui_kit::test]
    fn a_file_changed_on_disk_while_unmodified_is_reloaded(cx: &mut TestAppContext) {
        let path = temp_file("reload", "before\n");
        let (window, view) = opened(cx, path.to_str().unwrap(), "before\n");
        assert_eq!(cx.update(|cx| view.read(cx).text(cx)), "before\n");
        // A focus with nothing changed says nothing: the notice is about a fact,
        // not about a focus.
        refocus(cx, window);
        assert!(cx.update(|cx| view.read(cx).notice()).is_none());

        // Somebody else wrote it — an agent, a `git checkout`, another editor.
        std::fs::write(&path, "after\n").unwrap();
        refocus(cx, window);

        assert_eq!(
            cx.update(|cx| view.read(cx).text(cx)),
            "after\n",
            "the editor kept text the file no longer has"
        );
        assert_eq!(
            cx.update(|cx| view.read(cx).notice()).as_deref(),
            Some("reloaded from disk")
        );

        let _ = std::fs::remove_file(&path);
    }

    #[gpui_kit::test]
    fn a_file_changed_on_disk_while_modified_keeps_the_edits(cx: &mut TestAppContext) {
        let path = temp_file("keep", "before\n");
        let (window, view) = opened(cx, path.to_str().unwrap(), "before\n");

        // The reader is working: an edit of their own, unsaved.
        type_into(cx, window, &view, "and mine");
        drawn(cx, window);
        assert!(cx.update(|cx| view.read(cx).is_dirty(cx)));

        std::fs::write(&path, "theirs\n").unwrap();
        refocus(cx, window);

        // Both sides survive: the buffer keeps the reader's text, and the file
        // on disk keeps the other writer's.
        assert!(
            cx.update(|cx| view.read(cx).text(cx)).contains("and mine"),
            "the reader's edits were replaced"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "theirs\n");
        assert_eq!(
            cx.update(|cx| view.read(cx).notice()).as_deref(),
            Some("changed on disk — your edits are kept")
        );

        let _ = std::fs::remove_file(&path);
    }

    #[gpui_kit::test]
    fn a_save_is_not_a_change_to_reload(cx: &mut TestAppContext) {
        let path = temp_file("save", "before\n");
        let (window, view) = opened(cx, path.to_str().unwrap(), "before\n");
        type_into(cx, window, &view, "mine");
        drawn(cx, window);
        cx.update(|cx| {
            view.update(cx, |view, cx| view.save(cx).expect("save"));
        });

        refocus(cx, window);

        // Our own write is the file's state as far as this view is concerned;
        // reloading it would be the editor fighting itself.
        assert!(cx.update(|cx| view.read(cx).notice()).is_none());
        assert_eq!(
            cx.update(|cx| view.read(cx).text(cx)),
            std::fs::read_to_string(&path).unwrap()
        );

        let _ = std::fs::remove_file(&path);
    }

    #[gpui_kit::test]
    fn a_file_deleted_on_disk_is_reported_and_the_buffer_stays(cx: &mut TestAppContext) {
        let path = temp_file("gone", "before\n");
        let (window, view) = opened(cx, path.to_str().unwrap(), "before\n");
        std::fs::remove_file(&path).unwrap();

        refocus(cx, window);

        assert_eq!(
            cx.update(|cx| view.read(cx).text(cx)),
            "before\n",
            "the buffer went with the file"
        );
        assert!(
            cx.update(|cx| view.read(cx).notice()).is_some(),
            "the reader was not told the file is gone"
        );
    }

    /// Whether the frame last drawn has an observed element with this id.
    fn element(cx: &mut TestAppContext, window: WindowHandle<Root>, id: &str) -> bool {
        drawn(cx, window);
        let id = id.to_string();
        cx.update_window(window.into(), move |_, window, _| {
            window.try_find(id).is_some()
        })
        .expect("read the frame")
    }

    /// The source is what opens — typing and cmd-s keep meaning the editor — and
    /// the document is one click away, in both directions.
    #[test]
    fn a_markdown_file_offers_the_rendered_document() {
        let cx = &mut TestAppContext::single();
        let (window, view) = opened(cx, "/tmp/example.md", "# Title\n\nBody\n");

        assert!(!cx.update(|cx| view.read(cx).previewing()));
        assert!(element(cx, window, "editor-preview-toggle"));
        assert!(!element(cx, window, "editor-preview"));

        cx.update(|cx| view.update(cx, |view, cx| view.toggle_preview(cx)));
        assert!(cx.update(|cx| view.read(cx).previewing()));
        assert!(element(cx, window, "editor-preview"));

        cx.update(|cx| view.update(cx, |view, cx| view.toggle_preview(cx)));
        assert!(!cx.update(|cx| view.read(cx).previewing()));
        assert!(!element(cx, window, "editor-preview"));
    }

    /// Nothing to render, so nothing to switch to: a Rust file gets no
    /// Preview/Edit control.
    #[test]
    fn a_language_without_a_rendering_has_no_switch() {
        let cx = &mut TestAppContext::single();
        let (window, _view) = opened(cx, "/tmp/example.rs", "fn main() {}\n");
        assert!(!element(cx, window, "editor-preview-toggle"));
    }

    #[test]
    fn a_rust_file_is_parsed_as_rust() {
        let cx = &mut TestAppContext::single();
        let (_, view) = opened(cx, "/tmp/example.rs", "fn main() {}\n");
        assert_eq!(cx.update(|cx| view.read(cx).language(cx)), "rust");
    }

    #[test]
    fn a_typescript_file_is_parsed_as_typescript() {
        let cx = &mut TestAppContext::single();
        let (_, view) = opened(cx, "/tmp/example.ts", "export const n: number = 1;\n");
        assert_eq!(cx.update(|cx| view.read(cx).language(cx)), "typescript");
    }

    /// The two ways there is nothing to parse, and neither is an error: an extension no table
    /// names, and a name with no grammar behind it.
    #[test]
    fn a_file_with_no_language_is_plain_text() {
        let cx = &mut TestAppContext::single();
        let (_, view) = opened(cx, "/tmp/notes.wat", "not a language\n");
        // No language set at all, so the state reports the name it started with.
        assert!(cx.update(|cx| view.read(cx).language(cx)).is_empty());
    }

    #[test]
    fn a_name_with_no_grammar_is_plain_text() {
        let cx = &mut TestAppContext::single();
        let (window, editor) = open(cx, |window, cx| editor_state(window, cx, Some("text")));
        drawn(cx, window);
        // The registry knows `text` and has no grammar for it: the state keeps
        // the name, the editor parses nothing.
        assert_eq!(cx.update(|cx| editor.read(cx).language_name()), "text");

        // A name the registry has never heard of is the same story, which is the
        // case a typo in the table would produce.
        let (window, editor) = open(cx, |window, cx| {
            editor_state(window, cx, Some("not-a-language"))
        });
        drawn(cx, window);
        assert_eq!(
            cx.update(|cx| editor.read(cx).language_name()),
            "not-a-language"
        );
    }
}
