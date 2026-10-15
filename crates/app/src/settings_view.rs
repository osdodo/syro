//! The settings surface, and the one place the app changes how it looks.

use gpui_kit::base::{
    ColorTokens, Root, StyledExt as _, TestSupportExt as _, Theme, TypographyTokens,
};
use gpui_kit::component::button::Button;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::searchable_list::SearchableListItem;
use gpui_kit::component::select::{Select, SelectEvent, SelectState};
use gpui_kit::component::slider::{Slider, SliderEvent, SliderState};
use gpui_kit::component::{IndexPath, Sizable as _, ThemeMode};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rust_i18n::t;
use syro_core::commit_message;
use syro_core::config::{Appearance, ComputerAwakeMode, Settings};

use crate::i18n;
use crate::state::AppState;

/// The palette `settings.toml` asks for, or the system's when it names none.
fn theme_mode(appearance: Option<Appearance>, cx: &App) -> ThemeMode {
    match appearance {
        Some(Appearance::Light) => ThemeMode::Light,
        Some(Appearance::Dark) => ThemeMode::Dark,
        None => ThemeMode::from(cx.window_appearance()),
    }
}

/// The bounds a font size and a scrollback are kept inside. The low ends are
/// where a terminal stops being readable or useful; the high ends are where a
/// setting stops being a preference and starts being a mistake.
pub const MIN_FONT_SIZE: f32 = 8.;
pub const MAX_FONT_SIZE: f32 = 32.;
pub const MIN_SCROLLBACK: u32 = 1_000;
pub const MAX_SCROLLBACK: u32 = 100_000;
const SCROLLBACK_STEP: u32 = 1_000;

/// The bounds a background opacity is kept inside: a window is solid at one end
/// of the range and see-through at the other, which is the whole of what a
/// transparency setting has to say. Nothing here has a floor, because the
/// glyphs never carry the alpha and floating surfaces keep a readable tint
/// (see [`popup_surface`]) — so a window at the far end is still usable.
pub const MIN_OPACITY: f32 = 0.0;
pub const MAX_OPACITY: f32 = 1.0;
/// One step of the transparency slider, in percent.
const OPACITY_STEP: f32 = 5.;
/// The slider's whole travel: solid at one end, see-straight-through at the
/// other.
const MAX_TRANSPARENCY: f32 = 100.;
/// How many fills the app paints over each other between a panel and the desktop:
/// GPUI Root's fill, and the panel on it. What is behind the window comes
/// through both as `(1 - alpha)²`, so the alpha a slider's percent asks for is not
/// that percent — see [`opacity_at`].
const TRANSPARENCY_FILLS: f32 = 2.;
/// The blur slider's travel, as a radius: past this the desktop is a wash of
/// colour and more radius changes nothing a person can see.
pub const MAX_BLUR: u32 = 50;

/// A font size the app will use. The stepper cannot leave the range, and neither
/// can anything else that sets one: this is the only place the bounds are
/// applied.
fn clamp_font(size: f32) -> f32 {
    size.clamp(MIN_FONT_SIZE, MAX_FONT_SIZE)
}

/// A scrollback the app will use, from a step that may be negative.
fn clamp_scrollback(lines: i64) -> u32 {
    lines.clamp(MIN_SCROLLBACK as i64, MAX_SCROLLBACK as i64) as u32
}

/// An opacity the app will use. The stepper cannot leave the range, and neither
/// can a hand-edited `settings.toml`.
fn clamp_opacity(opacity: f32) -> f32 {
    opacity.clamp(MIN_OPACITY, MAX_OPACITY)
}

/// What the form shows and its slider moves: how much of what is behind the
/// window comes through it. That is what a person means by "50% transparent",
/// and it is not the alpha either fill carries — two fills at `alpha` pass
/// `(1 - alpha)²` of the desktop, which is why a linear alpha felt dead for the
/// first half of the track and violent over the last third.
fn transparency(opacity: f32) -> f32 {
    (1. - opacity).powf(TRANSPARENCY_FILLS)
}

/// The opacity a slider at `percent` of the way along asks for: the alpha that
/// lets `percent` of the desktop through both fills, so half way along the track
/// is half a see-through window.
fn opacity_at(percent: f32) -> f32 {
    let wanted = (percent / 100.).clamp(0., 1.);
    clamp_opacity(1. - wanted.powf(1. / TRANSPARENCY_FILLS))
}

/// Whether a window at `opacity` needs a transparent platform background: the
/// platform knows only opaque or not.
fn is_transparent(opacity: f32) -> bool {
    opacity < MAX_OPACITY
}

/// The platform background for a window opened at `opacity`: frosted when the
/// setting asks for it and there is anything to frost.
pub fn window_background(opacity: f32, blur: u32) -> WindowBackgroundAppearance {
    if !is_transparent(clamp_opacity(opacity)) {
        // A solid window has nothing behind it to blur.
        WindowBackgroundAppearance::Opaque
    } else if cfg!(target_os = "macos") || blur == 0 {
        // On macOS the radius is the window's own, set beside this (see
        // [`set_blur_radius`]): the platform's `Blurred` material has no radius.
        WindowBackgroundAppearance::Transparent
    } else {
        // Elsewhere the platform's material is all there is: blurred or not, and
        // `Transparent` is what one that cannot blur does.
        WindowBackgroundAppearance::Blurred
    }
}

/// Blur what is behind `window` by `radius` points, 0 for none. The window
/// server's own call, which is what Ghostty and kitty use for the same setting:
/// AppKit's public blur is a material with no radius.
#[cfg(target_os = "macos")]
#[allow(unexpected_cfgs)] // objc 0.2's `msg_send!` checks a `cargo-clippy` feature.
fn set_blur_radius(window: &Window, radius: u32) {
    use objc::runtime::Object;
    use objc::{msg_send, sel, sel_impl};
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGSMainConnectionID() -> i32;
        fn CGSSetWindowBackgroundBlurRadius(connection: i32, window: u32, radius: i32) -> i32;
    }

    // Not `window.window_handle()`: gpui has an inherent method of that name.
    let Ok(handle) = HasWindowHandle::window_handle(window) else {
        return;
    };
    // A test window is not an AppKit one, and has nothing to blur.
    let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
        return;
    };
    unsafe {
        let view = handle.ns_view.as_ptr() as *mut Object;
        let ns_window: *mut Object = msg_send![view, window];
        if ns_window.is_null() {
            return;
        }
        let number: isize = msg_send![ns_window, windowNumber];
        CGSSetWindowBackgroundBlurRadius(CGSMainConnectionID(), number as u32, radius as i32);
    }
}

