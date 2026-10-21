//! The logos a terminal is drawn with: the agent preset's brand mark, or the
//! shell's own terminal glyph.
//!
//! Two kinds, because a brand logo is not one colour. A coloured logo is served
//! as an asset and rasterized by `img()` with its own fills; a one-colour mark is
//! tinted through `Icon` like every other glyph in the app. GPUI's `Icon` draws
//! every SVG as a single-coloured mask, so colour has to go through `img()`.

use gpui_kit::component::Icon;
use gpui_kit::{AnyElement, Hsla, IntoElement as _, Styled as _, img, px};

/// A terminal's mark.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Logo {
    /// A brand-coloured logo, drawn as the image it is; the path is one
    /// [`AppAssets`] serves. Not every brand has a colour: several are black on
    /// white, and those are [`Logo::Mono`] instead.
    Color(&'static str),
    /// A one-colour mark — a monochrome brand, or the shell's glyph — tinted like
    /// every other icon so it reads on both themes.
    Mono(&'static [u8]),
}

/// The logo for a preset id, or `None` for an agent with no mark of its own — a
/// user-defined preset, or one this table does not know — which is then named in
/// words.
pub fn agent_logo(id: &str) -> Option<Logo> {
    match id {
        // Pi's brand is the letter it titles its terminal with, drawn by hand.
        "pi" => Some(Logo::Mono(include_bytes!("../assets/logos/pi.svg"))),
        "claude" => Some(Logo::Color("logos/claude.svg")),
        "codex" => Some(Logo::Color("logos/codex.svg")),
        // OpenCode's mark is black with no colour of its own, so it follows the
        // theme rather than disappearing into one.
        "opencode" => Some(Logo::Mono(include_bytes!("../assets/logos/opencode.svg"))),
        "gemini" => Some(Logo::Color("logos/gemini.svg")),
        "qoder" => Some(Logo::Color("logos/qoder.svg")),
        _ => None,
    }
}

/// The shell's own glyph: a terminal that runs no agent is still a terminal.
pub const TERMINAL_LOGO: Logo = Logo::Mono(include_bytes!("../assets/logos/terminal.svg"));

/// A logo as both readouts draw it: one size and no shrinking, so a tab's mark
/// and a sidebar row's mark are the same mark. `color` tints a monochrome mark
/// and is ignored by a coloured one.
pub fn logo_icon(logo: Logo, color: Hsla) -> AnyElement {
    match logo {
        Logo::Color(path) => img(path)
            .w(px(12.))
            .h(px(12.))
            .flex_shrink_0()
            .into_any_element(),
        Logo::Mono(svg) => Icon::default()
            .data(svg)
            .size(px(12.))
            .flex_shrink_0()
            .text_color(color)
            .into_any_element(),
    }
}

/// The bytes the app's asset source serves for the paths `img()` asks it for, or
/// `None` for every other path.
fn agent_logo_asset(path: &str) -> Option<&'static [u8]> {
    match path {
        "logos/pi.svg" => Some(include_bytes!("../assets/logos/pi.svg")),
        "logos/claude.svg" => Some(include_bytes!("../assets/logos/claude.svg")),
        "logos/codex.svg" => Some(include_bytes!("../assets/logos/codex.svg")),
        "logos/opencode.svg" => Some(include_bytes!("../assets/logos/opencode.svg")),
        "logos/gemini.svg" => Some(include_bytes!("../assets/logos/gemini.svg")),
        "logos/qoder.svg" => Some(include_bytes!("../assets/logos/qoder.svg")),
        "logos/terminal.svg" => Some(include_bytes!("../assets/logos/terminal.svg")),
        _ => None,
    }
}

/// The app's asset source: gpui-kit's icon catalogue, plus the agent logos under
/// `logos/`, which `img()` loads and rasterizes in colour.
#[derive(Clone, Copy, Debug, Default)]
pub struct AppAssets;

impl gpui_kit::AssetSource for AppAssets {
    fn load(&self, path: &str) -> gpui_kit::Result<Option<std::borrow::Cow<'static, [u8]>>> {
        if let Some(bytes) = agent_logo_asset(path) {
            return Ok(Some(std::borrow::Cow::Borrowed(bytes)));
        }
        gpui_kit::assets::Assets.load(path)
    }

    fn list(&self, path: &str) -> gpui_kit::Result<Vec<gpui_kit::SharedString>> {
        gpui_kit::assets::Assets.list(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::AssetSource as _;

    /// Every logo is a whole SVG; a `include_bytes!` pointing at the wrong file
    /// fails here rather than as a blank square in the tab bar.
    #[test]
    fn every_logo_is_a_whole_svg() {
        for id in ["pi", "claude", "codex", "opencode", "gemini", "qoder"] {
            let logo = agent_logo(id).unwrap_or_else(|| panic!("{id} has no logo"));
            let svg = match logo {
                Logo::Color(path) => {
                    String::from_utf8_lossy(agent_logo_asset(path).unwrap()).into_owned()
                }
                Logo::Mono(bytes) => String::from_utf8_lossy(bytes).into_owned(),
            };
            assert!(svg.starts_with("<svg"), "{id} does not start an SVG");
            assert!(svg.contains("</svg>"), "{id} is not a whole SVG");
        }
        assert_eq!(agent_logo("aider"), None);
    }

    /// Every builtin preset has a mark, so a terminal running one is never named
    /// in bare words while its sibling shows a logo.
    #[test]
    fn every_builtin_agent_has_a_logo() {
        for agent in syro_core::agent::builtin_agents() {
            assert!(
                agent_logo(agent.id.as_str()).is_some(),
                "{} has no logo",
                agent.id
            );
        }
    }

    /// The asset source serves every path a logo names, and still serves the
    /// icons the chrome draws.
    #[test]
    fn the_asset_source_serves_the_logos_and_the_icons() {
        use gpui_kit::assets::IconNamed as _;

        for id in ["pi", "claude", "codex", "opencode", "gemini", "qoder"] {
            if let Some(Logo::Color(path)) = agent_logo(id) {
                assert!(
                    AppAssets.load(path).is_ok(),
                    "{id} names {path}, which the asset source does not serve"
                );
            }
        }
        assert!(AppAssets.load("logos/terminal.svg").is_ok());
        assert!(
            AppAssets
                .load(&gpui_kit::component::IconName::Plus.path())
                .is_ok()
        );
    }
}
