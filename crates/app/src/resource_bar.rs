//! The resource readout: the app's own cost on the strip across the bottom of
//! the window, what each worktree costs, which the sidebar draws on its rows, and
//! what the coding agents have spent lately, which is the strip's usage chip.

use std::time::Instant;

use gpui_kit::FocusHandle;
use gpui_kit::Hsla;
use gpui_kit::base::{ColorTokens, StyledExt as _, TestSupportExt as _, Theme};
use gpui_kit::component::chart::PieChart;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{ActiveTheme as _, Icon, IconName};
use gpui_kit::*;
use rust_i18n::t;
use syro_core::model::AgentStatus;
use syro_core::resource::{ProcessTable, ResourceSnapshot};
use syro_core::session::{self, Cache, DEFAULT_LIMIT, SessionRoots, USAGE_DAYS, UsageSummary};

use crate::usage_chart::{DaySpend, ModelBars, ModelSpend};

/// How often the readout re-reads the process table. Five seconds is a number
/// that is still true enough to glance at; the sweep it saves is the app's own
/// CPU.
const REFRESH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

/// How often it re-reads the agents' transcripts. The first sweep is the one that
/// costs something — it folds every recent transcript — and every sweep after it
/// resumes from each file's own offset, so it reads only what was appended.
const USAGE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

/// The strip's height.
const BAR_HEIGHT: f32 = 24.;

/// The panel's own measurements: its padding, the height of the title row above the
/// scrolling body, the headline under it, the gap between any two of its rows, and how
/// far it stands off the window's leading edge.
const PANEL_PADDING: f32 = 16.;
const PANEL_TITLE: f32 = 24.;
const PANEL_HEADLINE: f32 = 52.;
const PANEL_GAP: f32 = 12.;
const PANEL_INSET: f32 = 44.;

/// One legend row, and the heading above the first of them: the only part of the panel
/// that grows with the data, and so the only part its height is worked out from.
const LEGEND_ROW: f32 = 28.;
const LEGEND_HEAD: f32 = 28.;

/// How far the panel's foot stands above the window's own bottom edge: the status strip
/// plus the clearance the panel keeps over it, so the two do not read as one bar.
const PANEL_FOOT: f32 = BAR_HEIGHT + 48.;

/// The least room the panel leaves above its own top, so a long list is stopped before
/// it runs off the window rather than after.
const PANEL_HEADROOM: f32 = 12.;

/// The columns the legend's numbers sit in, at the row's trailing end — the name takes
/// whatever is left, so the numbers are flush with the panel's edge rather than stopping
/// short of it.
const LEGEND_TOKENS: f32 = 78.;
const LEGEND_COST: f32 = 72.;
const LEGEND_PERCENT: f32 = 40.;

/// One live terminal, as the readout needs it: which worktree it is in, what to
/// call it, the pid whose process tree is its cost, and what its agent is doing.
#[derive(Debug, Clone, PartialEq)]
pub struct LiveTerminal {
    /// The worktree, by id — a path, and the only part of this that is unique:
    /// two repositories can both have a worktree on `main`.
    pub worktree: String,
    /// What the row is called: the agent's name (`Pi - fix the build`), or a shell's own title.
    pub title: String,
    /// The child's pid, or `None` once it is gone: there is nothing left to
    /// measure, and the row stays with how it went rather than disappearing.
    pub pid: Option<u32>,
    /// The two ends of the length its row shows — the agent's current or last
    /// stretch of work — counted when the row is drawn. `None` draws no length.
    pub span: Option<(Instant, Option<Instant>)>,
    /// How the child ended, if it has — `Some(true)` is a clean exit, which is the
    /// difference between the row's `Done` and its `Failed`.
    pub exit_ok: Option<bool>,
    pub status: Option<AgentStatus>,
    /// Said it was working and has gone quiet (`syro_core::agent::stalled`).
    pub stalled: bool,
}

/// One terminal under a worktree, as a row shows it.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    /// What the row is called: the agent's name, or a shell's own title.
    pub title: String,
    pub status: Option<AgentStatus>,
    pub stalled: bool,
    /// How the child ended, if it has.
    pub exit: Option<bool>,
    /// Something is running in front of this terminal's own child, in its tty: a
    /// shell with a job in it. What the row says `Running` for when nothing else
    /// knows what the job is.
    pub busy: bool,
    /// How long it has been working, or worked; `None` before it has.
    pub uptime: Option<std::time::Duration>,
    pub cpu_pct: f32,
    pub rss_bytes: u64,
}

/// What one worktree's terminals cost, as the last sweep measured them.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WorktreeUsage {
    pub cpu_pct: f32,
    pub rss_bytes: u64,
    /// One entry per terminal in the worktree, in the order the workspace listed
    /// them. Empty is a worktree with nothing running in it, which is the
    /// difference between no numbers and zeroes.
    pub terminals: Vec<Row>,
}