/// Floating surfaces share the window's native blur, with a tint floor for readable text.
// shortcut: GPUI has no per-element backdrop blur; this frosts the desktop, not app content.
pub fn popup_surface(color: Hsla) -> Hsla {
    Hsla {
        a: color.a.clamp(0.94, 1.),
        ..color
    }
}

/// Draw every surface of the app at `opacity`, so what is behind it shows
/// through all of it — floating surfaces keep a tint floor (see [`popup_surface`]) — and
/// the glyphs stay opaque, because the alpha goes on the colors things are drawn
/// *on* and never on the ones drawn *with*.
///
/// A box is drawn *on* a panel rather than beside it, and so is every line: those
/// two are a layer lighter than the panel, which is what the second pass below is
/// for.
///
// shortcut: the fills are the ones this app paints with, so a token from a
// component it does not use keeps its own alpha. Upgrade if a component made of a
// background this list misses turns out to stay solid over a see-through window.
pub fn apply_opacity(cx: &mut App, opacity: f32, blur: u32) {
    let opacity = clamp_opacity(opacity);
    let background = window_background(opacity, blur);
    // A solid window has nothing behind it to blur.
    let blur = if is_transparent(opacity) {
        blur.min(MAX_BLUR)
    } else {
        0
    };
    gpui_kit::component::Theme::update(cx, |theme| {
        // `background` is the whole window, `popover` the app's panels and
        // dialogs, and the rest the fills they are made of. Through
        // `Theme::update`, each one lands in the theme's renderable tokens and
        // in the copy gpui-base paints this app with.
        for colour in [
            &mut theme.colors.background,
            &mut theme.colors.popover,
            &mut theme.colors.accent,
            &mut theme.colors.secondary,
            &mut theme.colors.primary,
            &mut theme.colors.danger,
        ] {
            colour.a = opacity;
        }
        // The fills a box is made of, and every line — a border is a line rather
        // than a fill and it goes with them, because it is what separates two of
        // those panels. These are painted *on* a panel, not over the desktop, so at
        // the panel's own alpha the two add up: a box that should be a tint on the
        // surface reads as a hole cut in it, and the line round it as a hard rule
        // across the desktop — worst at half a see-through window, which is where
        // the setting is used. Squared, they are one layer's worth lighter, that
        // layer being the one the panel is made of (`TRANSPARENCY_FILLS`), so a box
        // and its edge stay as see-through as the panel they sit on.
        for colour in [
            &mut theme.colors.muted,
            &mut theme.colors.input,
            &mut theme.colors.border,
        ] {
            colour.a = opacity * opacity;
        }
        // The editor's surface is the highlight theme's own, not the palette's: the
        // colour the app's themes give it is the window's, but opaque, so the editor
        // is the one pane that has to be told as well. Its current line is a fill of
        // the same kind.
        let highlight = std::sync::Arc::make_mut(&mut theme.highlight_theme);
        for fill in [
            &mut highlight.style.editor_background,
            &mut highlight.style.editor_active_line,
        ] {
            if let Some(fill) = fill {
                fill.a = opacity;
            }
        }
        // The gutter is a second fill of the editor's own background: the base editor
        // lays one over that column so text scrolled sideways cannot reach the line
        // numbers, which at a transparency is the one column of the window a layer
        // denser than the code beside it. Dropped while the window is see-through —
        // the gutter is the same surface as the code, and the numbers say where it is.
        //
        // shortcut: with that fill gone, a line scrolled sideways can show under the
        // numbers. Give the column a fill again if a file is ever opened long and
        // unwrapped.
        if is_transparent(opacity) {
            highlight.style.editor_gutter_background = Some(transparent_black());
        }
    });
    // Deferred: a setting is committed from inside the window's update, and a
    // window that is being updated is not in `App::windows` while it is, so a
    // direct pass would move every window but the one being looked at.
    cx.defer(move |cx| {
        for handle in cx.windows() {
            // Whichever view is inside it, a window's root is gpui-base's.
            let Some(window) = handle.downcast::<Root>() else {
                continue;
            };
            let _ = window.update(cx, |_, window, _| {
                window.set_background_appearance(background);
                #[cfg(target_os = "macos")]
                set_blur_radius(window, blur);
                #[cfg(not(target_os = "macos"))]
                let _ = blur;
            });
        }
    });
}

/// The default scale's line-height-to-size ratio — 20/13 in gpui-kit 0.7.1.
fn line_ratio() -> f32 {
    let mono = TypographyTokens::default().mono_md;
    mono.line_height.as_f32() / mono.size.as_f32()
}

/// Apply the appearance and the terminal's type.
pub fn apply(cx: &mut App, settings: &Settings) {
    // The native chrome (title bar, traffic lights) follows the app's palette, not
    // the system's. First, so `None` has cleared the override before the system's
    // appearance is read below.
    cx.set_window_appearance(settings.appearance.map(|appearance| match appearance {
        Appearance::Light => WindowAppearance::Light,
        Appearance::Dark => WindowAppearance::Dark,
    }));
    gpui_kit::component::Theme::change(theme_mode(settings.appearance, cx), None, cx);

    let size = px(settings.terminal.font_size);
    gpui_kit::component::Theme::update(cx, |theme| theme.mono_font_size = size);

    // Every fill the app paints carries the opacity, so what is behind the window shows through
    // the whole UI and the glyphs on top of it stay opaque.
    apply_opacity(cx, settings.background_opacity, settings.background_blur);

    // The line height is not one of the component theme's own fields — its `typography_tokens`
    // always derives it from the scale's default — so a font size on its own would grow the
    // glyphs and leave the rows the height of the old ones. Last, because the writes above
    // rebuild gpui-base's copy of the theme and this patch lives on that copy alone.
    Theme::global_mut(cx).tokens.typography.mono_md.line_height = size * line_ratio();

    cx.refresh_windows();
}

