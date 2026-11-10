//! The image tab: a picture, drawn rather than read as text.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui_kit::base::StyledExt as _;
use gpui_kit::base::TestSupportExt as _;
use gpui_kit::*;
use syro_core::model::TabSpec;

use crate::pane::PaneItem;

/// Behind every image, whatever the theme: line art is drawn dark far more
/// often than light, and a dark-stroked SVG on a dark pane is invisible. A
/// light gray rather than white, so white artwork still has an edge.
const BACKDROP: Hsla = hsla(0., 0., 0.92, 1.);

/// gpui decodes every frame of a GIF or WebP up front and caches them for the
/// app's life: the README's 1.3 MB demo GIF became ~800 MB of RGBA. A preview
/// shows the first frame only; everything else goes through gpui as before.
pub fn still(path: PathBuf) -> ImageSource {
    let animated = path
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("gif") || ext.eq_ignore_ascii_case("webp"));
    if !animated {
        return path.into();
    }
    ImageSource::Custom(Arc::new(move |window, cx| {
        window.use_asset::<FirstFrame>(&path, cx)
    }))
}

enum FirstFrame {}

impl Asset for FirstFrame {
    type Source = PathBuf;
    type Output = Result<Arc<RenderImage>, ImageCacheError>;

    fn load(path: PathBuf, _cx: &mut App) -> impl Future<Output = Self::Output> + Send + 'static {
        async move { first_frame(&path) }
    }
}

fn first_frame(path: &Path) -> Result<Arc<RenderImage>, ImageCacheError> {
    // `load_from_memory` decodes just the first frame of an animation.
    let mut pixels = ::image::load_from_memory(&std::fs::read(path)?)?.into_rgba8();
    // gpui draws BGRA.
    for pixel in pixels.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
    Ok(Arc::new(RenderImage::new([::image::Frame::new(pixels)])))
}

#[cfg(test)]
mod tests {
    #[test]
    fn an_animated_gif_is_decoded_to_one_frame() {
        let gif =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/syro-demo.gif");
        let image = super::first_frame(&gif).unwrap();
        assert_eq!(image.frame_count(), 1);
    }
}

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
                img(still(self.path.clone()))
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
