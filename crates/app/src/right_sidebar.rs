//! The right sidebar: the activity strip, and the panel behind it.

use gpui_kit::base::{ColorTokens, StyledExt as _, TestSupportExt as _};
use gpui_kit::component::{Icon, IconName};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rust_i18n::t;

/// How wide the sidebar is before anyone drags it.
pub const WIDTH: f32 = 260.;
pub const MIN_WIDTH: f32 = 160.;
pub const MAX_WIDTH: f32 = 500.;

/// The strip's height, which is the tab bar's: the two are next to each other
/// and a control that jumps between them is a control nobody finds twice.
pub const STRIP_HEIGHT: f32 = 36.;

/// Which panel the sidebar is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Panel {
    #[default]
    Files,
    Sessions,
    Git,
}

impl Panel {
    /// In strip order.
    pub const ALL: [Panel; 3] = [Panel::Files, Panel::Git, Panel::Sessions];

    pub fn label(self) -> String {
        match self {
            Panel::Files => t!("tree.files"),
            Panel::Sessions => t!("sessions.title"),
            Panel::Git => t!("git.source_control"),
        }
        .to_string()
    }

    /// The icon on the strip. All three are in the installed Lucide subset (see
    /// the icon test in `main.rs`).
    pub fn icon(self) -> IconName {
        match self {
            // The explorer icon is a document; the installed subset has no
            // "files" mark that is not a folder, and a folder is the mark the
            // sidebar's own repository list uses.
            Panel::Files => IconName::Folder,
            // Tabler's "category"; `LayoutDashboard` is the nearest four-square
            // mark in the installed set.
            Panel::Sessions => IconName::LayoutDashboard,
            // A git-branch glyph would be the mark here; the installed Lucide
            // subset has no git icons at all, and `Network` — nodes on
            // connecting lines — is the nearest mark in it for a history of commits.
            Panel::Git => IconName::Network,
        }
    }

    /// The id a test finds the strip's button by.
    pub fn button_id(self) -> &'static str {
        match self {
            Panel::Files => "panel-files",
            Panel::Sessions => "panel-sessions",
            Panel::Git => "panel-git",
        }
    }
}

/// Show a panel, or hide it when it is the one already on screen. Dispatched by
/// the strip; the workspace owns what it does to the sidebar.
#[derive(Clone, PartialEq, Debug, Action)]
#[action(namespace = panel, no_json)]
pub struct Show {
    pub panel: Panel,
}

/// Show the panel that was on screen last, or hide it. The strip's trailing
/// toggle and the button that floats in the pane area while the sidebar is down.
#[derive(Clone, PartialEq, Debug, Action)]
#[action(namespace = panel, no_json)]
pub struct Toggle;

/// The activity strip: one button per panel, and the toggle at the trailing end.
pub fn strip(
    active: Option<Panel>,
    colors: &ColorTokens,
    cx: &mut Context<crate::Workspace>,
) -> AnyElement {
    let mut row = div()
        .id("panel-strip")
        .test_support()
        .h_flex()
        .items_center()
        .gap_1()
        .px_2()
        .h(px(STRIP_HEIGHT))
        .flex_shrink_0()
        .w_full()
        .border_b_1()
        .border_color(colors.border);

    for panel in Panel::ALL {
        let selected = active == Some(panel);
        row = row.child(
            div()
                .id(panel.button_id())
                .test_support()
                .aria_label(SharedString::from(
                    t!("panel.show", name = panel.label()).to_string(),
                ))
                .h_flex()
                .items_center()
                .justify_center()
                .size(px(24.))
                .rounded_sm()
                .cursor_pointer()
                // The selected panel is the one the strip says is selected: the
                // same mark a tab carries, because it is the same idea.
                .when(selected, |el| {
                    el.bg(colors.muted).text_color(colors.foreground)
                })
                .when(!selected, |el| {
                    el.text_color(colors.muted_foreground)
                        .hover(|el| el.bg(colors.muted))
                })
                .child(Icon::new(panel.icon()).size(px(15.)))
                .on_click(cx.listener(move |_this, _event, window, cx| {
                    window.dispatch_action(Box::new(Show { panel }), cx);
                })),
        );
    }

    // The trailing end: the same toggle as the one that floats in the pane area
    // while the sidebar is down, so a click in either place means the same
    // thing. Only one of the two is ever drawn, which is why they share an id.
    row.child(div().flex_1())
        .child(toggle_button(colors, cx))
        .into_any_element()
}

/// The button that shows and hides the whole sidebar.
pub fn toggle_button(colors: &ColorTokens, cx: &mut Context<crate::Workspace>) -> AnyElement {
    div()
        .id("panel-toggle")
        .test_support()
        .aria_label(t!("panel.toggle"))
        .h_flex()
        .items_center()
        .justify_center()
        .size(px(24.))
        .rounded_sm()
        .cursor_pointer()
        .text_color(colors.muted_foreground)
        .hover(|el| el.bg(colors.muted))
        .child(Icon::new(IconName::PanelRight).size(px(15.)))
        .on_click(cx.listener(|_this, _event, window, cx| {
            window.dispatch_action(Box::new(Toggle), cx);
        }))
        .into_any_element()
}