/// The strip across the bottom of the window.
pub struct ResourceBar {
    /// Pushed in by the workspace.
    terminals: Vec<LiveTerminal>,
    /// The last good sweep. `None` until the first one lands, and after one
    /// that failed.
    snapshot: Option<ResourceSnapshot>,
    /// Why the last sweep failed, if it did.
    error: Option<String>,
    /// Where the transcripts are read from. A field rather than a global, so a
    /// test can point it at a fixture home — the shape the session panel has too.
    roots: SessionRoots,
    /// Carried between sweeps: what each transcript has already told us.
    usage_cache: Cache,
    /// What the agents have spent lately. `None` until the first usage sweep lands.
    usage: Option<UsageSummary>,
    /// Whether the usage panel is open.
    chart_open: bool,
    /// How many times it has been opened: the charts' appear key, so a panel that
    /// is opened again draws its data in again rather than sitting there finished.
    charts_opened: u64,
    /// When the transcripts were last read: the poll ticks faster than the read.
    swept_at: Option<Instant>,
    /// The poll loop. Held so it stops with the view.
    _poll: Task<()>,
    /// The usage sweep in flight. One at a time: two readers would fold into two
    /// caches and the slower one would win.
    _sweep: Option<Task<()>>,
    /// Held while the panel is open, so Escape reaches the scrim: a view's keys only
    /// arrive when something of the view's has the window's focus. `None` in a bar
    /// built outside a context, which the logic tests do.
    focus: Option<FocusHandle>,
}

impl ResourceBar {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let poll = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(REFRESH_INTERVAL).await;
                // Nothing to measure while nobody is looking: a background window spending a `ps` a second is a
                // process fork the user is not paying for, and in a test there is no active
                // window at all — which is what keeps the suite from forking `ps` in every
                // window test.
                let active = cx.update(|cx| cx.active_window().is_some());
                if !active {
                    continue;
                }
                if this
                    .update(cx, |bar, cx| {
                        bar.refresh(cx);
                        bar.sweep_usage(cx);
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        let mut bar = Self {
            terminals: Vec::new(),
            snapshot: None,
            error: None,
            roots: SessionRoots::default(),
            usage_cache: Cache::default(),
            usage: None,
            chart_open: false,
            charts_opened: 0,
            swept_at: None,
            _poll: poll,
            _sweep: None,
            focus: Some(cx.focus_handle()),
        };
        bar.refresh(cx);
        bar
    }

    /// Read the agents' transcripts again, off the UI thread, and keep what they have
    /// spent lately. Reading a few hundred transcripts is not a frame's work, so it
    /// happens on the background executor and the strip redraws when it lands.
    fn sweep_usage(&mut self, cx: &mut Context<Self>) {
        if self._sweep.is_some()
            || self
                .swept_at
                .is_some_and(|at| at.elapsed() < USAGE_INTERVAL)
        {
            return;
        }
        self.swept_at = Some(Instant::now());
        let roots = self.roots.clone();
        let mut cache = std::mem::take(&mut self.usage_cache);
        let task = cx.spawn(async move |this, cx| {
            let (usage, cache) = cx
                .background_executor()
                .spawn(async move {
                    let scan = session::scan(&roots, &mut cache, DEFAULT_LIMIT);
                    (scan.usage, cache)
                })
                .await;
            this.update(cx, |bar, cx| {
                bar.usage_cache = cache;
                bar.usage = Some(usage);
                bar._sweep = None;
                cx.notify();
            })
            .ok();
        });
        self._sweep = Some(task);
    }

    /// The live terminals, as the workspace sees them. Called where the set
    /// changes rather than every frame: an unchanged list is not a repaint, and
    /// a sweep belongs in an event.
    pub fn set_terminals(&mut self, terminals: Vec<LiveTerminal>, cx: &mut Context<Self>) {
        if self.terminals == terminals {
            return;
        }
        self.terminals = terminals;
        // A terminal that just exited is gone from the next sweep; one that just started should show up
        // now rather than a second from now.
        let this = cx.entity();
        cx.defer(move |cx| {
            let _ = this.update(cx, |bar, cx| bar.refresh(cx));
        });
    }

    /// One sweep, on the UI thread.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        // A terminal whose child is gone has nothing to measure. `u32::MAX` is not
        // a pid `ps` reports, so its tree comes back empty and it costs nothing —
        // while its row stays, because the row is not the cost.
        let pids: Vec<u32> = self
            .terminals
            .iter()
            .map(|terminal| terminal.pid.unwrap_or(u32::MAX))
            .collect();
        match ProcessTable::sample() {
            Ok(table) => {
                let snapshot = ResourceSnapshot::of(&table, std::process::id(), &pids);
                self.snapshot = Some(snapshot);
                self.error = None;
            }
            Err(error) => {
                self.error = Some(format!("{error}"));
                self.snapshot = None;
            }
        }
        cx.notify();
    }

    /// What one worktree's terminals cost, and the terminals that make it up, as
    /// the last sweep measured them. `None` before the first sweep; a worktree
    /// with nothing running in it is `Some` with no terminals, which is the
    /// difference between a row with no numbers and a row of zeroes.
    pub fn worktree_usage(&self, worktree_id: &str) -> Option<WorktreeUsage> {
        let snapshot = self.snapshot.as_ref()?;
        let mut usage = WorktreeUsage::default();
        for (index, terminal) in self.terminals.iter().enumerate() {
            if terminal.worktree != worktree_id {
                continue;
            }
            // The snapshot's entries are in the order this list was handed over,
            // so one terminal's cost is at that terminal's own index.
            let entry = snapshot.terminals.get(index);
            let cost = entry.map(|entry| entry.usage).unwrap_or_default();
            usage.cpu_pct += cost.cpu_pct;
            usage.rss_bytes += cost.rss_bytes;
            // Counted as the row is drawn — this is called from a render — so a
            // live terminal's length grows on the readout's own tick and a
            // finished one's stops where its child did.
            usage.terminals.push(Row {
                title: terminal.title.clone(),
                status: terminal.status,
                stalled: terminal.stalled,
                exit: terminal.exit_ok,
                busy: entry.is_some_and(|entry| entry.busy),
                uptime: terminal.span.map(|(start, end)| {
                    end.unwrap_or_else(Instant::now)
                        .saturating_duration_since(start)
                }),
                cpu_pct: cost.cpu_pct,
                rss_bytes: cost.rss_bytes,
            });
        }
        Some(usage)
    }