/// `settings::Open` — the form. Handled by the workspace, because opening a
/// dialog needs a window.
#[derive(Clone, PartialEq, Debug, Action)]
#[action(namespace = settings, no_json)]
pub struct Open;

/// One tab of the form: what the left column lists, and the rows the right side
/// shows while it is the one selected.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Category {
    Appearance,
    Terminal,
    General,
}

impl Category {
    /// In tab order. The first is the one the form opens on: the everyday
    /// settings come first, and how the app looks comes last.
    const ALL: [Category; 3] = [Self::General, Self::Terminal, Self::Appearance];

    fn label(self) -> String {
        match self {
            Self::Appearance => t!("settings.appearance"),
            Self::Terminal => t!("settings.terminal"),
            Self::General => t!("settings.general"),
        }
        .to_string()
    }

    /// The id a test clicks the tab by.
    fn id(self) -> &'static str {
        match self {
            Self::Appearance => "settings-tab-appearance",
            Self::Terminal => "settings-tab-terminal",
            Self::General => "settings-tab-general",
        }
    }
}

/// One row of the language picker: the setting it stands for, and the name it
/// is read by. A language is named in its own language, because a picker that
/// names Chinese in English is a picker for people who already read English;
/// "follow the system" is about the app rather than a language, so it is
/// written in the one the app is currently showing.
#[derive(Clone)]
struct LanguageItem(Option<String>);

impl SearchableListItem for LanguageItem {
    type Value = Option<String>;

    fn title(&self) -> SharedString {
        match self.0.as_deref() {
            None => t!("settings.language_system").to_string().into(),
            Some(i18n::EN) => SharedString::from("English"),
            _ => SharedString::from("简体中文"),
        }
    }

    fn value(&self) -> &Option<String> {
        &self.0
    }

    fn render(&self, _: &mut Window, _: &mut App) -> impl IntoElement {
        roomier(self.title())
    }
}

/// One row of the appearance picker: the palette it stands for, and the name
/// the form gives it. `None` is "follow the system", the same answer the
/// language picker offers.
#[derive(Clone)]
struct AppearanceItem(Option<Appearance>);

impl SearchableListItem for AppearanceItem {
    type Value = Option<Appearance>;

    fn title(&self) -> SharedString {
        match self.0 {
            None => t!("settings.language_system").to_string().into(),
            Some(Appearance::Light) => t!("settings.light").to_string().into(),
            Some(Appearance::Dark) => t!("settings.dark").to_string().into(),
        }
    }

    fn value(&self) -> &Option<Appearance> {
        &self.0
    }

    fn render(&self, _: &mut Window, _: &mut App) -> impl IntoElement {
        roomier(self.title())
    }
}

/// A picker's row, with air above and below it: the component's own row is tight,
/// and a short menu of settings reads better when the answers are not stacked
/// against each other.
fn roomier(title: SharedString) -> impl IntoElement {
    div().py_1().child(title)
}

/// The form.
pub struct SettingsView {
    state: Entity<AppState>,
    /// See `dialogs::NewWorktree::window`: the handle the form was opened in is
    /// what closes it, and `active_window()` is not it.
    window: AnyWindowHandle,
    /// `None` is "follow the system", as with the language.
    appearance: Option<Appearance>,
    /// The appearance picker: a dropdown, like the language one.
    appearance_select: Entity<SelectState<Vec<AppearanceItem>>>,
    /// Which tab's rows the right side shows.
    category: Category,
    /// `None` is "follow the system", which is what a settings file without a
    /// `language` says. Applied on click rather than only on restart: a language
    /// picker that needs a restart is a picker nobody trusts.
    language: Option<String>,
    /// The language picker: a dropdown, because the answer is one of a few and
    /// only one of them can be it.
    language_select: Entity<SelectState<Vec<LanguageItem>>>,
    /// The window's opacity, as the setting spells it; the form shows its
    /// inverse (see [`transparency`]).
    opacity: f32,
    /// How much a see-through window blurs what is behind it, as a radius.
    blur: u32,
    /// The transparency slider. The setting is the opacity and the slider moves
    /// its inverse, so the thumb is at 0% for a solid window.
    opacity_slider: Entity<SliderState>,
    /// The blur slider: the radius, as the setting spells it.
    blur_slider: Entity<SliderState>,
    font_size: f32,
    scrollback: u32,
    /// The command the AI button runs to write a commit message, as typed in the
    /// field. Empty is the default (see [`commit_message::DEFAULT_COMMAND`]).
    command: String,
    command_input: Entity<InputState>,
    _command_events: Subscription,
    /// Whether, and when, the machine is kept from sleeping. The row shows the
    /// same three answers `settings.toml` spells out.
    computer_awake: ComputerAwakeMode,
    /// What the write to `settings.toml` said, when it said anything.
    error: Option<String>,
    _events: Subscription,
    _slider_events: [Subscription; 2],
    _language_events: Subscription,
    _appearance_events: Subscription,
}

