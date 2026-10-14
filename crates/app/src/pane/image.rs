//! The image tab: a picture, drawn rather than read as text.

use std::path::{Path, PathBuf};

use gpui_kit::base::StyledExt as _;
use gpui_kit::base::TestSupportExt as _;
use gpui_kit::*;
use syro_core::model::TabSpec;

use crate::pane::PaneItem;

/// Behind every image, whatever the theme: line art is drawn dark far more
/// often than light, and a dark-stroked SVG on a dark pane is invisible. A
/// light gray rather than white, so white artwork still has an edge.
const BACKDROP: Hsla = hsla(0., 0., 0.92, 1.);

/// One image, on screen. Nothing about it is edited, so there is no state
/// beyond the path and the place the keyboard lands.
pub struct ImageView {
    path: PathBuf,
    focus: FocusHandle,
}

impl ImageView {
    pub fn new(path: PathBuf, cx: &mut Context<Self>) -> Self {
        Self {
            path,
            focus: cx.focus_handle(),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The file name, which is what a tab has room for.
    fn file_name(&self) -> SharedString {
        self.path
            .file_name()
            .map(|name| SharedString::from(name.to_string_lossy().into_owned()))
            .unwrap_or_else(|| SharedString::from(self.path.display().to_string()))
    }
}

impl Focusable for ImageView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl PaneItem for ImageView {
    fn title(&self, _cx: &App) -> SharedString {
        self.file_name()
    }

    fn to_spec(&self, _cx: &App) -> Option<TabSpec> {
        Some(TabSpec::File {
            path: self.path.clone(),
        })
    }
}

impl Render for ImageView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        // Centred and capped at the pane: a small image keeps its size, a large
        // one is scaled down to be seen whole. gpui guesses the format from the
        // bytes, so the name only decided that this pane was the right one.
        div()
            .id("image-pane")
            .test_support()
            .v_flex()
            .size_full()
            .min_h_0()
            .items_center()
            .justify_center()
            .p_4()
            .child(
                img(self.path.clone())
                    .max_w_full()
                    .max_h_full()
                    .object_fit(ObjectFit::Contain)
                    // On the image's own bounds, so an opaque image covers it and
                    // only the transparent parts — and the bars a clamped
                    // aspect leaves — show the backdrop.
                    .bg(BACKDROP),
            )
    }
}