    /// The line a screen reader reads and a test asserts on.
    fn summary(&self) -> String {
        match (&self.error, &self.snapshot) {
            (Some(_), _) => t!("resource.unavailable").to_string(),
            (None, Some(snapshot)) => {
                t!("resource.app", count = format_bytes(snapshot.app.rss_bytes)).to_string()
            }
            (None, None) => t!("resource.measuring").to_string(),
        }
    }

    /// What the usage chip says: the window's tokens and price, or that there is
    /// nothing to say yet. A dash rather than a zero, which would read as "you
    /// spent nothing" — the one thing a strip that has not read anything cannot know.
    fn usage_text(&self) -> String {
        match &self.usage {
            None => t!("resource.usage_measuring").to_string(),
            Some(usage) if usage.total.tokens == 0 => t!("resource.usage_none").to_string(),
            Some(usage) => t!(
                "resource.usage",
                days = USAGE_DAYS,
                tokens = format_tokens(usage.total.tokens),
                cost = format!("${:.2}", usage.total.cost),
            )
            .to_string(),
        }
    }

    /// The same numbers as a sentence, for the chip's aria label.
    fn usage_label(&self) -> String {
        match &self.usage {
            Some(usage) if usage.total.tokens > 0 => t!(
                "resource.usage_aria",
                days = USAGE_DAYS,
                tokens = format_tokens(usage.total.tokens),
                cost = format!("${:.2}", usage.total.cost),
            )
            .to_string(),
            // Measuring, or nothing recorded: the short form already reads as one.
            _ => self.usage_text(),
        }
    }

    /// The usage chip: what the agents have spent lately, and the way into the
    /// detail. The arrow points up while the detail is closed and down while it is
    /// open, which is the only thing the strip can say about a panel this size.
    fn usage_chip(&self, colors: &ColorTokens, cx: &mut Context<Self>) -> AnyElement {
        div()
            .id("usage-open")
            .test_support()
            .aria_label(self.usage_label())
            .h_flex()
            .items_center()
            .gap_1()
            .px_1()
            .rounded_sm()
            .cursor_pointer()
            .hover(|el| el.bg(colors.muted))
            .on_click(cx.listener(|bar, _event, window, cx| bar.open_chart(window, cx)))
            .child(Icon::new(IconName::ChartPie).size(px(12.)))
            .child(SharedString::from(self.usage_text()))
            .child(
                Icon::new(if self.chart_open {
                    IconName::ChevronDown
                } else {
                    IconName::ChevronUp
                })
                .size(px(11.)),
            )
            .into_any_element()
    }

    /// Open the usage detail: the panel the strip's chip drops down, centred over the
    /// window.
    ///
    /// Not `window.open_dialog`: a dialog's own surface is painted in a layer above the
    /// deferred pass its content draws in, so a chart's tooltip — which a chart draws
    /// deferred, so it can overflow the plot — ends up behind the card it belongs to.
    fn open_chart(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // One panel at a time: the strip stays clickable while the panel is open, and a
        // second copy stacked over the first is nobody's idea of a readout.
        if self.chart_open {
            return;
        }
        self.chart_open = true;
        self.charts_opened += 1;
        // Escape is the scrim's to handle, and it only sees keys while it is focused:
        // the terminal would otherwise take the key for its own prompt.
        if let Some(focus) = self.focus.clone() {
            window.focus(&focus, cx);
        }
        cx.notify();
    }