impl SettingsView {
    pub fn new(state: Entity<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let terminal = state.read(cx).settings().terminal.clone();
        let language = state.read(cx).settings().language.clone();
        let computer_awake = state.read(cx).settings().computer_awake;
        let command = state
            .read(cx)
            .settings()
            .commit_message_command
            .clone()
            .unwrap_or_default();
        let command_input = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(command.clone())
                .placeholder(commit_message::DEFAULT_COMMAND)
        });
        // Written as it is typed, the way every other row here lands at once: a
        // command is short, and a form that needs a save button is a form that
        // loses an edit.
        let command_events = cx.subscribe(&command_input, |this, input, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                this.command = input.read(cx).value().to_string();
                this.commit(cx);
            }
        });
        let appearance = state.read(cx).settings().appearance;
        let opacity = clamp_opacity(state.read(cx).settings().background_opacity);
        let blur = state.read(cx).settings().background_blur.min(MAX_BLUR);
        let blur_slider = cx.new(|_| {
            SliderState::new()
                .min(0.)
                .max(MAX_BLUR as f32)
                .step(1.)
                .default_value(blur as f32)
        });
        let opacity_slider = cx.new(|_| {
            SliderState::new()
                .min(0.)
                .max(MAX_TRANSPARENCY)
                .step(OPACITY_STEP)
                .default_value(transparency(opacity) * 100.)
        });
        // A drag is a change per frame: it is shown while the thumb moves, and
        // written once the pointer lets go.
        let opacity_events =
            cx.subscribe(&opacity_slider, |this, _slider, event: &SliderEvent, cx| {
                let (percent, done) = slid(event);
                this.opacity = opacity_at(percent);
                this.slide(done, cx);
            });
        let blur_events = cx.subscribe(&blur_slider, |this, _slider, event: &SliderEvent, cx| {
            let (radius, done) = slid(event);
            this.blur = (radius.round().max(0.) as u32).min(MAX_BLUR);
            this.slide(done, cx);
        });
        let items = vec![
            LanguageItem(None),
            LanguageItem(Some(i18n::EN.to_string())),
            LanguageItem(Some(i18n::ZH.to_string())),
        ];
        let selected = items
            .iter()
            .position(|item| item.0 == language)
            .map(|row| IndexPath::default().row(row));
        let language_select = cx.new(|cx| SelectState::new(items, selected, window, cx));
        // A language is picked once and lands at once: the form redraws in it, and
        // `settings.toml` is written in the same step.
        let language_events = cx.subscribe(
            &language_select,
            |this, _select, event: &SelectEvent<Vec<LanguageItem>>, cx| {
                let SelectEvent::Confirm(Some(language)) = event else {
                    return;
                };
                if *language == this.language {
                    return;
                }
                this.language = language.clone();
                i18n::apply(this.language.as_deref());
                this.commit(cx);
                // The rows are written in the language just picked, so both
                // pickers have to redraw their own labels.
                this.language_select.update(cx, |_, cx| cx.notify());
                this.appearance_select.update(cx, |_, cx| cx.notify());
            },
        );
        let items = vec![
            AppearanceItem(None),
            AppearanceItem(Some(Appearance::Light)),
            AppearanceItem(Some(Appearance::Dark)),
        ];
        let selected = items
            .iter()
            .position(|item| item.0 == appearance)
            .map(|row| IndexPath::default().row(row));
        let appearance_select = cx.new(|cx| SelectState::new(items, selected, window, cx));
        let appearance_events = cx.subscribe(
            &appearance_select,
            |this, _select, event: &SelectEvent<Vec<AppearanceItem>>, cx| {
                let SelectEvent::Confirm(Some(appearance)) = event else {
                    return;
                };
                if *appearance == this.appearance {
                    return;
                }
                this.appearance = *appearance;
                this.commit(cx);
            },
        );
        // The dialog is a view of the state, so a change made anywhere else —
        // a `settings.toml` reload, a command — is redrawn rather than fought.
        let events = cx.subscribe(
            &state,
            |this, state, _event: &syro_core::event::CoreEvent, cx| {
                let terminal = state.read(cx).settings().terminal.clone();
                this.font_size = terminal.font_size;
                this.scrollback = terminal.scrollback;
                this.language = state.read(cx).settings().language.clone();
                this.computer_awake = state.read(cx).settings().computer_awake;
                this.appearance = state.read(cx).settings().appearance;
                this.opacity = clamp_opacity(state.read(cx).settings().background_opacity);
                this.blur = state.read(cx).settings().background_blur.min(MAX_BLUR);
                // The sliders are not copies of the rows: a value that arrived from
                // anywhere else has to move the thumbs too, or the two disagree.
                let opacity = transparency(this.opacity) * 100.;
                let blur = this.blur as f32;
                let (opacity_slider, blur_slider) =
                    (this.opacity_slider.clone(), this.blur_slider.clone());
                // The language picker is a copy of the setting too: a change made
                // elsewhere has to be shown in it as well.
                let (language_select, language) =
                    (this.language_select.clone(), this.language.clone());
                let (appearance_select, appearance) =
                    (this.appearance_select.clone(), this.appearance);
                let _ = this.window.update(cx, |_, window, cx| {
                    opacity_slider.update(cx, |s, cx| s.set_value(opacity, window, cx));
                    blur_slider.update(cx, |s, cx| s.set_value(blur, window, cx));
                    language_select.update(cx, |s, cx| s.set_selected_value(&language, window, cx));
                    appearance_select
                        .update(cx, |s, cx| s.set_selected_value(&appearance, window, cx));
                });
                cx.notify();
            },
        );
        Self {
            state,
            window: window.window_handle(),
            appearance,
            appearance_select,
            category: Category::ALL[0],
            language,
            language_select,
            opacity,
            blur,
            opacity_slider,
            blur_slider,
            font_size: clamp_font(terminal.font_size),
            scrollback: clamp_scrollback(terminal.scrollback as i64),
            computer_awake,
            command,
            command_input,
            error: None,
            _events: events,
            _slider_events: [opacity_events, blur_events],
            _language_events: language_events,
            _appearance_events: appearance_events,
            _command_events: command_events,
        }
    }

    /// Everything these fields spell out, as the state and the file keep it.
    fn settings(&self, cx: &App) -> Settings {
        let mut settings = self.state.read(cx).settings().clone();
        settings.terminal.font_size = self.font_size;
        settings.terminal.scrollback = self.scrollback;
        settings.language = self.language.clone();
        settings.appearance = self.appearance;
        settings.background_opacity = self.opacity;
        settings.background_blur = self.blur;
        settings.computer_awake = self.computer_awake;
        // Blank is "use the default", which is what `None` says in the file.
        settings.commit_message_command =
            Some(self.command.clone()).filter(|command| !command.trim().is_empty());
        settings
    }

    /// Draw the change and tell the app, without writing it: a slider drag is a
    /// change per frame, and a `settings.toml` write per frame is a disk write
    /// per frame.
    fn show(&mut self, cx: &mut Context<Self>) {
        let settings = self.settings(cx);
        apply(cx, &settings);
        cx.notify();
    }

    /// Take a change: the app is told, the state is told, and the form redraws.
    fn commit(&mut self, cx: &mut Context<Self>) {
        self.show(cx);
        // Read before the state is updated: a read while it is being updated is
        // a second borrow of it.
        let settings = self.settings(cx);
        let written = self
            .state
            .update(cx, |state, cx| state.set_settings(settings, cx));
        self.error = written
            .err()
            .map(|error| t!("settings.write_failed", error = format!("{error:#}")).to_string());
        cx.notify();
    }

    /// A drag is a change per frame: it is shown while the thumb moves, and
    /// written once the pointer lets go.
    fn slide(&mut self, done: bool, cx: &mut Context<Self>) {
        if done {
            self.commit(cx);
        } else {
            self.show(cx);
        }
    }

    fn step_font(&mut self, by: f32, cx: &mut Context<Self>) {
        self.font_size = clamp_font(self.font_size + by);
        self.commit(cx);
    }

    fn step_scrollback(&mut self, by: i64, cx: &mut Context<Self>) {
        self.scrollback = clamp_scrollback(self.scrollback as i64 + by);
        self.commit(cx);
    }
}

