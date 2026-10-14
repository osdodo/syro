//! The resource readout: the app's own cost on the strip across the bottom of
//! the window, and what each worktree costs, which the sidebar draws on its rows.

use std::time::Instant;

use gpui_kit::base::{StyledExt as _, TestSupportExt as _, Theme};
use gpui_kit::component::{Icon, IconName};
use gpui_kit::*;
use rust_i18n::t;
use syro_core::model::AgentStatus;
use syro_core::resource::{ProcessTable, ResourceSnapshot};

/// How often the readout re-reads the process table. Five seconds is a number
/// that is still true enough to glance at; the sweep it saves is the app's own
/// CPU.
const REFRESH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

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
    /// The poll loop. Held so it stops with the view.
    _poll: Task<()>,
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
                if this.update(cx, |bar, cx| bar.refresh(cx)).is_err() {
                    break;
                }
            }
        });
        let mut bar = Self {
            terminals: Vec::new(),
            snapshot: None,
            error: None,
            _poll: poll,
        };
        bar.refresh(cx);
        bar
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
}

impl Render for ResourceBar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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
            .h(px(24.))
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
            );

        if let Some(error) = &self.error {
            strip = strip.child(SharedString::from(
                t!("resource.error", error = error).to_string(),
            ));
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

#[cfg(test)]
mod tests {
    // Not `use super::*`: the module above globs gpui in, and gpui's own `test`
    // attribute comes with it — under `cfg(test)` it shadows the built-in one
    // and every `#[test]` below expands into itself.
    use syro_core::model::AgentStatus;
    use syro_core::resource::{ProcessTable, ResourceSnapshot};

    use std::time::{Duration, Instant};

    use gpui_kit::Task;

    use super::{LiveTerminal, ResourceBar};

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
            _poll: Task::ready(()),
        }
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