    /// The detail's overlay: a scrim that closes it on a click, with the card centred
    /// on it. Deferred, so it paints over the whole window rather than inside the
    /// strip, which clips what it holds.
    ///
    /// Not `window.open_dialog`: a dialog's own surface paints in a layer above the
    /// deferred pass its content draws in, so a chart's tooltip — which a chart defers
    /// so it can overflow the plot — would end up behind the card it belongs to.
    fn chart_overlay(&self, window: Size<Pixels>, cx: &mut Context<Self>) -> AnyElement {
        let colors = Theme::global(cx).tokens.colors.clone();
        let strip = cx.entity().downgrade();
        // The panel's own size, worked out rather than negotiated: every piece of it is
        // a number this side picks. A strip down the window's leading side — tall and
        // narrow, so the charts stay dense — with the model list running past the bottom,
        // where scrolling picks it up. The floor is the legend's own width: below it the
        // four columns would be the thing that overflowed.
        // The two charts have heights of their own; the list below them is what scrolls.
        let days_height = px(220.);
        let share_height = px(200.);
        // Only the list grows with the data, so the panel is as tall as the list makes it
        // — and no taller, or a short list would leave half the panel blank. The window
        // caps it, and what the cap cuts off is scrolled to.
        let models = self
            .usage
            .as_ref()
            .map(|usage| usage.models.len())
            .unwrap_or(0);
        let content = px(PANEL_TITLE
            + PANEL_PADDING * 2.
            + PANEL_HEADLINE
            + PANEL_GAP * 3.
            + LEGEND_HEAD
            + LEGEND_ROW * models as f32)
            + days_height
            + share_height;
        let panel = size(
            (window.width * 0.30)
                .clamp(px(440.), px(560.))
                .min(window.width - px(96.)),
            content
                .min(window.height - px(PANEL_FOOT + PANEL_HEADROOM))
                .max(px(320.)),
        );
        let usage = self.usage.clone().unwrap_or_default();
        let appear_key = self.charts_opened;
        let close = {
            let strip = strip.clone();
            move |_: &MouseDownEvent, _: &mut Window, cx: &mut App| {
                strip
                    .update(cx, |bar, cx| {
                        bar.chart_open = false;
                        cx.notify();
                    })
                    .ok();
            }
        };
        let mut scrim = div()
            .id("usage-scrim")
            .test_support()
            .w(window.width)
            .h(window.height)
            .occlude()
            .on_mouse_down(MouseButton::Left, close);
        if let Some(focus) = self.focus.clone() {
            scrim = scrim.track_focus(&focus).on_key_down(cx.listener(
                |bar, event: &KeyDownEvent, _, cx| {
                    if event.keystroke.key == "escape" {
                        bar.chart_open = false;
                        cx.notify();
                    }
                },
            ));
        }
        deferred(
            anchored().child(
                scrim.child(
                    div()
                        .v_flex()
                        .w(window.width)
                        .h(window.height)
                        // Against the strip end of the window rather than the top: the
                        // panel hangs off the bar it reports on, and grows upward as the
                        // list does. Off the leading edge by an inset, and clear of the
                        // strip by the same.
                        .items_start()
                        .justify_end()
                        .pb(px(PANEL_FOOT))
                        .pl(px(PANEL_INSET))
                        .child(usage_panel(
                            usage,
                            appear_key,
                            size(days_height, share_height),
                            panel,
                            strip,
                            &colors,
                            cx,
                        )),
                ),
            ),
        )
        // No raised priority: the deferred pass is walked in ascending priority, so a
        // priority above the charts' own (zero) would paint this card — and its scrim —
        // over the tooltips the charts defer inside it.
        .into_any_element()
    }

    /// What the last usage sweep found, for a test that asserts on the chart
    /// rather than on the scan.
    #[cfg(test)]
    pub fn set_usage(&mut self, usage: UsageSummary) {
        self.usage = Some(usage);
    }
}