/// Where a slider's thumb is, and whether the pointer has let go of it.
fn slid(event: &SliderEvent) -> (f32, bool) {
    match event {
        SliderEvent::Change(value) => (value.start(), false),
        SliderEvent::Release(value) => (value.start(), true),
    }
}

/// A labelled slider row: the setting, the track, and the value it is at.
fn slider_row(
    id: &str,
    label: String,
    slider: Slider,
    value: String,
    colors: &ColorTokens,
) -> AnyElement {
    div()
        .id(SharedString::from(format!("settings-{id}")))
        .test_support()
        .v_flex()
        .w_full()
        .gap_2()
        .child(
            div()
                .id(SharedString::from(format!("settings-{id}-label")))
                .test_support()
                .text_sm()
                .child(SharedString::from(label.clone())),
        )
        .child(
            div()
                .h_flex()
                .items_center()
                .gap_3()
                .child(
                    div()
                        .id(SharedString::from(format!("settings-{id}-slider")))
                        .test_support()
                        .flex_1()
                        .min_w_0()
                        .child(slider),
                )
                .child(
                    div()
                        .id(SharedString::from(format!("settings-{id}-value")))
                        .test_support()
                        .aria_label(SharedString::from(format!("{label}: {value}")))
                        .w(px(48.))
                        .flex_shrink_0()
                        .text_right()
                        .text_sm()
                        .text_color(colors.muted_foreground)
                        .child(SharedString::from(value)),
                ),
        )
        .into_any_element()
}

/// One labelled row: what the setting is, the value, and the two buttons that move it.
fn stepper(
    id: &str,
    label: &str,
    value: String,
    colors: &ColorTokens,
    minus: AnyElement,
    plus: AnyElement,
) -> AnyElement {
    div()
        .id(SharedString::from(format!("settings-{id}")))
        .test_support()
        .v_flex()
        .w_full()
        .gap_2()
        .child(
            div()
                .id(SharedString::from(format!("settings-{id}-label")))
                .test_support()
                .text_sm()
                .child(SharedString::from(label.to_string())),
        )
        .child(
            div()
                .h_flex()
                .items_center()
                .gap_1()
                .child(minus)
                .child(
                    div()
                        .id(SharedString::from(format!("settings-{id}-value")))
                        .test_support()
                        .aria_label(SharedString::from(format!("{label}: {value}")))
                        .min_w(px(96.))
                        .text_center()
                        .text_sm()
                        .text_color(colors.muted_foreground)
                        .child(SharedString::from(value)),
                )
                .child(plus),
        )
        .into_any_element()
}

impl Render for SettingsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = Theme::global(cx).tokens.colors.clone();

        let mut tabs = div()
            .id("settings-tabs")
            .test_support()
            .v_flex()
            .w(px(152.))
            .h_full()
            .pr_3()
            .pt_1()
            .flex_shrink_0()
            .gap_1()
            // The seam between the tabs and the rows: one dialog, two panes.
            .border_r_1()
            .border_color(colors.border);
        for category in Category::ALL {
            let selected = self.category == category;
            let label = category.label();
            tabs = tabs.child(
                div()
                    .id(category.id())
                    .test_support()
                    .aria_label(SharedString::from(label.clone()))
                    .aria_selected(selected)
                    .h_flex()
                    .items_center()
                    .h(px(36.))
                    .px_3()
                    .rounded_md()
                    .cursor_pointer()
                    .text_sm()
                    // The selected tab is the one the form says it is: the same
                    // fill a selected panel's button carries.
                    .when(selected, |el| {
                        el.bg(colors.muted).text_color(colors.foreground)
                    })
                    .when(!selected, |el| {
                        el.text_color(colors.muted_foreground)
                            .hover(|el| el.bg(colors.muted))
                    })
                    .child(SharedString::from(label))
                    .on_click(cx.listener(move |this, _event, _window, cx| {
                        this.category = category;
                        cx.notify();
                    })),
            );
        }

        let pane = match self.category {
            Category::Appearance => self.appearance_tab(&colors),
            Category::Terminal => self.terminal_tab(&colors, cx),
            Category::General => self.general_tab(&colors, cx),
        };

        let mut form = div()
            .id("settings")
            .test_support()
            .v_flex()
            .size_full()
            .gap_3()
            .text_sm()
            // The tabs are beside the rows, not above them: a category per click
            // beats a scroll. Both panes take the whole height the dialog gives
            // them, so the seam between them runs edge to edge.
            .child(
                div()
                    .id("settings-panes")
                    .test_support()
                    .h_flex()
                    .flex_1()
                    .min_h_0()
                    .gap_6()
                    .child(tabs)
                    .child(
                        // The rows scroll rather than clip: a category with more
                        // of them than fit is a category the dialog's own close
                        // button can still be reached under.
                        div()
                            .id("settings-rows")
                            .test_support()
                            .h_full()
                            .flex_1()
                            .min_h_0()
                            .min_w_0()
                            .child(
                                div().size_full().overflow_y_scrollbar().child(
                                    div()
                                        .id("settings-content")
                                        .test_support()
                                        .v_flex()
                                        .w_full()
                                        .flex_shrink_0()
                                        // A slider's thumb hangs 8px past either end
                                        // of its track, and the rows are drawn under
                                        // the scroller's clip: it needs that much room
                                        // beside the track, or it is cut in half at the
                                        // value whose end it is parked at.
                                        .pl_2()
                                        .pr_3()
                                        .pb_4()
                                        .child(
                                            div()
                                                .mb_4()
                                                .pt_2()
                                                .pb_3()
                                                .border_b_1()
                                                .border_color(colors.border)
                                                .text_lg()
                                                .child(SharedString::from(self.category.label())),
                                        )
                                        .child(pane),
                                ),
                            ),
                    ),
            );

        if let Some(error) = self.error.clone() {
            form = form.child(
                div()
                    .id("settings-error")
                    .test_support()
                    .px_3()
                    .py_2()
                    .rounded_md()
                    .bg(colors.muted)
                    .role(Role::Alert)
                    .aria_label(SharedString::from(error.clone()))
                    .text_xs()
                    .text_color(colors.destructive)
                    .child(SharedString::from(error)),
            );
        }

        // Every change is applied and written the moment it is made, so there is
        // nothing to confirm: the title bar's close button and Escape are the
        // ways out.
        form
    }
}

