//! What a tab can hold.
pub mod diff;
pub mod editor;
pub mod image;
pub mod terminal;

use anyhow::Result;
use gpui_kit::*;
use syro_core::model::{AgentStatus, TabSpec};

/// One tab's contents.
pub trait PaneItem: Render + Focusable {
    /// The tab label.
    fn title(&self, cx: &App) -> SharedString;

    /// The status dot: `None` for a pane that is not running an agent.
    #[allow(dead_code, reason = "the tab's own dot is the last item")]
    fn status(&self, _cx: &App) -> Option<AgentStatus> {
        None
    }

    /// Whether the tab needs a save marker.
    fn is_dirty(&self, _cx: &App) -> bool {
        false
    }

    /// Save the tab's contents. A no-op for the panes that hold nothing
    /// savable, which is every pane until the editor lands.
    fn save(&mut self, _cx: &mut Context<Self>) -> Result<()> {
        Ok(())
    }

    /// How to bring this tab back after a restart; `None` for one that does not
    /// survive one — anything that cannot be described by a `TabSpec`.
    fn to_spec(&self, cx: &App) -> Option<TabSpec>;
}