/// The usage detail's body: the window's headline numbers, the days as a bar chart,
/// and the share each provider took beside the legend that names them.
#[allow(clippy::too_many_arguments)]
fn usage_panel(
    usage: UsageSummary,
    appear_key: u64,
    charts: Size<Pixels>,
    panel: Size<Pixels>,
    strip: WeakEntity<ResourceBar>,
    colors: &ColorTokens,
    cx: &App,
) -> AnyElement {
    let (days_height, share_height) = (charts.width, charts.height);
    let (width, height) = (panel.width, panel.height);
    // The ring is a share of the box it is drawn in, and the box the panel's own
    // width: both grow with the window rather than with a table of constants.
    let ring = (share_height - px(16.)).as_f32() * 0.4;
    // What the body has to scroll within: the panel less its own padding and the
    // title row above it.
    let body_height = (height - px(PANEL_TITLE + PANEL_PADDING * 2.)).max(px(120.));

    let headline = |label: String, totals: &syro_core::session::Usage| {
        div()
            .v_flex()
            .gap_0p5()
            .child(
                div()
                    .text_xs()
                    .text_color(colors.muted_foreground)
                    .child(SharedString::from(label)),
            )
            .child(div().text_lg().child(SharedString::from(format!(
                "{} tok · ${:.2}",
                format_tokens(totals.tokens),
                totals.cost
            ))))
    };

    // Before the first sweep there is nothing to chart, and a chart of no data is
    // either a blank box or a division by zero: the panel says so instead.
    if usage.days.is_empty() {
        return div()
            .id("usage-panel")
            .test_support()
            .child(SharedString::from(
                t!("resource.usage_measuring").to_string(),
            ))
            .into_any_element();
    }

    // Every day's column is stacked by model, in the colours the ring and the legend
    // beside it use: one model keeps one colour wherever it appears.
    let shares = shares_of(&usage, cx);
    let color_of = |model: &str| {
        shares
            .iter()
            .find(|share| share.label.as_ref() == model)
            .map(|share| share.color)
            .unwrap_or_else(|| colors.border)
    };
    let days = ModelBars::new(
        usage
            .days
            .iter()
            .map(|day| DaySpend {
                label: SharedString::from(day.label.clone()),
                models: day
                    .models
                    .iter()
                    .map(|slice| ModelSpend {
                        model: SharedString::from(slice.model.clone()),
                        tokens: slice.tokens,
                        cost: slice.cost,
                        color: color_of(&slice.model),
                    })
                    .collect(),
            })
            .collect(),
    );

    let legend = legend(&shares, body_height, colors);
    let share = PieChart::new(shares.clone())
        .value(|share| share.percent as f32)
        .color(|share| share.color)
        .tooltip_name(|share| share.label.clone())
        .tooltip_value(|share, _, _| {
            SharedString::from(format!(
                "{} tok · ${:.2}",
                format_tokens(share.tokens),
                share.cost
            ))
        })
        .inner_radius(ring * 0.62)
        .outer_radius(ring)
        .pad_angle(0.01)
        .id("usage-share-chart")
        .appear_key(appear_key);

    div()
        .id("usage-panel")
        .test_support()
        // Solid at any window opacity, so the charts are read on their own surface —
        // and occluding, so a click anywhere on the card is the card's and not a click
        // on the scrim behind it, which would close the panel out from under the
        // pointer. The scrim is only what the *window* behind the panel falls through
        // to.
        .occlude()
        .v_flex()
        .gap(px(PANEL_GAP))
        .w(width)
        .h(height)
        .p(px(PANEL_PADDING))
        .rounded_lg()
        .border_1()
        .border_color(colors.border)
        .bg(crate::settings_view::solid(colors.surface))
        .child(
            div()
                .h_flex()
                .justify_between()
                .items_center()
                .child(
                    div()
                        .id("usage-title")
                        .test_support()
                        .text_sm()
                        .text_color(colors.muted_foreground)
                        .child(SharedString::from(t!("resource.usage_title").to_string())),
                )
                .child(
                    div()
                        .id("usage-close")
                        .test_support()
                        .aria_label(t!("common.close"))
                        .h_flex()
                        .items_center()
                        .justify_center()
                        .size(px(20.))
                        .rounded_sm()
                        .cursor_pointer()
                        .hover(|el| el.bg(colors.muted))
                        .child(Icon::new(IconName::Close).size(px(12.)))
                        .on_click(move |_event, _, cx| {
                            strip
                                .update(cx, |bar, cx| {
                                    bar.chart_open = false;
                                    cx.notify();
                                })
                                .ok();
                        }),
                ),
        )
        .child(
            // A column, scrolled: the panel is a tall strip down the window's side, and
            // the list of models is what runs past the bottom of it. The box the scroller
            // is given is a row of its own — `overflow_y_scrollbar` wraps its element, and
            // the wrapper takes the size rather than the element it wraps.
            div()
                .id("usage-viewport")
                .test_support()
                .w_full()
                .flex_1()
                .min_h(px(0.))
                .h(body_height)
                .child(
                    div()
                        .v_flex()
                        .gap_3()
                        .w_full()
                        .h_full()
                        .overflow_y_scrollbar()
                        // The scrolling element is a wrapper the kit builds; its own id is not
                        // one a test can find, so the content inside it carries the name.
                        .child(
                            div()
                                .id("usage-body")
                                .test_support()
                                // No shrinking: the content is what it is, and the
                                // scroller around it is what gives way.
                                .flex_shrink_0()
                                .v_flex()
                                .gap(px(PANEL_GAP))
                                .child(
                                    div()
                                        .h_flex()
                                        .flex_shrink_0()
                                        .gap_6()
                                        .child(headline(
                                            t!("resource.usage_window", days = USAGE_DAYS)
                                                .to_string(),
                                            &usage.total,
                                        ))
                                        .child(headline(
                                            t!("resource.usage_today").to_string(),
                                            &usage.today,
                                        )),
                                )
                                .child(
                                    div()
                                        .id("usage-days-box")
                                        .test_support()
                                        .h(days_height)
                                        .flex_shrink_0()
                                        .w_full()
                                        .child(days),
                                )
                                .child(
                                    div()
                                        .id("usage-share-box")
                                        .test_support()
                                        .w_full()
                                        .h(share_height)
                                        .flex_shrink_0()
                                        .child(share),
                                )
                                .child(legend),
                        ),
                ),
        )
        .into_any_element()
}

/// One provider's slice of the share chart: its name, what it cost, and the colour
/// its ring and its legend row share.
#[derive(Clone)]
struct Share {
    label: SharedString,
    tokens: u64,
    cost: f64,
    /// The share of the window's tokens, which is what the ring's arcs are laid out
    /// from and what the legend's last column prints.
    percent: f64,
    color: Hsla,
}

/// The providers as slices, biggest first (the scan already sorted them), each taking
/// its colour from the theme's chart palette in that order.
fn shares_of(usage: &UsageSummary, cx: &App) -> Vec<Share> {
    let total: u64 = usage.models.iter().map(|model| model.tokens).sum();
    usage
        .models
        .iter()
        .enumerate()
        .map(|(index, model)| Share {
            label: SharedString::from(model.label.clone()),
            tokens: model.tokens,
            cost: model.cost,
            percent: if total == 0 {
                0.
            } else {
                model.tokens as f64 / total as f64 * 100.
            },
            color: model_color(index, cx),
        })
        .collect()
}

/// The hues a model's colour walks, in degrees: blue, orange, green, magenta, violet,
/// yellow, teal, red, lime, purple.
///
/// A set, not a rotation: the theme's own chart palette is one hue in five steps — the
/// built-in themes are five blues — and five blues cannot tell five models apart, while
/// hues taken at an even spacing land next to each other again as soon as the list is
/// long (or short) enough. These stay apart whatever the list does.
const MODEL_HUES: [f32; 10] = [205., 25., 145., 320., 265., 50., 175., 0., 95., 290.];