impl SettingsView {
    /// The rows of the appearance tab: the palette, and how much of the window
    /// it is drawn on.
    fn appearance_tab(&self, colors: &ColorTokens) -> AnyElement {
        let percent = transparency(self.opacity) * 100.;

        div()
            .id("settings-appearance-rows")
            .test_support()
            .v_flex()
            .w_full()
            .gap_6()
            .child(
                div()
                    .id("settings-appearance")
                    .test_support()
                    .v_flex()
                    .gap_2()
                    .child(
                        div()
                            .id("settings-appearance-label")
                            .test_support()
                            .text_sm()
                            .child(t!("settings.appearance")),
                    )
                    .child(
                        // The same fixed width as the language picker, so the two
                        // dropdowns line up.
                        div().w(px(160.)).flex_shrink_0().child(
                            Select::new(&self.appearance_select)
                                .id("settings-appearance-select")
                                .small()
                                .accessibility_label(t!("settings.appearance").to_string())
                                .menu_width(px(160.)),
                        ),
                    ),
            )
            // The slider is the whole travel of the setting: the thumb at 0% is a
            // solid window, at the far end the most transparent the app goes.
            .child(slider_row(
                "opacity",
                t!("settings.opacity").to_string(),
                Slider::new(&self.opacity_slider),
                format!("{percent:.0}%"),
                colors,
            ))
            // Under the transparency, because it is about the same thing: what a
            // see-through window does to what is behind it. Off while the window
            // is solid, because then there is nothing behind it to blur.
            .child(slider_row(
                "blur",
                t!("settings.blur").to_string(),
                Slider::new(&self.blur_slider).disabled(!is_transparent(self.opacity)),
                self.blur.to_string(),
                colors,
            ))
            .into_any_element()
    }

    /// The rows of the terminal tab.
    fn terminal_tab(&self, colors: &ColorTokens, cx: &mut Context<Self>) -> AnyElement {
        let font_minus = Button::new("settings-font-minus")
            .small()
            .outline()
            .label("−")
            .on_click(cx.listener(|this, _, _, cx| this.step_font(-1., cx)))
            .into_any_element();
        let font_plus = Button::new("settings-font-plus")
            .small()
            .outline()
            .label("+")
            .on_click(cx.listener(|this, _, _, cx| this.step_font(1., cx)))
            .into_any_element();
        let scrollback_minus = Button::new("settings-scrollback-minus")
            .small()
            .outline()
            .label("−")
            .on_click(
                cx.listener(|this, _, _, cx| this.step_scrollback(-(SCROLLBACK_STEP as i64), cx)),
            )
            .into_any_element();
        let scrollback_plus = Button::new("settings-scrollback-plus")
            .small()
            .outline()
            .label("+")
            .on_click(
                cx.listener(|this, _, _, cx| this.step_scrollback(SCROLLBACK_STEP as i64, cx)),
            )
            .into_any_element();

        div()
            .id("settings-terminal-rows")
            .test_support()
            .v_flex()
            .w_full()
            .gap_6()
            .child(stepper(
                "font",
                &t!("settings.font_size"),
                format!("{:.0} px", self.font_size),
                colors,
                font_minus,
                font_plus,
            ))
            .child(stepper(
                "scrollback",
                &t!("settings.scrollback"),
                t!("settings.lines", count = self.scrollback).to_string(),
                colors,
                scrollback_minus,
                scrollback_plus,
            ))
            // Said where the setting is, not in a tooltip: both of these are
            // facts about *when* a change lands, and a user who changes the font
            // size and sees nothing happen needs to know why.
            .child(
                div()
                    .text_xs()
                    .text_color(colors.muted_foreground)
                    .child(t!("settings.note")),
            )
            .into_any_element()
    }

    /// The rows of the general tab.
    fn general_tab(&self, colors: &ColorTokens, cx: &mut Context<Self>) -> AnyElement {
        div()
            .id("settings-general-rows")
            .test_support()
            .v_flex()
            .w_full()
            .gap_6()
            .child(
                div()
                    .id("settings-language")
                    .test_support()
                    .v_flex()
                    .gap_2()
                    .child(
                        div()
                            .id("settings-language-label")
                            .test_support()
                            .text_sm()
                            .child(t!("settings.language")),
                    )
                    .child(
                        // A fixed width, so the trigger and the menu line up and
                        // the whole of it is the clickable control.
                        div().w(px(160.)).flex_shrink_0().child(
                            Select::new(&self.language_select)
                                .id("settings-language-select")
                                .small()
                                .accessibility_label(t!("settings.language").to_string())
                                .menu_width(px(160.)),
                        ),
                    ),
            )
            .child(self.commit_message_row(colors))
            .child(self.awake_row(colors, cx))
            .child(self.web_row(colors, cx))
            .into_any_element()
    }