/// The colour a model keeps across the panel: one of [`MODEL_HUES`], at the theme's own
/// saturation and lightness so the panel still looks like the app it is drawn in. The
/// eleventh model starts the hues again a shade darker.
fn model_color(index: usize, cx: &App) -> Hsla {
    let base = cx.theme().chart_2;
    let step = (index / MODEL_HUES.len()) as f32;
    Hsla {
        h: model_hue(index),
        s: base.s.max(0.55),
        l: (base.l - 0.10 * step).clamp(0.40, 0.72),
        a: 1.,
    }
}

/// The hue `index` models in, as a fraction of the wheel.
fn model_hue(index: usize) -> f32 {
    MODEL_HUES[index % MODEL_HUES.len()] / 360.
}

/// The legend: every provider with its colour, what it spent, and its share. A grid
/// rather than a row of columns, so each column is as wide as its widest value and a
/// long host name is printed whole.
fn legend(shares: &[Share], max_height: Pixels, colors: &ColorTokens) -> AnyElement {
    let mut rows = div()
        .id("usage-legend")
        .test_support()
        .v_flex()
        .gap_2()
        .w_full()
        .flex_shrink_0()
        .max_h(max_height)
        .text_xs()
        .text_color(colors.muted_foreground);
    for share in shares {
        let number = |width: f32, text: String| {
            div()
                .w(px(width))
                .flex_shrink_0()
                .text_right()
                .child(SharedString::from(text))
        };
        rows = rows.child(
            div()
                .id(SharedString::from(format!("usage-legend-{}", share.label)))
                .test_support()
                .h_flex()
                .flex_shrink_0()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .w(px(8.))
                        .h(px(8.))
                        .flex_shrink_0()
                        .rounded_sm()
                        .bg(share.color),
                )
                // The name takes the slack, so the figures beside it end at the panel's
                // own edge rather than leaving a hand's width of nothing.
                .child(div().flex_1().child(share.label.clone()))
                .child(number(
                    LEGEND_TOKENS,
                    format!("{} tok", format_tokens(share.tokens)),
                ))
                .child(number(LEGEND_COST, format!("${:.2}", share.cost)))
                .child(number(LEGEND_PERCENT, format!("{:.0}%", share.percent))),
        );
    }
    div()
        .v_flex()
        .gap_2()
        .w_full()
        .flex_shrink_0()
        .max_h(max_height)
        .child(
            div()
                .text_xs()
                .text_color(colors.muted_foreground)
                .child(SharedString::from(t!("resource.usage_share").to_string())),
        )
        .child(rows)
        .into_any_element()
}

impl Render for ResourceBar {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = Theme::global(cx).tokens.colors.clone();
        let summary = self.summary();
        // The app process alone, not its subtree: what the terminals cost
        // belongs to their worktrees, and the sidebar's rows are where that is
        // read.
        let app = self.snapshot.as_ref().map(|snapshot| snapshot.app);

        let mut strip = div()
            .id("resource-bar")
            .test_support()
            .h_flex()
            .items_center()
            .gap_3()
            .px_3()
            .h(px(BAR_HEIGHT))
            .flex_shrink_0()
            .w_full()
            // A readout, not a layout: past its width the tail is clipped rather
            // than the row growing or the sidebar being pushed along.
            .overflow_hidden()
            .bg(colors.surface)
            .border_t_1()
            .border_color(colors.border)
            .text_xs()
            .text_color(colors.muted_foreground)
            .child(
                // The settings button, at the leading end of the status bar: the
                // one way into the settings form from the chrome.
                div()
                    .id("settings-open")
                    .test_support()
                    .aria_label(t!("dialog.settings"))
                    .h_flex()
                    .items_center()
                    .justify_center()
                    .size(px(20.))
                    .rounded_sm()
                    .cursor_pointer()
                    .hover(|el| el.bg(colors.muted))
                    .child(Icon::new(IconName::Settings).size(px(13.)))
                    .on_click(cx.listener(|_this, _event, window, cx| {
                        // Dispatched rather than opened here: the workspace owns
                        // the dialog, and an action from this element reaches it
                        // (see `Workspace::handle_settings_open`).
                        window.dispatch_action(Box::new(crate::settings_view::Open), cx);
                    })),
            )
            .child(
                // The readout: an icon and the app's own cost. A label rather
                // than a button — there is nothing behind it to open any more.
                div()
                    .id("resource-summary")
                    .test_support()
                    .aria_label(summary)
                    .h_flex()
                    .items_center()
                    .gap_1()
                    .px_1()
                    .child(Icon::new(IconName::MemoryStick).size(px(12.)))
                    .child(SharedString::from(match app {
                        Some(app) => format_bytes(app.rss_bytes),
                        None => "—".to_string(),
                    })),
            )
            .child(self.usage_chip(&colors, cx));

        if let Some(error) = &self.error {
            strip = strip.child(SharedString::from(
                t!("resource.error", error = error).to_string(),
            ));
        }

        if self.chart_open {
            strip = strip.child(self.chart_overlay(window.bounds().size, cx));
        }

        strip
    }
}

/// Bytes as a person reads them: one decimal at GB, whole MB below, and never a
/// bare "0 B" for something that is running.
pub fn format_bytes(bytes: u64) -> String {
    const MB: f64 = 1024.0 * 1024.0;
    const GB: f64 = 1024.0 * MB;
    let bytes = bytes as f64;
    if bytes >= GB {
        format!("{:.1} GB", bytes / GB)
    } else {
        format!("{:.0} MB", (bytes / MB).max(1.0))
    }
}