    /// The command the AI button runs: one field, and the diff goes in on its
    /// stdin. Blank is the default, which the placeholder names.
    fn commit_message_row(&self, colors: &ColorTokens) -> AnyElement {
        div()
            .id("settings-commit-message")
            .test_support()
            .v_flex()
            .w_full()
            .pt_4()
            .pb_2()
            .border_t_1()
            .border_color(colors.border)
            .gap_2()
            .child(div().text_sm().child(t!("settings.commit_message_command")))
            .child(
                div()
                    .id("settings-commit-message-input")
                    .test_support()
                    .w_full()
                    .child(Input::new(&self.command_input).focus_bordered(false)),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(colors.muted_foreground)
                    .child(t!(
                        "settings.commit_message_command_hint",
                        default = commit_message::DEFAULT_COMMAND
                    )),
            )
            .into_any_element()
    }

    /// The phone panel's address, with a copy button. Otherwise the URL exists
    /// only in the log the app was launched from, which is not where anyone looks
    /// for it.
    ///
    /// The switch and the port stay in `settings.toml`: both are read once, when
    /// the listener is bound, so a control here would be a control that lies
    /// until the next launch.
    fn web_row(&self, colors: &ColorTokens, cx: &App) -> AnyElement {
        let url = self.state.read(cx).web_url().map(str::to_string);
        let shown = url
            .clone()
            .unwrap_or_else(|| t!("settings.web_off").to_string());
        let mut row = div()
            .id("settings-web")
            .test_support()
            .h_flex()
            .items_center()
            .gap_3()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .v_flex()
                    .child(div().text_sm().child(t!("settings.web")))
                    .child(
                        div()
                            .id("settings-web-url")
                            .test_support()
                            // The URL is the whole point of the row, and a
                            // screen reader can read it out even when the panel
                            // is too narrow to show it.
                            .aria_label(SharedString::from(shown.clone()))
                            .text_xs()
                            .text_color(colors.muted_foreground)
                            .text_ellipsis()
                            .child(SharedString::from(shown)),
                    ),
            );
        if let Some(url) = url {
            row = row.child(
                Button::new("settings-web-copy")
                    .small()
                    .outline()
                    .label(t!("settings.web_copy"))
                    .on_click(move |_, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(url.clone()))
                    }),
            );
        }
        row.into_any_element()
    }

    /// The sleep setting: a radio group, because exactly one of the three answers
    /// is true. The answer in effect says so below the heading, so `Agent` with
    /// no agent running — the same setting, just not acting — does not read as
    /// broken.
    fn awake_row(&self, colors: &ColorTokens, cx: &mut Context<Self>) -> AnyElement {
        let options: [(&str, ComputerAwakeMode, String, String); 3] = [
            (
                "settings-computer-awake-on",
                ComputerAwakeMode::On,
                t!("settings.computer_awake_on").to_string(),
                t!("settings.computer_awake_on_desc").to_string(),
            ),
            (
                "settings-computer-awake-auto",
                ComputerAwakeMode::Auto,
                t!("settings.computer_awake_auto").to_string(),
                t!("settings.computer_awake_auto_desc").to_string(),
            ),
            (
                "settings-computer-awake-off",
                ComputerAwakeMode::Off,
                t!("settings.computer_awake_off").to_string(),
                t!("settings.computer_awake_off_desc").to_string(),
            ),
        ];

        let active = self.state.read(cx).computer_awake_active();
        let mode_label = options
            .iter()
            .find(|(_, mode, ..)| *mode == self.computer_awake)
            .map(|(_, _, label, _)| label.clone())
            .unwrap_or_default();
        let state_label = if active {
            t!("settings.computer_awake_active", mode = mode_label).to_string()
        } else {
            mode_label
        };

        let mut radios = Vec::new();
        for (id, mode, label, description) in options {
            let chosen = self.computer_awake == mode;
            radios.push(
                div()
                    .id(id)
                    .test_support()
                    .aria_label(SharedString::from(format!("{label}: {description}")))
                    .aria_selected(chosen)
                    .h_flex()
                    .items_start()
                    .gap_3()
                    .px_3()
                    .py_2()
                    .rounded_md()
                    .cursor_pointer()
                    .when(chosen, |el| el.bg(colors.muted))
                    .when(!chosen, |el| el.hover(|el| el.bg(colors.muted)))
                    .child(
                        // The dot: filled for the answer, an empty ring for the
                        // other two.
                        div()
                            .mt(px(3.))
                            .size(px(12.))
                            .flex_shrink_0()
                            .rounded_full()
                            .border_1()
                            .border_color(if chosen {
                                colors.primary
                            } else {
                                colors.border
                            })
                            .when(chosen, |el| el.bg(colors.primary)),
                    )
                    .child(
                        div()
                            .v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_1()
                            .child(div().text_sm().child(SharedString::from(label)))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(colors.muted_foreground)
                                    .child(SharedString::from(description)),
                            ),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.computer_awake = mode;
                        this.commit(cx);
                    })),
            );
        }

        div()
            .id("settings-computer-awake")
            .test_support()
            .v_flex()
            .w_full()
            .pt_4()
            .border_t_1()
            .border_color(colors.border)
            .gap_2()
            .child(
                div()
                    .id("settings-computer-awake-header")
                    .test_support()
                    .v_flex()
                    .gap_2()
                    .child(div().text_sm().child(t!("settings.computer_awake")))
                    .child(
                        div()
                            .id("settings-computer-awake-state")
                            .test_support()
                            .aria_label(SharedString::from(state_label.clone()))
                            .text_sm()
                            .text_color(colors.muted_foreground)
                            .child(SharedString::from(state_label)),
                    ),
            )
            .children(radios)
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    // Not `use super::*`: the module above globs gpui in, and gpui's `test`
    // attribute comes with it — under `cfg(test)` it shadows the built-in one.
    use super::{
        MAX_BLUR, MAX_FONT_SIZE, MAX_OPACITY, MAX_SCROLLBACK, MAX_TRANSPARENCY, MIN_FONT_SIZE,
        MIN_OPACITY, MIN_SCROLLBACK, OPACITY_STEP, apply_opacity, clamp_font, clamp_opacity,
        clamp_scrollback, is_transparent, line_ratio, opacity_at, popup_surface, transparency,
        window_background,
    };
    use gpui_kit::WindowBackgroundAppearance;

    #[test]
    fn floating_surfaces_keep_a_readable_tint() {
        let color = gpui_kit::hsla(0.6, 0.2, 0.3, 1.);
        assert_eq!(popup_surface(color).a, 1.);
        for alpha in [0., 0.4, 0.9] {
            let surface = popup_surface(gpui_kit::Hsla { a: alpha, ..color });
            assert_eq!(surface.a, 0.94);
            assert_eq!(
                (surface.h, surface.s, surface.l),
                (color.h, color.s, color.l)
            );
        }
    }

    /// A box is drawn on a panel rather than beside it, so at the panel's own
    /// alpha the two would add up and the box would read as a hole cut in the
    /// surface. Its fill and the line round it are a layer lighter instead.
    #[gpui_kit::test]
    fn a_box_on_a_panel_is_a_layer_lighter(cx: &mut gpui_kit::TestAppContext) {
        cx.update(gpui_kit::init);
        cx.update(|cx| apply_opacity(cx, 0.4, 0));

        let colors = cx.update(|cx| gpui_kit::base::Theme::global(cx).tokens.colors);
        let panel = colors.surface.a;
        assert_eq!(panel, 0.4, "the panel is not at the window's opacity");
        for (what, lighter) in [
            ("a box's fill", colors.muted.a),
            ("an input's fill", colors.input.a),
            ("a line", colors.border.a),
        ] {
            assert_eq!(lighter, panel * panel, "{what} is not a layer lighter");
            assert!(lighter < panel, "{what} is as dense as the panel it is on");
        }
    }

    #[test]
    fn the_line_ratio_comes_from_the_scale_rather_than_a_guess() {
        // gpui-kit 0.7.1's mono scale is 13px on 20px. If that changes, the rows
        // follow it instead of drifting 30% out.
        let ratio = line_ratio();
        assert!(
            (1.4..=1.7).contains(&ratio),
            "the mono scale's ratio moved: {ratio}"
        );
    }

    /// The opacity is bounded, the form shows its inverse, and only a bounded
    /// value below the top of the range makes the window transparent.
    #[test]
    fn the_opacity_is_bounded_and_the_form_shows_its_inverse() {
        assert_eq!(clamp_opacity(0.0), MIN_OPACITY);
        assert_eq!(clamp_opacity(2.0), MAX_OPACITY);
        assert_eq!(clamp_opacity(0.75), 0.75);

        // A solid window is 0% transparent, the floor is the rest of it.
        assert_eq!(transparency(MAX_OPACITY), 0.0);
        let most = transparency(MIN_OPACITY);
        assert!(
            (most - (MAX_OPACITY - MIN_OPACITY)).abs() < 1e-6,
            "the floor is not the most transparent a window gets: {most}"
        );

        assert!(!is_transparent(MAX_OPACITY));
        assert!(is_transparent(MAX_OPACITY - OPACITY_STEP / 100.));
    }

    /// Where the slider is is the opacity it asks for, and its whole travel is
    /// usable: the far end is the most transparent a window gets, and no part of
    /// it is a value the clamp would throw away.
    #[test]
    fn the_slider_travel_is_the_opacity_range() {
        assert_eq!(MAX_TRANSPARENCY, 100.);
        assert_eq!(opacity_at(0.), MAX_OPACITY);
        assert_eq!(opacity_at(MAX_TRANSPARENCY), MIN_OPACITY);
        // Past either end is the bound, not a hole in the range.
        assert_eq!(opacity_at(-20.), MAX_OPACITY);
        assert_eq!(opacity_at(120.), MIN_OPACITY);
        // Where the thumb is is the number the row shows, both ways round.
        for percent in [0., 5., 30., 50., 70., 100.] {
            let back = transparency(opacity_at(percent)) * 100.;
            assert!(
                (back - percent).abs() < 0.01,
                "{percent}% came back as {back}%"
            );
        }
    }

    /// The number on the row is what the window does to what is behind it, not
    /// the alpha each of the two fills carries — and the two differ, which is the
    /// whole point: a linear alpha felt dead for the first half of the track.
    #[test]
    fn the_transparency_is_what_the_window_lets_through() {
        // A solid window lets nothing through, and the far end lets all of it.
        assert_eq!(transparency(MAX_OPACITY), 0.);
        assert_eq!(transparency(MIN_OPACITY), 1.);
        // Half way along is half a see-through window, and the alpha that does it
        // is well under half: two fills of 0.29 pass 0.5 of the desktop.
        let half = opacity_at(50.);
        assert!(
            (half - 0.293).abs() < 0.001,
            "half way along is not half see-through: {half}"
        );
        assert!((transparency(half) - 0.5).abs() < 1e-6);
        // And it climbs the whole way, rather than sitting still for half the
        // track and then lurching.
        let mut last = 0.;
        for percent in [10., 20., 30., 40., 50., 60., 70., 80., 90., 100.] {
            let through = transparency(opacity_at(percent));
            assert!(through > last, "{percent}% did not add transparency");
            last = through;
        }
    }

    /// A see-through window frosts what is behind it only when the setting asks
    /// for it, and a solid one has nothing to frost.
    #[test]
    fn the_window_is_frosted_only_when_it_is_see_through_and_asked_to_be() {
        assert_eq!(
            window_background(MAX_OPACITY, MAX_BLUR),
            WindowBackgroundAppearance::Opaque
        );
        assert_eq!(
            window_background(0.6, 0),
            WindowBackgroundAppearance::Transparent
        );
        // macOS blurs by radius on a transparent window; elsewhere the platform's
        // material is the blur.
        let blurred = if cfg!(target_os = "macos") {
            WindowBackgroundAppearance::Transparent
        } else {
            WindowBackgroundAppearance::Blurred
        };
        assert_eq!(window_background(0.6, 20), blurred);
    }

    #[test]
    fn a_step_stops_at_the_bounds() {
        assert_eq!(clamp_font(MIN_FONT_SIZE - 1.), MIN_FONT_SIZE);
        assert_eq!(clamp_font(MAX_FONT_SIZE + 1.), MAX_FONT_SIZE);
        assert_eq!(clamp_font(14.), 14.);
        // A negative step is the same clamp from the other side.
        assert_eq!(clamp_scrollback(i64::MIN), MIN_SCROLLBACK);
        assert_eq!(clamp_scrollback(i64::MAX), MAX_SCROLLBACK);
    }
}