/// Tokens as a person reads them, and as a strip 24 points tall can hold: a
/// transcript's own tokens run to billions, so the number is a decimal and a
/// letter rather than a count.
pub fn format_tokens(tokens: u64) -> String {
    let tokens = tokens as f64;
    if tokens >= 1e9 {
        format!("{:.1}B", tokens / 1e9)
    } else if tokens >= 1e6 {
        format!("{:.1}M", tokens / 1e6)
    } else if tokens >= 1e3 {
        format!("{:.0}k", tokens / 1e3)
    } else {
        format!("{tokens:.0}")
    }
}

#[cfg(test)]
mod tests {
    // Not `use super::*`: the module above globs gpui in, and gpui's own `test`
    // attribute comes with it — under `cfg(test)` it shadows the built-in one
    // and every `#[test]` below expands into itself.
    use syro_core::model::AgentStatus;
    use syro_core::resource::{ProcessTable, ResourceSnapshot};
    use syro_core::session::{
        Cache, SessionRoots, Usage, UsageDay, UsageGroup, UsageSlice, UsageSummary,
    };

    use std::time::{Duration, Instant};

    use gpui_kit::Task;

    use super::{LiveTerminal, MODEL_HUES, ResourceBar, format_tokens, model_hue};

    /// Two worktrees, three terminals, one of them with a child of its own.
    fn table() -> ProcessTable {
        ProcessTable::parse(
            "\
    1     0  0.0  10240
    100   1  2.5  51200
    101 100  1.0  20480
    200   1  7.0  40960
    300   1  0.5  10240
",
        )
    }

    fn terminals() -> Vec<LiveTerminal> {
        vec![
            LiveTerminal {
                worktree: "repo:/w/main".to_string(),
                title: "Pi - main".to_string(),
                pid: Some(100),
                span: None,
                exit_ok: None,
                status: Some(AgentStatus::Working),
                stalled: false,
            },
            LiveTerminal {
                worktree: "repo:/w/main".to_string(),
                title: "Pi - fix the build".to_string(),
                pid: Some(200),
                span: None,
                exit_ok: None,
                status: None,
                stalled: false,
            },
            LiveTerminal {
                worktree: "repo:/w/feature".to_string(),
                title: "osdodo@matrix:~/w/feature".to_string(),
                pid: Some(300),
                span: None,
                exit_ok: None,
                status: None,
                stalled: false,
            },
        ]
    }

    fn bar() -> ResourceBar {
        ResourceBar {
            terminals: terminals(),
            snapshot: None,
            error: None,
            roots: SessionRoots::default(),
            usage_cache: Cache::default(),
            usage: None,
            chart_open: false,
            charts_opened: 0,
            swept_at: None,
            _poll: Task::ready(()),
            _sweep: None,
            focus: None,
        }
    }

    /// A sweep's answer: two days of one model, which is all the chip and the chart
    /// below need to be drawn.
    fn usage() -> UsageSummary {
        let slice = |model: &str, tokens: u64, cost: f64| UsageSlice {
            model: model.to_string(),
            tokens,
            cost,
        };
        UsageSummary {
            total: Usage {
                tokens: 2_500_000_000,
                cost: 1824.49,
            },
            today: Usage {
                tokens: 40_000_000,
                cost: 4.46,
            },
            days: vec![
                UsageDay {
                    label: "10-13".to_string(),
                    tokens: 731_000_000,
                    cost: 677.44,
                    models: vec![slice("claude-opus-5-5", 731_000_000, 677.44)],
                },
                UsageDay {
                    label: "10-14".to_string(),
                    tokens: 423_000_000,
                    cost: 343.57,
                    models: vec![slice("claude-opus-5-5", 423_000_000, 343.57)],
                },
            ],
            providers: vec![UsageGroup {
                label: "iotex".to_string(),
                tokens: 1_564_000_000,
                cost: 12.64,
            }],
            models: vec![UsageGroup {
                label: "claude-opus-5-5".to_string(),
                tokens: 1_564_000_000,
                cost: 12.64,
            }],
        }
    }

    /// The chip reads the window's numbers, and says it is measuring rather than
    /// showing a zero — a strip that has not read anything cannot know there was
    /// nothing to read.
    #[test]
    fn the_chip_says_what_the_window_cost() {
        let mut bar = bar();
        assert_eq!(bar.usage_text(), "Usage: —");

        bar.usage = Some(usage());
        assert_eq!(bar.usage_text(), "14d 2.5B tok · $1824.49");
        assert_eq!(
            bar.usage_label(),
            "Usage: 2.5B tokens and $1824.49 over the last 14 days"
        );

        // Nothing recorded is not the same as not having looked.
        bar.usage = Some(UsageSummary::default());
        assert_eq!(bar.usage_text(), "Usage: none");
    }

    /// Every model's colour has to be a different one: the palette this replaced was
    /// five steps of one hue, and a rotation folds back onto itself a few models in.
    #[test]
    fn every_model_hue_stays_apart_from_the_others() {
        let hues: Vec<f32> = (0..MODEL_HUES.len()).map(model_hue).collect();
        for (left, a) in hues.iter().enumerate() {
            for (right, b) in hues.iter().enumerate().skip(left + 1) {
                let apart = (a - b).abs();
                let apart = apart.min(1. - apart);
                assert!(
                    apart > 0.06,
                    "models {left} and {right} are {apart:.3} apart on the wheel: {hues:?}"
                );
            }
        }
        // And the eleventh model starts the hues again rather than inventing an
        // eleventh colour nobody can name.
        assert_eq!(model_hue(0), model_hue(MODEL_HUES.len()));
    }

    /// Tokens as the strip and the chart can hold them: a transcript's own tokens
    /// run to billions, and a bar's number is not a column in a table.
    #[test]
    fn tokens_read_in_units_a_strip_can_hold() {
        assert_eq!(format_tokens(0), "0");
        assert_eq!(format_tokens(999), "999");
        assert_eq!(format_tokens(12_300), "12k");
        assert_eq!(format_tokens(12_300_000), "12.3M");
        assert_eq!(format_tokens(2_495_635_613), "2.5B");
    }

    /// A bar whose last sweep is the table above.
    fn measured() -> ResourceBar {
        let mut bar = bar();
        bar.snapshot = Some(ResourceSnapshot::of(&table(), 1, &[100, 200, 300]));
        bar
    }

    #[test]
    fn a_worktree_costs_its_terminals_trees() {
        let bar = measured();

        // Two terminals in one worktree, and one of them started a child: a
        // terminal costs its whole tree.
        let main = bar
            .worktree_usage("repo:/w/main")
            .expect("the sweep answered");
        assert_eq!(main.terminals.len(), 2);
        assert_eq!(main.cpu_pct, 2.5 + 1.0 + 7.0);
        assert_eq!(main.rss_bytes, (51200 + 20480 + 40960) * 1024);
        // A row is called what the workspace named it, the same name its tab has.
        assert_eq!(main.terminals[0].title, "Pi - main");
        assert_eq!(main.terminals[1].title, "Pi - fix the build");
        assert_eq!(main.terminals[0].status, Some(AgentStatus::Working));
        // 100's tree is 100 + 101, and it is the first terminal's own row.
        assert_eq!(main.terminals[0].rss_bytes, (51200 + 20480) * 1024);
        assert_eq!(main.terminals[1].rss_bytes, 40960 * 1024);

        let feature = bar
            .worktree_usage("repo:/w/feature")
            .expect("the sweep answered");
        assert_eq!(feature.terminals.len(), 1);
        assert_eq!(feature.rss_bytes, 10240 * 1024);
        assert_eq!(feature.terminals[0].title, "osdodo@matrix:~/w/feature");

        // A worktree with nothing running in it has no numbers, not zeroes —
        // which is what the row draws nothing for.
        let other = bar
            .worktree_usage("repo:/w/other")
            .expect("the sweep answered");
        assert!(other.terminals.is_empty());
        assert_eq!(other.rss_bytes, 0);
    }

    #[test]
    fn before_the_first_sweep_there_are_no_numbers() {
        assert_eq!(bar().worktree_usage("repo:/w/main"), None);
    }

    /// A shell with a job in front of it is busy; the terminal that ran it is not.
    #[test]
    fn a_shell_with_a_job_in_front_of_it_is_busy() {
        let table = ProcessTable::parse(
            "\
    1     0  0.0  10240    0
    300   1  0.5  10240  301
    301 300  1.0   2048  301
",
        );
        let mut bar = bar();
        bar.snapshot = Some(ResourceSnapshot::of(&table, 1, &[100, 200, 300]));

        let feature = bar
            .worktree_usage("repo:/w/feature")
            .expect("the sweep answered");
        assert!(feature.terminals[0].busy, "bp is in front of the shell");
        // And a terminal with nothing in front of it is not, which is what keeps
        // a plain shell at its prompt saying nothing.
        let main = bar
            .worktree_usage("repo:/w/main")
            .expect("the sweep answered");
        assert!(!main.terminals[0].busy);
    }

    /// A terminal whose child is gone keeps its row: it has no pid left, so it
    /// costs nothing, and the row says how it went and how long it ran.
    #[test]
    fn a_finished_terminal_keeps_its_row_without_a_cost() {
        let started = Instant::now() - Duration::from_secs(600);
        let finished = Instant::now() - Duration::from_secs(60);
        let mut bar = bar();
        bar.terminals.push(LiveTerminal {
            worktree: "repo:/w/main".to_string(),
            title: "Pi - blog".to_string(),
            pid: None,
            span: Some((started, Some(finished))),
            exit_ok: Some(false),
            status: None,
            stalled: false,
        });
        bar.snapshot = Some(ResourceSnapshot::of(
            &table(),
            1,
            &[100, 200, 300, u32::MAX],
        ));

        let main = bar
            .worktree_usage("repo:/w/main")
            .expect("the sweep answered");
        let row = main.terminals.last().expect("the finished row");
        assert_eq!(row.title, "Pi - blog");
        assert_eq!(row.exit, Some(false));
        // Counted from the start to the exit, not to now: it is not running.
        assert_eq!(row.uptime.map(|uptime| uptime.as_secs()), Some(540));
        assert_eq!(row.cpu_pct, 0.0);
        assert_eq!(row.rss_bytes, 0);
        // And the terminals that are still running are untouched by it.
        assert_eq!(main.cpu_pct, 2.5 + 1.0 + 7.0);
    }

    #[test]
    fn the_strip_reads_the_app_alone() {
        // The app is pid 1 and one process: its terminals' trees are not its
        // own, and counting them here would show every one of them twice.
        let summary = measured().summary();
        assert_eq!(summary, "Resources: 10 MB");
    }
}
