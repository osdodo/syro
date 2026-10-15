//! The terminal tab: a real PTY (`syro_core::term::TermSession`) painted as a grid of cells,
//! with the keyboard and the wheel wired to it.

use std::cell::Cell as SharedCell;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use alacritty_terminal::grid::Dimensions as _;
use alacritty_terminal::grid::Grid;
use alacritty_terminal::index::{Column, Line, Point as GridPoint, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::{Term, TermMode};
use alacritty_terminal::vte::ansi::{Color as AnsiColor, CursorShape, NamedColor, Rgb as AnsiRgb};
use gpui_kit::base::{ColorTokens, StyledExt as _, TestSupportExt as _, Theme};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rust_i18n::t;
use syro_core::agent;
use syro_core::model::{AgentId, AgentSpec, AgentStatus, TabSpec, TerminalId};
use syro_core::session;
// `MouseButton as Report`: gpui has a `MouseButton` too, and this one is the
// protocol's — the button as the *child* is told about it.
use syro_core::term::{
    EventProxy, MouseButton as Report, MouseModifiers, MouseReport, TermEvent, TermSession,
    TermSize, TermSnapshot, TermSpec,
};

use super::PaneItem;
use crate::file_tree::DraggedPath;

/// How often the view looks for terminal output.
const REPAINT_INTERVAL: Duration = Duration::from_millis(16);

/// alacritty's own minimums: a grid smaller than this cannot hold a cursor.
const MIN_COLUMNS: usize = 2;
const MIN_SCREEN_LINES: usize = 1;

/// The gap between the pane's edge and the first cell.
const PADDING_X: f32 = 8.;
const PADDING_Y: f32 = 6.;

/// One id per `TerminalView`, so two terminals in a window are two elements.
static NEXT_TERMINAL: AtomicUsize = AtomicUsize::new(0);

/// What a terminal's agent is doing changed.
#[derive(Debug, Clone, PartialEq)]
pub struct PaneStatus {
    pub status: Option<AgentStatus>,
    /// Said it was working and then went quiet (see `agent::stalled`). Reported
    /// beside the status because it comes from the same evidence — this
    /// terminal's output — and changes on its own, with no event to announce it.
    pub stalled: bool,
}

impl EventEmitter<PaneStatus> for TerminalView {}

/// A past session a terminal was started from: what the resume flags need to
/// name it. Kept by the view, so the tab's Restart re-enters the same session
/// rather than starting a new one beside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRef {
    /// The agent's own session id (`{id}`).
    pub id: String,
    /// The transcript file (`{path}`), which is what Pi resumes by.
    pub path: PathBuf,
}

/// The `TermSpec` an agent preset describes: its command, its args, its env, in `cwd`.
pub fn term_spec_for_agent(
    agent: &AgentSpec,
    cwd: PathBuf,
    size: TermSize,
    resume: Option<&SessionRef>,
) -> TermSpec {
    let mut spec = TermSpec::command(cwd, size, &agent.command);
    spec.args = agent.args.clone();
    if let Some(resume) = resume {
        spec.args.extend(
            agent
                .resume_args
                .iter()
                .map(|arg| expand_resume_arg(arg, resume)),
        );
    }
    spec.env = agent
        .env
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    spec
}

/// The builtin presets, parsed once: what a shell's title is read against to see an agent in it.
/// shortcut: builtins only, so a user-only agent typed at a shell prompt is not recognised; pass
/// the resolved agents in if that matters.
fn builtin_agents() -> &'static [AgentSpec] {
    static AGENTS: std::sync::OnceLock<Vec<AgentSpec>> = std::sync::OnceLock::new();
    AGENTS.get_or_init(agent::builtin_agents)
}

/// The builtin agent a user launched from a plain shell, if the command line
/// starts with that agent's command.
fn manual_agent(line: &str) -> Option<AgentSpec> {
    let command = line.split_whitespace().next()?;
    let command = command.trim_matches(['\'', '"']);
    if command.starts_with("./") || command.starts_with("../") {
        return None;
    }
    let command = command.rsplit('/').next()?;
    builtin_agents()
        .iter()
        .find(|agent| agent.command == command || agent.id == command)
        .cloned()
}

/// A random v4 UUID, the shape Claude Code's `--session-id` insists on. No crate: std's
/// randomly keyed hasher is random enough for an id nobody guesses at.
fn new_session_id() -> String {
    use std::hash::{BuildHasher as _, RandomState};
    let now = std::time::SystemTime::now();
    let high = RandomState::new().hash_one(now);
    let low = RandomState::new().hash_one((now, std::process::id()));
    let bits = (u128::from(high) << 64 | u128::from(low)) & !(0xF << 76) & !(0x3 << 62)
        | 0x4 << 76
        | 0x2 << 62;
    let hex = format!("{bits:032x}");
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

/// One `resume_args` entry with `{id}` and `{path}` filled in.
fn expand_resume_arg(arg: &str, resume: &SessionRef) -> String {
    arg.replace("{id}", &resume.id)
        .replace("{path}", &resume.path.to_string_lossy())
}

/// A terminal tab.
pub struct TerminalView {
    /// Held by `Rc` because the canvas closures paint from it. Dropping the
    /// last clone drops the session, whose own `Drop` hangs up the child — so
    /// closing the tab ends the agent rather than leaving it running.
    session: Rc<TermSession>,
    focus: FocusHandle,
    metrics: CellMetrics,
    element_id: ElementId,
    /// Stable for the life of this view, and unique in the process: what a
    /// status map is keyed by and what a notification is tagged with.
    terminal_id: TerminalId,
    /// What the tab was opened with, so [`PaneItem::to_spec`] can put it back.
    cwd: PathBuf,
    /// The preset this terminal was started from, rules and all. `None` is a
    /// plain shell, which has no agent status.
    agent: Option<AgentSpec>,
    /// The past session it re-entered, if it did.
    resume: Option<SessionRef>,
    /// The id a new session was started under (`new_args`), which is how its transcript is found.
    session_id: Option<String>,
    /// A builtin agent this shell's title says is running in it, when no preset started one:
    /// `pi` typed at the prompt is still Pi.
    seen: Option<AgentSpec>,
    /// A builtin agent explicitly launched from this shell's command line.
    manual: Option<AgentSpec>,
    /// The task the agent's title names (`agent::task_of`).
    task: Option<String>,
    /// Printable input typed at the shell prompt, used to identify an agent
    /// started manually before it has set a terminal title.
    input_line: String,
    /// The first thing the user asked, from the transcript. Looked for until found.
    prompt: Option<String>,
    /// The child's `OSC 0` title, which is also what the status rules read.
    title: Option<SharedString>,
    /// Pixels of wheel scroll that did not add up to a line yet.
    scroll_pixels: f32,
    /// The IME's pre-edit text: what the user has composed but not committed.
    /// `None` while no composition is in flight. Painted at the cursor with an
    /// underline, and the only thing [`Self::commit_text`] ever sends.
    marked: Option<String>,
    /// The title's reading. Recomputed when the title changes — classification
    /// compiles the preset's regexes, which is not a per-frame cost.
    status: Option<AgentStatus>,
    /// When the child last put anything on its terminal. The other half of the
    /// status: a title says what the agent claims to be doing, this says whether
    /// it is still doing anything at all.
    last_output: Instant,
    /// [`Self::last_output`] older than `agent::STALL_AFTER`, while `status` is
    /// `Working`. Recomputed on every poll tick (the loop already runs) and
    /// reported to the state, which is what the dots read.
    stalled: bool,
    exit: Option<ExitStatus>,
    /// When this child was started: what a row's length is counted from, and what
    /// a restart resets, because a restart is a new view.
    started: Instant,
    /// When the child exited, if it has. A finished row stops counting here
    /// rather than growing while the pane sits with a dead child in it.
    exited_at: Option<Instant>,
    /// The agent's current or last stretch of work: when it started, and when it
    /// stopped if it has. What a row's length measures — an agent that has not
    /// started anything has no length to show.
    work: Option<(Instant, Option<Instant>)>,
    /// The child reports its own status (`OSC 7501`), so the title is not read
    /// for one: Pi's title stays `π - project` whatever it is doing.
    reports_status: bool,
    /// Where the last painted frame put the grid.
    layout: Rc<SharedCell<GridLayout>>,
    /// A press is being dragged: the selection keeps the kind the press chose
    /// (`Selection` in the grid already holds the position).
    dragging: bool,
    /// Where a right-click opened the pane's menu, in window coordinates.
    /// `None` is no menu; see [`PaneMenu`].
    menu: Option<Point<Pixels>>,
    /// The poll loop. Held so it stops with the view.
    _repaint: Task<()>,
}

/// The pane's context menu: the split entries and the terminal's own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PaneMenu {
    Copy,
    Paste,
    SplitRight,
    SplitDown,
    ClosePane,
}

impl PaneMenu {
    const ALL: [PaneMenu; 5] = [
        PaneMenu::Copy,
        PaneMenu::Paste,
        PaneMenu::SplitRight,
        PaneMenu::SplitDown,
        PaneMenu::ClosePane,
    ];

    fn label(self) -> String {
        match self {
            PaneMenu::Copy => t!("pane.copy"),
            PaneMenu::Paste => t!("pane.paste"),
            PaneMenu::SplitRight => t!("pane.split_right"),
            PaneMenu::SplitDown => t!("pane.split_down"),
            PaneMenu::ClosePane => t!("pane.close"),
        }
        .to_string()
    }

    /// The shortcut, in the spelling a menu shows it in. The same keys the
    /// app's keymap binds — a menu that says `⌘D` is not worth much if `⌘D`
    /// does something else.
    fn shortcut(self) -> &'static str {
        match self {
            PaneMenu::Copy => "⌘C",
            PaneMenu::Paste => "⌘V",
            PaneMenu::SplitRight => "⌘D",
            PaneMenu::SplitDown => "⌘⇧D",
            PaneMenu::ClosePane => "⌘W",
        }
    }

    /// The id a test finds the entry by.
    fn id(self) -> &'static str {
        match self {
            PaneMenu::Copy => "pane-menu-copy",
            PaneMenu::Paste => "pane-menu-paste",
            PaneMenu::SplitRight => "pane-menu-split-right",
            PaneMenu::SplitDown => "pane-menu-split-down",
            PaneMenu::ClosePane => "pane-menu-close",
        }
    }

    /// Whether a separator line is drawn above this entry: the menu's three
    /// groups are the clipboard, the splits, and closing the pane.
    fn opens_group(self) -> bool {
        matches!(self, PaneMenu::SplitRight | PaneMenu::ClosePane)
    }

    /// Whether the entry can be used right now. Copy without a selection has
    /// nothing to copy, which is a greyed-out row and not a hidden one.
    fn enabled(self, view: &TerminalView) -> bool {
        match self {
            PaneMenu::Copy => view.selection_text().is_some(),
            _ => true,
        }
    }
}

/// The menu's width, and how tall one of its entries is.
const MENU_WIDTH: f32 = 200.;

/// The pixel size of one cell. Measured once, not per frame: the theme's mono
/// face only changes when the theme does.
#[derive(Clone)]
struct CellMetrics {
    font: Font,
    font_size: Pixels,
    cell: Size<Pixels>,
}

/// Where the grid sits and how big it is, decided while laying the frame out.
#[derive(Clone, Copy, PartialEq)]
struct GridLayout {
    columns: usize,
    screen_lines: usize,
    origin: Point<Pixels>,
}

impl CellMetrics {
    fn measure(text_system: &TextSystem, theme: &Theme) -> Self {
        let typography = &theme.tokens.typography;
        let font = font(typography.mono.clone());
        let font_size = typography.mono_md.size;
        let font_id = text_system.resolve_font(&font);
        // A monospace face advances `m` by exactly one cell; an em is the
        // fallback for a face that does not.
        let width = text_system
            .advance(font_id, font_size, 'm')
            .map(|advance| advance.width)
            .or_else(|_| text_system.em_advance(font_id, font_size))
            .unwrap_or_else(|_| font_size * 0.6);
        Self {
            font,
            font_size,
            cell: size(width, typography.mono_md.line_height),
        }
    }
}

impl GridLayout {
    /// The grid the painted bounds can hold, resizing the session when that
    /// changed. Never otherwise: `Term::resize` reflows the whole grid.
    fn resolve(bounds: Bounds<Pixels>, metrics: &CellMetrics, session: &TermSession) -> Self {
        // The padding comes off the pane before the grid is measured: the cells are what is left, and
        // every other coordinate in this file — a click, a selection, the IME's cursor box — is
        // derived from this one origin, so the gap is accounted for once.
        let inner = Bounds::new(
            Point::new(
                bounds.origin.x + px(PADDING_X),
                bounds.origin.y + px(PADDING_Y),
            ),
            size(
                (bounds.size.width - px(PADDING_X * 2.)).max(px(0.)),
                (bounds.size.height - px(PADDING_Y * 2.)).max(px(0.)),
            ),
        );
        let cell_width = metrics.cell.width.as_f32().max(1.);
        let cell_height = metrics.cell.height.as_f32().max(1.);
        let columns = ((inner.size.width.as_f32() / cell_width).floor() as usize).max(MIN_COLUMNS);
        let screen_lines =
            ((inner.size.height.as_f32() / cell_height).floor() as usize).max(MIN_SCREEN_LINES);

        let current = session.size();
        if current.columns != columns || current.screen_lines != screen_lines {
            if let Err(error) = session.resize(columns, screen_lines) {
                tracing::warn!(%error, "terminal resize failed");
            }
        }

        Self {
            columns,
            screen_lines,
            origin: inner.origin,
        }
    }

    /// The cells `[column, column + span)` of `line`, in window coordinates.
    fn cells(
        &self,
        metrics: &CellMetrics,
        line: usize,
        column: usize,
        span: usize,
    ) -> Bounds<Pixels> {
        let width = metrics.cell.width * span as f32;
        Bounds::new(
            Point::new(
                self.origin.x + metrics.cell.width * column as f32,
                self.origin.y + metrics.cell.height * line as f32,
            ),
            size(width, metrics.cell.height),
        )
    }
}

/// The part of a platform `Modifiers` that a mouse report can carry: xterm's
/// four, eight and sixteen. A platform or function key has no bit.
fn report_modifiers(modifiers: &Modifiers) -> MouseModifiers {
    MouseModifiers {
        shift: modifiers.shift,
        alt: modifiers.alt,
        control: modifiers.control,
    }
}

impl TerminalView {
    /// Start `spec` and show its grid. `agent` is the preset it was started
    /// from — kept whole, because its status rules are what the title is read
    /// against; `None` is a plain shell, which has no status.
    pub fn new(
        cx: &mut Context<Self>,
        spec: TermSpec,
        agent: Option<AgentSpec>,
        resume: Option<SessionRef>,
        seed: Option<&str>,
    ) -> anyhow::Result<Self> {
        let cwd = spec.cwd.clone();
        let mut spec = spec;
        // A new session gets an id picked here, so the transcript it writes is this terminal's.
        let session_id = agent
            .as_ref()
            .filter(|agent| resume.is_none() && !agent.new_args.is_empty())
            .map(|agent| {
                let id = new_session_id();
                spec.args.extend(agent.new_args.iter().filter_map(|arg| {
                    if arg.contains("{prompt}") {
                        // Only a handoff carries a first message; a plain new
                        // session drops the placeholder rather than send an
                        // empty argument.
                        seed.map(|seed| arg.replace("{prompt}", seed))
                    } else {
                        Some(arg.replace("{id}", &id))
                    }
                }));
                id
            });
        let session = Rc::new(TermSession::spawn(spec)?);
        let theme = Theme::global(cx);
        let metrics = CellMetrics::measure(cx.text_system(), &theme);
        let focus = cx.focus_handle();
        let repaint = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(REPAINT_INTERVAL).await;
                if this.update(cx, |view, cx| view.poll(cx)).is_err() {
                    break;
                }
            }
        });

        let terminal_id = format!("terminal-{}", NEXT_TERMINAL.fetch_add(1, Ordering::Relaxed));
        Ok(Self {
            session,
            focus,
            metrics,
            element_id: ElementId::Name(terminal_id.clone().into()),
            terminal_id,
            layout: Rc::new(SharedCell::new(GridLayout {
                columns: 0,
                screen_lines: 0,
                origin: Point::default(),
            })),
            dragging: false,
            menu: None,
            cwd,
            prompt: resume.as_ref().and_then(|resume| {
                let agent = session::SessionAgent::from_id(agent.as_ref()?.id.as_str())?;
                session::first_prompt(agent, &resume.id, &resume.path)
            }),
            agent,
            resume,
            session_id,
            seen: None,
            manual: None,
            task: None,
            input_line: String::new(),
            title: None,
            scroll_pixels: 0.,
            marked: None,
            status: None,
            // A child that has just started has just been heard from.
            last_output: Instant::now(),
            stalled: false,
            exit: None,
            started: Instant::now(),
            exited_at: None,
            work: None,
            reports_status: false,
            _repaint: repaint,
        })
    }

    /// Stable for the life of this view: what the state keys a status by, and
    /// what a notification is tagged with.
    pub fn terminal_id(&self) -> &TerminalId {
        &self.terminal_id
    }

    /// The visible grid as plain text: what the web panel shows instead of a
    /// grid of cells.
    pub fn snapshot_text(&self) -> String {
        self.session.snapshot().text()
    }

    /// A live view of this terminal for the web panel: the subscription first (so
    /// nothing read from here on is missed), then the screen as it stands.
    ///
    /// The caller paints the screen, throws away whatever the subscription
    /// already caught while doing so, and streams the rest.
    pub fn stream(&self) -> (String, syro_core::term::Subscription) {
        let bytes = self.session.subscribe();
        (self.snapshot_text(), bytes)
    }

    /// Bytes from the web panel, straight to the child, as if typed here.
    pub fn write_bytes(&self, bytes: &[u8]) -> anyhow::Result<()> {
        self.session.write(bytes)
    }

    /// The grid size, for a panel's own viewport to start from.
    pub fn term_size(&self) -> TermSize {
        self.session.size()
    }

    /// Resize the child, as the panel's viewport asks.
    pub fn resize_term(&self, columns: usize, screen_lines: usize) -> anyhow::Result<()> {
        self.session.resize(columns, screen_lines)
    }

    /// The status this terminal last reported.
    pub fn agent_status(&self) -> Option<AgentStatus> {
        self.status
    }

    /// Whether that status has gone quiet (`agent::stalled`).
    pub fn is_stalled(&self) -> bool {
        self.stalled
    }

    /// The task the agent's title names, when it names one.
    pub fn task(&self) -> Option<&str> {
        self.task.as_deref()
    }

    /// The command that re-enters this terminal's conversation, for a phone to
    /// paste into a shell. `None` for a plain shell, a preset that cannot
    /// resume, or a session whose transcript is not on disk yet.
    pub fn resume_command(&self) -> Option<String> {
        let agent = self.agent.as_ref()?;
        let (id, path) = match &self.resume {
            Some(resume) => (Some(resume.id.clone()), Some(resume.path.clone())),
            None => {
                let id = self.session_id.clone()?;
                let kind = session::SessionAgent::from_id(agent.id.as_str())?;
                let home = std::env::var_os("HOME")?;
                let roots = session::SessionRoots::from_home(Path::new(&home));
                let path = session::transcript_of(&roots, kind, &self.cwd, &id);
                (Some(id), path)
            }
        };
        syro_core::api::resume_command(agent, id.as_deref(), path.as_deref())
    }

    /// The directory this terminal was started in, which is what a restart
    /// starts the new child in.
    pub fn cwd(&self) -> &std::path::Path {
        &self.cwd
    }

    /// The preset this terminal runs, if it runs one.
    pub fn agent_id(&self) -> Option<&AgentId> {
        self.agent.as_ref().map(|agent| &agent.id)
    }

    /// The preset this terminal draws its logo from: the one it was started with,
    /// or the one its title named — the same agent [`Self::agent_name`] names. A
    /// recognised agent is not [`Self::agent_id`]: it has nothing to restore it as.
    pub fn running_agent_id(&self) -> Option<&AgentId> {
        self.running_agent().map(|agent| &agent.id)
    }

    /// The preset the readout names and draws this terminal as: an explicitly
    /// launched agent takes precedence over ambiguous title-based detection.
    fn running_agent(&self) -> Option<&AgentSpec> {
        self.agent
            .as_ref()
            .or(self.manual.as_ref())
            .or(self.seen.as_ref())
    }

    /// Remember a manually started builtin agent, which may not set an OSC title
    /// that identifies itself.
    fn note_manual_agent(&mut self, line: &str, cx: &mut Context<Self>) {
        let Some(agent) = manual_agent(line) else {
            return;
        };
        let changed = self.agent_name() != Some(agent.name.as_str());
        self.manual = Some(agent);
        self.task = None;
        if changed {
            cx.emit(PaneStatus {
                status: self.status,
                stalled: self.stalled,
            });
        }
    }

    /// Track enough shell input to identify an agent when Enter submits the command.
    fn observe_input(&mut self, bytes: &[u8], cx: &mut Context<Self>) {
        for byte in bytes {
            match *byte {
                b'\r' | b'\n' => {
                    let line = std::mem::take(&mut self.input_line);
                    self.note_manual_agent(&line, cx);
                }
                0x03 | 0x04 => self.input_line.clear(),
                0x7f => {
                    self.input_line.pop();
                }
                0x20..=0x7e => self.input_line.push(*byte as char),
                _ => {}
            }
        }
    }

    /// What that preset is called — `Pi`, `Claude Code` — for the readouts that
    /// name a terminal by its agent rather than by its title, which is the task
    /// the agent is on and changes as it works.
    pub fn agent_name(&self) -> Option<&str> {
        self.running_agent().map(|agent| agent.name.as_str())
    }

    /// What the agent is on, for its name: the task its title names, else the first thing the
    /// user asked it. `None` before either is known.
    pub fn detail(&self) -> Option<&str> {
        self.task.as_deref().or(self.prompt.as_deref())
    }

    /// Re-read the agent and its task from a new title (`None` is the default title). Told as a
    /// `PaneStatus`, which is what the readouts already re-read a terminal on.
    fn read_name(&mut self, title: Option<&str>, cx: &mut Context<Self>) {
        let before = (self.agent_name().map(str::to_string), self.task.clone());
        if self.agent.is_none() && self.manual.is_none() {
            if let Some(detected) = title.and_then(|title| {
                let (id, _) = agent::classify_any(builtin_agents(), title)?;
                builtin_agents().iter().find(|spec| spec.id == id).cloned()
            }) {
                self.seen = Some(detected);
            }
        }
        let spec = self
            .agent
            .as_ref()
            .or(self.manual.as_ref())
            .or(self.seen.as_ref());
        self.task = spec
            .zip(title)
            .and_then(|(spec, title)| agent::task_of(spec, title));
        if before != (self.agent_name().map(str::to_string), self.task.clone()) {
            cx.emit(PaneStatus {
                status: self.status,
                stalled: self.stalled,
            });
        }
    }

    /// Look for the first prompt in this terminal's own transcript. Only a preset started with a
    /// session id has one to look in; true when it was found just now.
    fn find_prompt(&mut self) -> bool {
        if self.prompt.is_some() {
            return false;
        }
        let (Some(agent), Some(id)) = (self.agent.as_ref(), self.session_id.as_ref()) else {
            return false;
        };
        let Some(kind) = session::SessionAgent::from_id(agent.id.as_str()) else {
            return false;
        };
        let Some(home) = std::env::var_os("HOME") else {
            return false;
        };
        // shortcut: the default session roots only; a custom `--session-dir` / PI_CODING_AGENT_SESSION_DIR is not followed.
        let roots = session::SessionRoots::from_home(Path::new(&home));
        self.prompt = session::transcript_of(&roots, kind, &self.cwd, id)
            .and_then(|path| session::first_prompt(kind, id, &path));
        self.prompt.is_some()
    }

    /// The past session this terminal re-entered, if it did. What a Restart
    /// hands back to `terminal::New`, so a restored conversation is not quietly
    /// replaced by a new one.
    pub fn resume(&self) -> Option<&SessionRef> {
        self.resume.as_ref()
    }

    /// The child's pid, for the resource readout: what a terminal costs is the process tree hanging
    /// off this pid.
    pub fn process_id(&self) -> Option<u32> {
        if self.exit.is_some() {
            return None;
        }
        self.session.process_id()
    }

    /// The child's exit status, once it has exited. the Restart reads this.
    pub fn exit_status(&self) -> Option<ExitStatus> {
        self.exit
    }

    /// The two ends of the length a row shows: the agent's current or last
    /// stretch of work, or for a child that never worked, its whole run once it
    /// has exited. `None` is nothing to show yet. Instants rather than a number
    /// of seconds, so a row drawn a minute from now is a minute longer.
    pub fn span(&self) -> Option<(Instant, Option<Instant>)> {
        self.work
            .or_else(|| self.exited_at.map(|end| (self.started, Some(end))))
    }

    /// Whether this terminal's agent has gone quiet mid-task. Read by the
    /// resource readout, which has the terminals in hand; the sidebar asks the
    /// state, which is where this reading was reported.
    pub fn stalled(&self) -> bool {
        self.stalled
    }

    /// The visible grid as plain text, for tests. Painting reads the
    /// grid through `with_term`.
    pub fn screen(&self) -> TermSnapshot {
        self.session.snapshot()
    }

    /// What a copy would put on the clipboard: the selected text, or `None`
    /// when nothing is selected. The selection lives in the grid.
    pub fn selection_text(&self) -> Option<String> {
        self.session.with_term(|term| term.selection_to_string())
    }

    /// The composition in flight, in UTF-16 code units — the range the platform
    /// keeps its IME state in, and the range `hasMarkedText` is answered from.
    /// `None` is "not composing", which is what lets the platform start one.
    pub fn marked_text_range(&self) -> Option<std::ops::Range<usize>> {
        self.marked
            .as_ref()
            .map(|text| 0..text.encode_utf16().count())
    }

    /// The IME's pre-edit text arrived: draw it while the user composes. An
    /// empty string is the input method saying the composition is over, which
    /// is why it takes the same road as `unmark_text`.
    pub fn set_marked_text(&mut self, text: &str, cx: &mut Context<Self>) {
        if text.is_empty() {
            return self.clear_marked_text(cx);
        }
        self.marked = Some(text.to_string());
        cx.notify();
    }

    /// The composition ended without committing — Escape, a click elsewhere,
    /// the input method changing its mind. Nothing reaches the child: a byte
    /// here would be an `ESC` typed into whatever TUI is running.
    pub fn clear_marked_text(&mut self, cx: &mut Context<Self>) {
        if self.marked.take().is_some() {
            cx.notify();
        }
    }

    /// The composition was committed: the text is the user's input, and goes to
    /// the child as UTF-8, in one write.
    pub fn commit_text(&mut self, text: &str, cx: &mut Context<Self>) {
        self.clear_marked_text(cx);
        let Some(bytes) = composition_bytes(Some(text)) else {
            return;
        };
        if let Err(error) = self.session.write(&bytes) {
            tracing::warn!(%error, "terminal write failed");
        }
        self.observe_input(&bytes, cx);
    }

    /// Everything the terminal said since the last look. A repaint is asked for
    /// only when something actually changed, so an idle terminal draws nothing.
    fn poll(&mut self, cx: &mut Context<Self>) {
        let mut changed = false;
        for event in self.session.drain_events() {
            match event {
                TermEvent::Wakeup => {
                    // Output, or the grid changing at all: both mean the child is
                    // still saying something.
                    self.last_output = Instant::now();
                    changed = true;
                }
                TermEvent::Title(title) => {
                    self.title = Some(title.clone().into());
                    self.read_name(Some(&title), cx);
                    // Read once per title, not once per frame: classification
                    // compiles the preset's regexes.
                    if !self.reports_status {
                        self.set_status(self.read_title(&title), cx);
                    }
                    changed = true;
                }
                TermEvent::ResetTitle => {
                    changed |= self.title.take().is_some();
                    self.read_name(None, cx);
                    // Back to the default title, which is no agent's.
                    if !self.reports_status {
                        self.set_status(None, cx);
                    }
                }
                TermEvent::ProgramStatus(status) => {
                    // `clear` hands the status back to the title.
                    self.reports_status = status.is_some();
                    let status = status
                        .or_else(|| self.title.clone().and_then(|title| self.read_title(&title)));
                    self.set_status(status, cx);
                    changed = true;
                }
                TermEvent::Exit(status) => {
                    // The screen stays as the child left it; the tab bar is what
                    // offers a restart. The agent is gone, so its status is too.
                    self.exit = Some(status);
                    self.exited_at = Some(Instant::now());
                    if let Some((_, end @ None)) = &mut self.work {
                        *end = self.exited_at;
                    }
                    self.reports_status = false;
                    self.status = None;
                    self.stalled = false;
                    // Emitted whether or not that cleared a status: the readout's
                    // rows come from the workspace, and a child that is gone is a
                    // row with news — `Done`, `Failed` — either way.
                    cx.emit(PaneStatus {
                        status: None,
                        stalled: false,
                    });
                    changed = true;
                }
                TermEvent::Error(error) => {
                    tracing::warn!(%error, "terminal pty error");
                    changed = true;
                }
                // A bell has nowhere to go and the clipboard write is the app's
                // to route; neither changes the picture.
                TermEvent::Bell | TermEvent::ClipboardStore(_) => {}
            }
        }
        // Silence is what a title cannot show: an agent that announced "working" and then went quiet
        // reads as working forever.
        let stalled = agent::stalled(self.status, self.last_output.elapsed());
        if stalled != self.stalled {
            self.stalled = stalled;
            cx.emit(PaneStatus {
                status: self.status,
                stalled,
            });
        }
        if changed {
            cx.notify();
        }
    }

    /// What the preset's rules make of `title`; `None` without a preset, and `None` when the title
    /// does not read as that agent — which is how an agent that exited without saying so reads
    /// (the shell sets its own title over the agent's).
    fn read_title(&self, title: &str) -> Option<AgentStatus> {
        let agent = self.agent.as_ref()?;
        agent::classify(agent, title)
    }

    /// Record a new reading and tell whoever is listening. Emitted only on a
    /// change, so a TUI that re-sets the same title every redraw is one event.
    fn set_status(&mut self, status: Option<AgentStatus>, cx: &mut Context<Self>) {
        if self.status == status {
            return;
        }
        // A status change is a turn starting or ending, which is when a transcript gets its
        // first prompt; the event below carries the new name with it.
        self.find_prompt();
        // Waiting on the user is still the same piece of work.
        let busy = |status| matches!(status, Some(AgentStatus::Working | AgentStatus::Permission));
        match (busy(self.status), busy(status)) {
            (false, true) => self.work = Some((Instant::now(), None)),
            (true, false) => {
                if let Some((_, end)) = &mut self.work {
                    *end = Some(Instant::now());
                }
            }
            _ => {}
        }
        self.status = status;
        // And the quiet reading with it: the two are reported together, so they
        // cannot be reported out of step with each other.
        self.stalled = agent::stalled(status, self.last_output.elapsed());
        cx.emit(PaneStatus {
            status,
            stalled: self.stalled,
        });
    }

    /// Whether the child owns the mouse right now: it asked for reports, and the user is not
    /// holding the key that takes them back.
    fn child_has_the_mouse(&self, modifiers: &Modifiers) -> bool {
        self.session.wants_mouse() && !modifiers.alt
    }

    /// The cell a window position is over, as `(column, line)`: what a mouse
    /// report is addressed to. `None` before the first frame has measured the
    /// grid.
    fn pointer_cell(&self, position: Point<Pixels>) -> Option<(usize, usize)> {
        let (point, _) = self.grid_point(position)?;
        Some((point.column.0, point.line.0.max(0) as usize))
    }

    /// Send one mouse event to the child, in the protocol it negotiated. `false`
    /// when there was nowhere to send it.
    fn report_mouse(
        &mut self,
        event: MouseReport,
        position: Point<Pixels>,
        modifiers: &Modifiers,
    ) -> bool {
        let Some((column, line)) = self.pointer_cell(position) else {
            return false;
        };
        let report = self
            .session
            .mouse_report(event, column, line, report_modifiers(modifiers));
        if let Err(error) = self.session.write(&report) {
            tracing::warn!(%error, "mouse report failed");
            return false;
        }
        true
    }

    fn handle_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The clipboard keys are the app's, not the child's, so they are taken here and not put through
        // `key_to_bytes`.
        if self.menu.is_some() && event.keystroke.key == "escape" {
            self.menu = None;
            cx.stop_propagation();
            cx.notify();
            return;
        }
        if self.focus.is_focused(window) {
            match clipboard_key(&event.keystroke) {
                Some(ClipboardKey::Copy) => {
                    self.copy_selection(cx);
                    cx.stop_propagation();
                    return;
                }
                Some(ClipboardKey::Paste) => {
                    self.paste(cx);
                    cx.stop_propagation();
                    return;
                }
                None => {}
            }
        }

        let Some(bytes) = key_to_bytes(&event.keystroke) else {
            return;
        };
        if let Err(error) = self.session.write(&bytes) {
            tracing::warn!(%error, "terminal write failed");
        }
        self.observe_input(&bytes, cx);
        cx.stop_propagation();
    }

    fn handle_scroll_wheel(
        &mut self,
        event: &ScrollWheelEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let line_height = self.metrics.cell.height.as_f32().max(1.);
        // Pixels, whichever unit the platform reported: a trackpad reports pixels and a wheel reports
        // whole lines, and what has to survive to the next event is the *remainder* of a line.
        let pixels = match &event.delta {
            ScrollDelta::Pixels(pixels) => pixels.y.as_f32(),
            ScrollDelta::Lines(lines) => lines.y * line_height,
        };
        self.scroll_pixels += pixels;
        let lines = (self.scroll_pixels / line_height).trunc();
        // What the grid is given comes out of the accumulator; the fraction
        // stays for the next event.
        self.scroll_pixels -= lines * line_height;
        let lines = lines as i32;
        if lines == 0 {
            // Nothing moved, so there is nothing to draw: the fraction is kept.
            return;
        }
        // Three targets, in the order a terminal tries them.
        if self.session.wants_mouse() {
            let at = self.pointer_cell(event.position);
            let report = match at {
                Some((column, line)) => self.session.wheel_report(
                    lines > 0,
                    column,
                    line,
                    report_modifiers(&event.modifiers),
                ),
                None => return,
            };
            // One report per line: a notch is a notch to the child, and a
            // trackpad's fractions have already added up here.
            for _ in 0..lines.abs() {
                if let Err(error) = self.session.write(&report) {
                    tracing::warn!(%error, "wheel report failed");
                    break;
                }
            }
            cx.notify();
            return;
        }
        if self.session.on_alternate_screen() {
            // Up and down, one per line, which is what xterm's "alternate scroll" sends: a pager or an
            // editor on the alternate screen has no history of ours to move, and this is how a
            // wheel has always scrolled them.
            let key: &[u8] = if lines > 0 { b"\x1b[A" } else { b"\x1b[B" };
            for _ in 0..lines.abs() {
                if let Err(error) = self.session.write(key) {
                    tracing::warn!(%error, "wheel arrow failed");
                    break;
                }
            }
            cx.notify();
            return;
        }
        // The selection goes with the text it was drawn around: alacritty
        // rotates it by the same offset, because it is anchored in the grid.
        self.session.scroll(lines);
        cx.notify();
    }

    fn handle_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus, cx);
        // The listener is registered for the left button; a press outside the
        // grid selects nothing (see `inside_grid`).
        if !self.inside_grid(event.position) {
            return;
        }
        // A child that asked for the mouse gets the press unless the platform modifier is held.
        if self.child_has_the_mouse(&event.modifiers) && !event.modifiers.platform {
            self.report_mouse(
                MouseReport::Press(Report::Left),
                event.position,
                &event.modifiers,
            );
            return;
        }
        let Some((point, side)) = self.grid_point(event.position) else {
            return;
        };
        if event.modifiers.platform {
            let url = self.session.with_term(|term| terminal_url_at(term, point));
            if let Some(url) = url {
                cx.open_url(&url);
                return;
            }
        }
        // A second click takes the word, a third the line, as everywhere else.
        let kind = match event.click_count {
            2 => SelectionType::Semantic,
            3.. => SelectionType::Lines,
            _ => SelectionType::Simple,
        };
        self.dragging = true;
        self.session
            .with_term(|term| term.selection = Some(Selection::new(kind, point, side)));
        cx.notify();
    }

    /// Autoscroll a drag that has left the grid, by how far out it is: a press that is pulled past
    /// an edge has to bring the history with it, or the selection stops at whatever happened to
    /// be on screen.
    fn autoscroll(&self, position: Point<Pixels>) {
        let layout = self.layout.get();
        if layout.screen_lines == 0 {
            return;
        }
        let height = self.metrics.cell.height.as_f32().max(1.);
        let bottom = layout.origin.y + self.metrics.cell.height * layout.screen_lines as f32;
        let outside = if position.y < layout.origin.y {
            position.y - layout.origin.y
        } else if position.y > bottom {
            position.y - bottom
        } else {
            return;
        };
        // Whole cells out, and never more than a few per move: a pointer flung
        // far outside should scroll faster, not teleport.
        let cells = (outside.as_f32() / height).abs().ceil().clamp(1., 3.);
        let lines = if outside < px(0.) {
            cells as i32
        } else {
            -(cells as i32)
        };
        self.session.scroll(lines);
    }

    /// One step of a drag: pull the viewport in if the pointer has left the grid, then point the
    /// selection at the cell it is over.
    fn drag_to(&mut self, position: Point<Pixels>, modifiers: &Modifiers, cx: &mut Context<Self>) {
        // A child that asked for drag reports (`?1002`, `?1003`) gets them: the pointer moving with a
        // button down is its business, and the selection is not ours to move.
        if self.child_has_the_mouse(modifiers) {
            if self.session.mouse_reports_drag() {
                self.report_mouse(MouseReport::Motion(Some(Report::Left)), position, modifiers);
            }
            return;
        }
        if !self.dragging {
            return;
        }
        self.autoscroll(position);
        // The step happened first, so `grid_point` reads the offset the scroll
        // just moved to and the selection reaches the row that came into view.
        let Some((point, side)) = self.grid_point(position) else {
            return;
        };
        // `grid_point` clamps, so a drag that leaves the grid keeps selecting
        // the edge cell instead of stopping half a word short.
        self.session.with_term(|term| {
            if let Some(selection) = term.selection.as_mut() {
                selection.update(point, side);
            }
        });
        cx.notify();
    }

    fn handle_mouse_up(
        &mut self,
        event: &MouseUpEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.dragging = false;
        if self.child_has_the_mouse(&event.modifiers) {
            self.report_mouse(
                MouseReport::Release(Report::Left),
                event.position,
                &event.modifiers,
            );
            cx.notify();
        }
    }

    /// A path dropped on the pane: the tree's half of the drag, and the whole of the terminal's.
    fn handle_path_drop(
        &mut self,
        dragged: &DraggedPath,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let text = shell_escape(&dragged.0);
        if let Err(error) = self.session.write(text.as_bytes()) {
            tracing::warn!(%error, "terminal write failed");
            return;
        }
        self.observe_input(text.as_bytes(), cx);
        // A drop is the reader doing something deliberate to this pane, so the
        // keyboard belongs here afterwards — the terminal only takes it on a
        // press otherwise.
        window.focus(&self.focus, cx);
        cx.notify();
    }

    /// Whether `position` is over the grid at all. A press that starts outside
    /// it selects nothing, which is what keeps a click in the margin from
    /// wiping a selection the reader is still reading.
    fn inside_grid(&self, position: Point<Pixels>) -> bool {
        let layout = self.layout.get();
        let x = position.x - layout.origin.x;
        let y = position.y - layout.origin.y;
        x >= px(0.)
            && y >= px(0.)
            && x < self.metrics.cell.width * layout.columns as f32
            && y < self.metrics.cell.height * layout.screen_lines as f32
    }

    /// The grid cell under `position`, clamped to the grid, with which half of
    /// the cell the pointer is in — the half is what decides whether the first
    /// or the last cell of the selection counts.
    fn grid_point(&self, position: Point<Pixels>) -> Option<(GridPoint, Side)> {
        let layout = self.layout.get();
        if layout.columns == 0 || layout.screen_lines == 0 {
            return None;
        }
        let cell = self.metrics.cell;
        let width = cell.width.as_f32().max(1.);
        let height = cell.height.as_f32().max(1.);
        let x = (position.x - layout.origin.x).as_f32() / width;
        let y = (position.y - layout.origin.y).as_f32() / height;

        let column = (x.floor().max(0.) as usize).min(layout.columns - 1);
        let screen_line = (y.floor().max(0.) as usize).min(layout.screen_lines - 1);
        // The viewport is scrolled: screen row 0 is a line up in the history.
        let display_offset = self.session.with_term(|term| term.grid().display_offset());
        let line = Line(screen_line as i32 - display_offset as i32);
        let side = if x - column as f32 > 0.5 {
            Side::Right
        } else {
            Side::Left
        };

        Some((GridPoint::new(line, Column(column)), side))
    }

    /// Open the pane's menu where the right-click was.
    fn open_menu(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.menu = Some(event.position);
        window.focus(&self.focus, cx);
        cx.notify();
    }

    fn close_menu(&mut self, cx: &mut Context<Self>) {
        self.menu = None;
        cx.notify();
    }

    /// Do what an entry says. The menu closes first: every one of these changes
    /// what is on screen, and a menu floating over the new layout is the kind of
    /// thing that makes a UI feel broken.
    fn run_menu(&mut self, item: PaneMenu, window: &mut Window, cx: &mut Context<Self>) {
        self.menu = None;
        match item {
            PaneMenu::Copy => self.copy_selection(cx),
            PaneMenu::Paste => self.paste(cx),
            // The splits and the close are the app's own actions, handled at the
            // app level — which is the same road the keybindings take, so the
            // menu cannot drift from what `⌘D` does.
            PaneMenu::SplitRight => window.dispatch_action(Box::new(crate::split::Right), cx),
            PaneMenu::SplitDown => window.dispatch_action(Box::new(crate::split::Down), cx),
            PaneMenu::ClosePane => window.dispatch_action(Box::new(crate::split::ClosePane), cx),
        }
        cx.notify();
    }

    /// The menu, in a layer above the grid: a menu drawn inside the pane's own
    /// element would be clipped at the pane's edge, which is exactly where a
    /// menu near the edge has to be.
    fn menu_overlay(
        &self,
        at: Point<Pixels>,
        window_size: Size<Pixels>,
        colors: &ColorTokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let this = cx.entity().downgrade();
        let mut panel = div()
            .id("pane-menu")
            .test_support()
            .v_flex()
            .w(px(MENU_WIDTH))
            .p_1()
            .rounded_md()
            .border_1()
            .border_color(colors.border)
            // A readable tint over the window's native blur.
            .bg(crate::settings_view::popup_surface(colors.surface))
            .shadow_lg()
            .text_xs()
            // The whole panel is the menu, its padding and the pixels between two entries included: a click
            // there is a click on the menu, not on the grid it is drawn over.
            .occlude()
            // Above the grid in a layer, so a wheel event would go to the
            // terminal underneath it.
            .on_scroll_wheel(|_event, _window, cx| cx.stop_propagation());
        for item in PaneMenu::ALL {
            if item.opens_group() {
                panel = panel.child(div().my_1().h(px(1.)).w_full().bg(colors.border));
            }
            let enabled = item.enabled(self);
            let view = this.clone();
            panel = panel.child(
                div()
                    .id(item.id())
                    .test_support()
                    .aria_label(item.label())
                    .h_flex()
                    .items_center()
                    .justify_between()
                    .gap_4()
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .text_color(if enabled {
                        colors.foreground
                    } else {
                        colors.muted_foreground
                    })
                    .when(enabled, |el| {
                        el.cursor_pointer()
                            .hover(|el| el.bg(colors.muted))
                            .on_click(move |_event, window, cx| {
                                view.update(cx, |view, cx| view.run_menu(item, window, cx))
                                    .ok();
                            })
                    })
                    .child(SharedString::from(item.label()))
                    .child(
                        div()
                            .text_color(colors.muted_foreground)
                            .child(SharedString::from(item.shortcut())),
                    ),
            );
        }

        // The scrim: a click anywhere outside the menu, on either button,
        // closes it — which is what makes the menu feel like a menu.
        let dismiss = this.clone();
        deferred(
            // The scrim needs a size of its own: an `anchored` box is as big as its child, and a child of
            // `size_full` inside it is a percentage of nothing — a scrim written that way
            // covers no pixels and closes nothing.
            anchored().position(point(px(0.), px(0.))).child(
                div()
                    .id("pane-menu-scrim")
                    .test_support()
                    .w(window_size.width)
                    .h(window_size.height)
                    // Blocks the mouse from everything behind it: the rows the menu is drawn over are not hovered
                    // and not clicked while it is up — without this they were in the same hover
                    // group, so a row highlighted under the pointer and its click opened the
                    // file behind the item that was aimed at.
                    .occlude()
                    .on_mouse_down(MouseButton::Left, move |_event, _window, cx| {
                        dismiss.update(cx, |view, cx| view.close_menu(cx)).ok();
                    })
                    .on_mouse_down(MouseButton::Right, {
                        let dismiss = this.clone();
                        move |_event, _window, cx| {
                            dismiss.update(cx, |view, cx| view.close_menu(cx)).ok();
                        }
                    })
                    .child(
                        anchored()
                            .position(at)
                            .anchor(Anchor::TopLeft)
                            .snap_to_window_with_margin(px(8.))
                            .child(panel),
                    ),
            ),
        )
        .with_priority(gpui_kit::base::POPUP_PRIORITY)
        .into_any_element()
    }

    /// The selection, to the clipboard.
    fn copy_selection(&mut self, cx: &mut Context<Self>) {
        if let Some(text) = self.selection_text() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    /// The clipboard, to the child — wrapped if the child asked for bracketed
    /// paste, which is how a TUI tells a paste from typing.
    fn paste(&mut self, cx: &mut Context<Self>) {
        if let Some(item) = cx.read_from_clipboard() {
            self.paste_item(item, cx);
        }
    }

    /// The same road for a paste the platform hands over rather than a `cmd-v`
    /// we caught: the input handler's `paste`.
    fn paste_item(&mut self, item: ClipboardItem, cx: &mut Context<Self>) {
        let Some(text) = clipboard_paste_text(&item) else {
            return;
        };
        let bracketed = self
            .session
            .with_term(|term| term.mode().contains(TermMode::BRACKETED_PASTE));
        if let Err(error) = self.session.write(&paste_bytes(&text, bracketed)) {
            tracing::warn!(%error, "terminal paste failed");
        }
        self.observe_input(text.as_bytes(), cx);
    }
}

impl Focusable for TerminalView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl PaneItem for TerminalView {
    fn title(&self, _cx: &App) -> SharedString {
        self.title
            .clone()
            .unwrap_or_else(|| t!("pane.terminal").to_string().into())
    }

    /// The tab's status dot (the tab bar reads it; the sidebar reads the state,
    /// which this terminal reported the same reading into).
    fn status(&self, _cx: &App) -> Option<AgentStatus> {
        self.status
    }

    fn to_spec(&self, _cx: &App) -> Option<TabSpec> {
        Some(TabSpec::Terminal {
            cwd: self.cwd.clone(),
            agent: self.agent_id().cloned(),
            // The id is enough: the transcript is found again on the way back.
            session: self
                .resume
                .as_ref()
                .map(|resume| resume.id.clone())
                .or_else(|| self.session_id.clone()),
        })
    }
}

impl Render for TerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let prepaint_session = self.session.clone();
        let paint_session = self.session.clone();
        let metrics = self.metrics.clone();
        let prepaint_metrics = self.metrics.clone();
        let prepaint_layout = self.layout.clone();
        let focus = self.focus.clone();
        let marked = self.marked.clone();
        // Weak, not strong: the frame holds this closure, so a strong handle
        // would be a view holding itself and the view would never drop — and
        // dropping the view is what hangs up the child.
        let handler_view = cx.entity().downgrade();
        let colors = Theme::global(cx).tokens.colors;
        let menu = self.menu;

        let mut root = div()
            .id(self.element_id.clone())
            // Observable by the test harness, the way every chrome control is:
            // a drag lands on this element, and a drop is only assertable if the
            // element it landed on can be pointed at.
            .test_support()
            .size_full()
            .bg(colors.background)
            .track_focus(&self.focus)
            // Tab and Shift-Tab are the child's, not the window's focus order:
            // the keymap cancels the root's bindings in this context (see
            // `app_keymap`), and the keystroke then reaches `handle_key_down` as
            // bytes.
            .key_context("terminal")
            .on_key_down(cx.listener(Self::handle_key_down))
            .on_scroll_wheel(cx.listener(Self::handle_scroll_wheel))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::handle_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::handle_mouse_up))
            // The split entries live in the pane's context menu, which a right-click in the terminal
            // opens.
            .on_mouse_down(MouseButton::Right, cx.listener(Self::open_menu))
            // A path dragged from the file tree lands here. The listener is typed, so a drag carrying
            // anything else is not offered to this element.
            .on_drop(cx.listener(Self::handle_path_drop))
            .child(
                // `size_full` matters: a canvas lays out to its own style, and
                // a bare one is a zero-height block.
                canvas(
                    move |bounds, _window, _cx| {
                        let layout =
                            GridLayout::resolve(bounds, &prepaint_metrics, &prepaint_session);
                        // The pointer handlers read this: the bounds are only
                        // known here, during layout.
                        prepaint_layout.set(layout);
                        layout
                    },
                    move |_bounds, layout, window, cx| {
                        let cursor_bounds = paint_grid(
                            layout,
                            &paint_session,
                            &metrics,
                            marked.as_deref(),
                            window,
                            cx,
                        );
                        // Registered during paint, for the focused view only:
                        window.handle_input(
                            &focus,
                            TerminalInputHandler {
                                view: handler_view.clone(),
                                cursor_bounds,
                            },
                            cx,
                        );
                        // The drag follows the pointer at window level, not on the element: an element's own mouse
                        // events stop at its bounds, and the whole point of autoscroll is the
                        // pointer leaving the grid.
                        let dragging_view = handler_view.clone();
                        window.on_mouse_event(move |event: &MouseMoveEvent, phase, _window, cx| {
                            if phase != DispatchPhase::Capture
                                || event.pressed_button != Some(MouseButton::Left)
                            {
                                return;
                            }
                            dragging_view
                                .update(cx, |view, cx| {
                                    view.drag_to(event.position, &event.modifiers, cx)
                                })
                                .ok();
                        });
                    },
                )
                .size_full(),
            );
        if let Some(at) = menu {
            root = root.child(self.menu_overlay(at, window.bounds().size, &colors, cx));
        }
        root
    }
}

fn terminal_url_at(term: &Term<EventProxy>, point: GridPoint) -> Option<String> {
    let grid = term.grid();
    if let Some(url) = grid[point].hyperlink().map(|link| link.uri().to_owned()) {
        if url.starts_with("https://") || url.starts_with("http://") {
            return Some(url);
        }
    }
    let row = &grid[point.line];
    let chars: Vec<char> = (0..grid.columns())
        .map(|column| row[Column(column)].c)
        .collect();
    url_in_chars(&chars, point.column.0)
}

fn url_in_chars(chars: &[char], column: usize) -> Option<String> {
    if column >= chars.len() || chars[column].is_whitespace() {
        return None;
    }
    let mut start = column;
    let mut end = column + 1;
    while start > 0 && !chars[start - 1].is_whitespace() && !"<>\"'".contains(chars[start - 1]) {
        start -= 1;
    }
    while end < chars.len() && !chars[end].is_whitespace() && !"<>\"'".contains(chars[end]) {
        end += 1;
    }
    while start < end && "([{".contains(chars[start]) {
        start += 1;
    }
    while end > start && ".,;:!?)]}".contains(chars[end - 1]) {
        end -= 1;
    }
    if column < start || column >= end {
        return None;
    }
    let url: String = chars[start..end].iter().collect();
    (url.starts_with("https://") || url.starts_with("http://")).then_some(url)
}

/// The terminal's side of the platform's text-input protocol — the other half of
/// [`key_to_bytes`], for text that is not a keystroke at all.
struct TerminalInputHandler {
    view: WeakEntity<TerminalView>,
    /// Where the cursor was last painted. Only the paint pass knows it, so the
    /// handler is built there with it.
    cursor_bounds: Option<Bounds<Pixels>>,
}

impl InputHandler for TerminalInputHandler {
    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Option<UTF16Selection> {
        // A terminal has no text selection to mirror: whatever is selected is
        // the grid's, not the platform's, and the IME only needs an anchor to
        // hang its candidate window off — which `bounds_for_range` answers.
        Some(UTF16Selection {
            range: 0..0,
            reversed: false,
        })
    }

    fn marked_text_range(
        &mut self,
        _window: &mut Window,
        cx: &mut App,
    ) -> Option<std::ops::Range<usize>> {
        self.view
            .update(cx, |view, _| view.marked_text_range())
            .ok()
            .flatten()
    }

    fn text_for_range(
        &mut self,
        _range_utf16: std::ops::Range<usize>,
        _adjusted_range: &mut Option<std::ops::Range<usize>>,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Option<String> {
        // The composed text is the only document this handler has, and the
        // platform already knows it: it wrote it.
        None
    }

    fn replace_text_in_range(
        &mut self,
        _replacement_range: Option<std::ops::Range<usize>>,
        text: &str,
        _window: &mut Window,
        cx: &mut App,
    ) {
        self.view
            .update(cx, |view, cx| view.commit_text(text, cx))
            .ok();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _range_utf16: Option<std::ops::Range<usize>>,
        new_text: &str,
        _new_selected_range: Option<std::ops::Range<usize>>,
        _window: &mut Window,
        cx: &mut App,
    ) {
        self.view
            .update(cx, |view, cx| view.set_marked_text(new_text, cx))
            .ok();
    }

    fn unmark_text(&mut self, _window: &mut Window, cx: &mut App) {
        self.view
            .update(cx, |view, cx| view.clear_marked_text(cx))
            .ok();
    }

    fn paste(&mut self, item: ClipboardItem, _window: &mut Window, cx: &mut App) {
        // The platform's own paste, when it delivers one: the same road as
        // `cmd-v`, so bracketed paste still tells the child it was a paste.
        self.view
            .update(cx, |view, cx| view.paste_item(item, cx))
            .ok();
    }

    fn bounds_for_range(
        &mut self,
        _range_utf16: std::ops::Range<usize>,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Option<Bounds<Pixels>> {
        self.cursor_bounds
    }

    fn character_index_for_point(
        &mut self,
        _point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Option<usize> {
        None
    }

    fn prefers_ime_for_printable_keys(&mut self, _window: &mut Window, _cx: &mut App) -> bool {
        // Without this, a printable key never reaches the input context and a composition can never
        // start: the first key of one arrives as a keystroke like any other.
        true
    }
}

/// Draw one frame of the grid: backgrounds, then the text of each row that has
/// any, then the cursor. Answers with where the cursor was painted, which is
/// where the platform puts the IME candidate window.
fn paint_grid(
    layout: GridLayout,
    session: &TermSession,
    metrics: &CellMetrics,
    marked: Option<&str>,
    window: &mut Window,
    cx: &mut App,
) -> Option<Bounds<Pixels>> {
    let colors = Theme::global(cx).tokens.colors;
    let palette = Palette::new(colors);
    let text_system = window.text_system().clone();

    session.with_term(|term| {
        paint_backgrounds(term, layout, metrics, &palette, window);
        paint_selection(term, layout, metrics, &palette, window);
        paint_rows(term, layout, metrics, &palette, &text_system, window, cx);
        // The composition sits where the cursor is, so it replaces the cursor
        // rather than joining it: one of the two is the insertion point, and
        // while the user is composing it is the composition.
        if let Some(marked) = marked {
            let bounds = paint_marked(term, layout, metrics, marked, &text_system, window, cx);
            return bounds.or_else(|| {
                paint_cursor(term, layout, metrics, &palette, &text_system, window, cx)
            });
        }
        paint_cursor(term, layout, metrics, &palette, &text_system, window, cx)
    })
}

/// The IME's pre-edit text, over the cell the cursor is on: the ground first so
/// the glyph under it is gone, then the text underlined, which is how every
/// other text field says "this is not committed yet".
fn paint_marked(
    term: &Term<EventProxy>,
    layout: GridLayout,
    metrics: &CellMetrics,
    marked: &str,
    text_system: &WindowTextSystem,
    window: &mut Window,
    cx: &mut App,
) -> Option<Bounds<Pixels>> {
    let content = term.renderable_content();
    // Scrolled into the history, nothing is composing at the top of the screen.
    if content.display_offset != 0 {
        return None;
    }
    let point = content.cursor.point;
    let grid = term.grid();
    if point.line.0 < 0
        || point.line.0 as usize >= grid.screen_lines()
        || point.column.0 >= grid.columns()
    {
        return None;
    }

    let run = TextRun {
        len: marked.len(),
        font: metrics.font.clone(),
        color: Theme::global(cx).tokens.colors.foreground,
        underline: Some(UnderlineStyle {
            color: None,
            thickness: px(1.),
            wavy: false,
        }),
        ..Default::default()
    };
    // No forced width: the composition is the user's text, laid out the way the
    // font lays it out, not snapped to a grid it has not been committed to.
    let shaped = text_system.shape_line(marked.into(), metrics.font_size, &[run], None);
    let width = shaped.width().max(metrics.cell.width);
    // `ponytail:` one row, clipped at the grid edge. A composition longer than
    // the row is rare and the platform clips it too; wrap it if that ever bites.
    let bounds = Bounds::new(
        layout
            .cells(metrics, point.line.0 as usize, point.column.0, 1)
            .origin,
        size(width, metrics.cell.height),
    );
    window.paint_quad(fill(bounds, Theme::global(cx).tokens.colors.background));
    if let Err(error) = shaped.paint(
        bounds.origin,
        metrics.cell.height,
        TextAlign::Left,
        None,
        window,
        cx,
    ) {
        tracing::warn!(%error, "terminal composition paint failed");
    }
    Some(bounds)
}

/// The selection, one quad per row, under the text so a selected glyph keeps
/// its own colour and only the ground beneath it changes.
fn paint_selection(
    term: &Term<EventProxy>,
    layout: GridLayout,
    metrics: &CellMetrics,
    palette: &Palette,
    window: &mut Window,
) {
    for (screen_line, from, to) in selection_spans(term) {
        window.paint_quad(fill(
            layout.cells(metrics, screen_line, from, to - from + 1),
            palette.selection,
        ));
    }
}

/// The selected columns of each screen row, inclusive, top to bottom.
fn selection_spans(term: &Term<EventProxy>) -> Vec<(usize, usize, usize)> {
    let Some(range) = term
        .selection
        .as_ref()
        .and_then(|selection| selection.to_range(term))
    else {
        return Vec::new();
    };

    let grid = term.grid();
    let columns = grid.columns();
    let offset = grid.display_offset() as i32;
    let mut spans = Vec::new();

    for screen_line in 0..grid.screen_lines() {
        let line = screen_line as i32 - offset;
        if line < range.start.line.0 || line > range.end.line.0 {
            continue;
        }
        // A block selection is the same columns on every row; the others run
        // from the start column to the end of the row and on to the end column.
        let (from, to) = if range.is_block {
            (range.start.column.0, range.end.column.0)
        } else {
            (
                if line == range.start.line.0 {
                    range.start.column.0
                } else {
                    0
                },
                if line == range.end.line.0 {
                    range.end.column.0
                } else {
                    columns - 1
                },
            )
        };
        if from <= to {
            spans.push((screen_line, from, to));
        }
    }
    spans
}

/// One quad per run of same-coloured cells, not one per cell: a full-width
/// background is one quad, not two hundred.
fn paint_backgrounds(
    term: &Term<EventProxy>,
    layout: GridLayout,
    metrics: &CellMetrics,
    palette: &Palette,
    window: &mut Window,
) {
    let grid = term.grid();
    let offset = grid.display_offset();
    let columns = grid.columns();

    for screen_line in 0..grid.screen_lines() {
        let row = &grid[Line(screen_line as i32 - offset as i32)];
        let mut column = 0;
        while column < columns {
            let background = palette.resolve(&row[Column(column)]).1;
            let start = column;
            column += 1;
            while column < columns && palette.resolve(&row[Column(column)]).1 == background {
                column += 1;
            }
            // The element is already filled with the theme background.
            if background == palette.background {
                continue;
            }
            window.paint_quad(fill(
                layout.cells(metrics, screen_line, start, column - start),
                background,
            ));
        }
    }
}

/// One shaped line per row. The row's text is the only thing built per frame,
/// and only up to the last cell that has something in it.
#[allow(clippy::too_many_arguments)]
fn paint_rows(
    term: &Term<EventProxy>,
    layout: GridLayout,
    metrics: &CellMetrics,
    palette: &Palette,
    text_system: &WindowTextSystem,
    window: &mut Window,
    cx: &mut App,
) {
    let grid = term.grid();
    let offset = grid.display_offset();
    let columns = grid.columns();

    for screen_line in 0..grid.screen_lines() {
        let line = Line(screen_line as i32 - offset as i32);
        let row = &grid[line];
        let Some(end_column) = last_inked_column(grid, line, columns) else {
            continue;
        };

        let mut text = String::new();
        let mut runs: Vec<TextRun> = Vec::new();
        let mut style: Option<RunStyle> = None;
        let mut run_len = 0usize;

        for column in 0..=end_column {
            let cell = &row[Column(column)];
            // A wide character is one glyph over two columns; its spacer is the
            // second column, already covered by that glyph's advance.
            if cell
                .flags
                .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
            {
                continue;
            }

            let (color, _) = palette.resolve(cell);
            let next = RunStyle::of(cell, color);
            if style.as_ref() != Some(&next) {
                if let Some(previous) = style.replace(next) {
                    runs.push(previous.run(run_len, metrics));
                    run_len = 0;
                }
            }

            // `HIDDEN` is a cell whose text the child wants unseen; a space in
            // its place keeps the row in the columns it was laid out in.
            let glyph = if cell.flags.contains(Flags::HIDDEN) {
                ' '
            } else {
                cell.c
            };
            text.push(glyph);
            run_len += glyph.len_utf8();
            // A wide glyph is one glyph over two cells, and `force_width` advances one slot per glyph — so
            // without an empty slot of its own the second cell is never accounted for and every
            // later cell in the row slides one column left.
            if cell.flags.contains(Flags::WIDE_CHAR) {
                text.push(' ');
                run_len += 1;
            }
            if let Some(zerowidth) = cell.zerowidth() {
                for mark in zerowidth {
                    text.push(*mark);
                    run_len += mark.len_utf8();
                }
            }
        }

        let Some(last) = style else { continue };
        runs.push(last.run(run_len, metrics));

        let shaped = text_system.shape_line(
            text.into(),
            metrics.font_size,
            &runs,
            // Snaps every glyph to its own cell: the grid is a cell grid, and a
            // font's own advances are not. This is what Zed's terminal does.
            Some(metrics.cell.width),
        );
        let origin = Point::new(
            layout.origin.x,
            layout.origin.y + metrics.cell.height * screen_line as f32,
        );
        if let Err(error) = shaped.paint(
            origin,
            metrics.cell.height,
            TextAlign::Left,
            None,
            window,
            cx,
        ) {
            tracing::warn!(%error, "terminal row paint failed");
        }
    }
}

/// The cursor, drawn over the text it sits on: a block cursor re-draws its own
/// cell in the background colour, which is what makes it readable.
fn paint_cursor(
    term: &Term<EventProxy>,
    layout: GridLayout,
    metrics: &CellMetrics,
    palette: &Palette,
    text_system: &WindowTextSystem,
    window: &mut Window,
    cx: &mut App,
) -> Option<Bounds<Pixels>> {
    let content = term.renderable_content();
    // Scrolled into the history, the cursor is below the viewport.
    if content.display_offset != 0 {
        return None;
    }
    let cursor = content.cursor;
    let row = cursor.point.line.0;
    let column = cursor.point.column.0;
    let grid = term.grid();
    if row < 0 || row as usize >= grid.screen_lines() || column >= grid.columns() {
        return None;
    }

    let cell = layout.cells(metrics, row as usize, column, 1);
    // A hidden cursor is not drawn, but it still answers where the IME goes: TUIs
    // that draw their own cursor (pi among them) hide the terminal's and park it
    // at the insertion point for exactly this. Without an answer the platform
    // puts the candidate window in the corner of the screen.
    if cursor.shape == CursorShape::Hidden {
        return Some(cell);
    }
    let block = match cursor.shape {
        CursorShape::Beam => Bounds::new(cell.origin, size(px(2.), metrics.cell.height)),
        CursorShape::Underline => Bounds::new(
            Point::new(cell.origin.x, cell.origin.y + metrics.cell.height - px(2.)),
            size(metrics.cell.width, px(2.)),
        ),
        _ => cell,
    };
    window.paint_quad(fill(block, palette.cursor));

    if !matches!(cursor.shape, CursorShape::Block) {
        return Some(block);
    }
    let glyph = grid[Line(row)][Column(column)].c;
    if glyph == ' ' {
        return Some(block);
    }
    let run = TextRun {
        len: glyph.len_utf8(),
        font: metrics.font.clone(),
        color: palette.background,
        ..Default::default()
    };
    let shaped = text_system.shape_line(glyph.to_string().into(), metrics.font_size, &[run], None);
    if let Err(error) = shaped.paint(
        cell.origin,
        metrics.cell.height,
        TextAlign::Left,
        None,
        window,
        cx,
    ) {
        tracing::warn!(%error, "terminal cursor paint failed");
    }
    Some(block)
}

/// The last column of `line` that has anything to draw, so a blank row costs
/// nothing. `None` when the whole row is untouched cells.
fn last_inked_column(grid: &Grid<Cell>, line: Line, columns: usize) -> Option<usize> {
    for column in (0..columns).rev() {
        let cell = &grid[line][Column(column)];
        let untouched = cell.c == ' '
            && cell.zerowidth().is_none()
            && cell.flags.is_empty()
            && cell.fg == AnsiColor::Named(NamedColor::Foreground)
            && cell.bg == AnsiColor::Named(NamedColor::Background);
        if !untouched {
            return Some(column);
        }
    }
    None
}

/// The parts of a cell's style that a `TextRun` carries. A change in any of
/// them ends the current run and starts a new one.
#[derive(Clone, PartialEq)]
struct RunStyle {
    color: Hsla,
    bold: bool,
    italic: bool,
    underline: bool,
    strikeout: bool,
}

impl RunStyle {
    fn of(cell: &Cell, color: Hsla) -> Self {
        Self {
            color,
            bold: cell.flags.intersects(Flags::BOLD | Flags::DIM_BOLD),
            italic: cell.flags.contains(Flags::ITALIC),
            underline: cell.flags.intersects(Flags::ALL_UNDERLINES),
            strikeout: cell.flags.contains(Flags::STRIKEOUT),
        }
    }

    fn run(&self, len: usize, metrics: &CellMetrics) -> TextRun {
        let mut font = metrics.font.clone();
        if self.bold {
            font = font.bold();
        }
        if self.italic {
            font = font.italic();
        }
        TextRun {
            len,
            font,
            color: self.color,
            underline: self.underline.then(|| UnderlineStyle {
                color: Some(self.color),
                thickness: px(1.),
                wavy: false,
            }),
            strikethrough: self.strikeout.then(|| StrikethroughStyle {
                color: Some(self.color),
                thickness: px(1.),
            }),
            ..Default::default()
        }
    }
}

/// The theme's own two roles, plus the standard xterm table for everything the
/// child names. One palette instead of three, so there is one place to look
/// when a colour is wrong.
#[derive(Clone, Copy)]
struct Palette {
    background: Hsla,
    foreground: Hsla,
    cursor: Hsla,
    selection: Hsla,
    /// Whether the background is light, which swaps the sixteen named colours
    /// for ones that read on it (see [`LIGHT_BASE`]).
    light: bool,
}

impl Palette {
    fn new(colors: ColorTokens) -> Self {
        Self {
            background: colors.background,
            foreground: colors.foreground,
            // A terminal cursor is drawn in the text colour, and its glyph in
            // the background's: that pair is readable in either appearance.
            cursor: colors.foreground,
            // The theme's own selection role, which is translucent in both
            // appearances — so the cell's own colours still read through it.
            selection: colors.selection,
            light: colors.background.l > 0.5,
        }
    }

    /// A cell's colours, as `(foreground, background)`.
    fn resolve(&self, cell: &Cell) -> (Hsla, Hsla) {
        let mut foreground = self.foreground_of(cell.fg);
        let mut background = self.background_of(cell.bg);
        if cell.flags.contains(Flags::INVERSE) {
            std::mem::swap(&mut foreground, &mut background);
        }
        if cell.flags.contains(Flags::DIM) {
            foreground = foreground.opacity(0.6);
        }
        (foreground, background)
    }

    fn foreground_of(&self, color: AnsiColor) -> Hsla {
        match color {
            AnsiColor::Named(NamedColor::Foreground) => self.foreground,
            AnsiColor::Named(NamedColor::Background) => self.background,
            AnsiColor::Named(NamedColor::Cursor) => self.cursor,
            AnsiColor::Named(named) => self.indexed(named_index(named)),
            AnsiColor::Spec(rgb) => ansi_rgb(rgb),
            AnsiColor::Indexed(index) => self.indexed(index),
        }
    }

    fn background_of(&self, color: AnsiColor) -> Hsla {
        match color {
            AnsiColor::Named(NamedColor::Background) => self.background,
            AnsiColor::Named(NamedColor::Foreground) => self.foreground,
            other => self.foreground_of(other),
        }
    }

    /// An xterm index, with the named sixteen taken from the light table on a
    /// light background: xterm's yellow and white are written for a dark one.
    fn indexed(&self, index: u8) -> Hsla {
        match LIGHT_BASE.get(index as usize) {
            Some(&rgb) if self.light => rgb_hsla(rgb),
            _ => xterm256(index),
        }
    }
}

/// The named sixteen for a light background: GitHub Light's terminal colours,
/// where every one, white and bright white included, reads as text on white.
const LIGHT_BASE: [(u8, u8, u8); 16] = [
    (0x24, 0x29, 0x2f),
    (0xcf, 0x22, 0x2e),
    (0x11, 0x63, 0x29),
    (0x4d, 0x2d, 0x00),
    (0x09, 0x69, 0xda),
    (0x82, 0x50, 0xdf),
    (0x1b, 0x7c, 0x83),
    (0x6e, 0x77, 0x81),
    (0x57, 0x60, 0x6a),
    (0xa4, 0x0e, 0x26),
    (0x1a, 0x7f, 0x37),
    (0x63, 0x3c, 0x01),
    (0x21, 0x8b, 0xff),
    (0xa4, 0x75, 0xf9),
    (0x31, 0x92, 0xaa),
    (0x8c, 0x95, 0x9f),
];

/// A `NamedColor` is an index into the same table, except for the three roles
/// the theme owns — those never reach here.
fn named_index(named: NamedColor) -> u8 {
    let index = named as usize;
    if index <= 15 {
        return index as u8;
    }
    // The dim set is the base sixteen again, at 259 and up: the enum reserves
    // 256..259 for foreground, background and cursor.
    if index >= 259 {
        return ((index - 259) % 16) as u8;
    }
    7
}

fn rgb_hsla((r, g, b): (u8, u8, u8)) -> Hsla {
    Rgba {
        r: r as f32 / 255.,
        g: g as f32 / 255.,
        b: b as f32 / 255.,
        a: 1.,
    }
    .into()
}

fn ansi_rgb(rgb: AnsiRgb) -> Hsla {
    Rgba {
        r: rgb.r as f32 / 255.,
        g: rgb.g as f32 / 255.,
        b: rgb.b as f32 / 255.,
        a: 1.,
    }
    .into()
}

/// The xterm 256-colour table: sixteen named, a 6×6×6 cube, then twenty-four
/// greys. The child's `Indexed` colours and the named sixteen both land here.
fn xterm256(index: u8) -> Hsla {
    const BASE: [(u8, u8, u8); 16] = [
        (0x00, 0x00, 0x00),
        (0xcd, 0x00, 0x00),
        (0x00, 0xcd, 0x00),
        (0xcd, 0xcd, 0x00),
        (0x00, 0x00, 0xee),
        (0xcd, 0x00, 0xcd),
        (0x00, 0xcd, 0xcd),
        (0xe5, 0xe5, 0xe5),
        (0x7f, 0x7f, 0x7f),
        (0xff, 0x00, 0x00),
        (0x00, 0xff, 0x00),
        (0xff, 0xff, 0x00),
        (0x5c, 0x5c, 0xff),
        (0xff, 0x00, 0xff),
        (0x00, 0xff, 0xff),
        (0xff, 0xff, 0xff),
    ];
    const STEPS: [u8; 6] = [0, 95, 135, 175, 215, 255];

    let (r, g, b) = match index {
        0..=15 => BASE[index as usize],
        16..=231 => {
            let offset = index as u16 - 16;
            let step = |part: u16| STEPS[(part % 6) as usize];
            (step(offset / 36), step(offset / 6), step(offset))
        }
        _ => {
            let grey = 8 + 10 * (index as u16 - 232);
            (grey as u8, grey as u8, grey as u8)
        }
    };
    Rgba {
        r: r as f32 / 255.,
        g: g as f32 / 255.,
        b: b as f32 / 255.,
        a: 1.,
    }
    .into()
}

/// One keystroke as the bytes a shell expects, or `None` for a key that is not the terminal's:
/// today that means anything held with the platform modifier, which belongs to the app's keymap
/// (Cmd-S, Cmd-T), not to the child.
pub fn key_to_bytes(keystroke: &Keystroke) -> Option<Vec<u8>> {
    let modifiers = keystroke.modifiers;
    if modifiers.platform {
        return None;
    }
    let key = keystroke.key.as_str();

    // Control characters first: `ctrl-c` must not fall through to the letter.
    if modifiers.control
        && let Some(control) = control_byte(key, keystroke.key_char.as_deref())
    {
        return Some(with_alt(vec![control], modifiers.alt));
    }

    if let Some(encoded) = named_key(key, modifiers.shift) {
        return Some(with_alt(encoded, modifiers.alt));
    }

    // What the keyboard actually typed, which is where shift and layouts have
    // already been applied (`shift-a` is "A", `alt-s` on a Mac is "ß").
    if let Some(typed) = keystroke.key_char.as_deref()
        && !typed.is_empty()
    {
        return Some(with_alt(typed.as_bytes().to_vec(), modifiers.alt));
    }
    // No character: the key is only usable if it is one, and a name like
    // `shift` or `media-play` is not.
    let mut chars = key.chars();
    let ch = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    Some(with_alt(ch.to_string().into_bytes(), modifiers.alt))
}

/// `ctrl` + a key, as the byte the terminal protocol assigns it: the letters
/// and `space` are `0x00`–`0x1f`, and the punctuation around them continues the
/// range.
fn control_byte(key: &str, typed: Option<&str>) -> Option<u8> {
    // `space` is the one named key that has a control byte, and its `key_char`
    // is a space anyway, so the name is what to match.
    if key == "space" {
        return Some(0x00);
    }
    let key = typed.unwrap_or(key);
    let mut chars = key.chars();
    let ch = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    match ch {
        'a'..='z' => Some(ch as u8 - b'a' + 1),
        'A'..='Z' => Some(ch as u8 - b'A' + 1),
        ' ' | '@' => Some(0x00),
        '[' => Some(0x1b),
        '\\' => Some(0x1c),
        ']' => Some(0x1d),
        '^' => Some(0x1e),
        '_' => Some(0x1f),
        _ => None,
    }
}

/// The keys that are not characters. `shift` only matters for `tab`, which is
/// the one key with a second meaning (`CSI Z`, back-tab).
fn named_key(key: &str, shift: bool) -> Option<Vec<u8>> {
    let bytes: &[u8] = match key {
        "enter" => b"\r",
        "tab" if shift => b"\x1b[Z",
        "tab" => b"\t",
        "backspace" => b"\x7f",
        "escape" => b"\x1b",
        "space" => b" ",
        "up" => b"\x1b[A",
        "down" => b"\x1b[B",
        "right" => b"\x1b[C",
        "left" => b"\x1b[D",
        "home" => b"\x1b[H",
        "end" => b"\x1b[F",
        "pageup" => b"\x1b[5~",
        "pagedown" => b"\x1b[6~",
        "insert" => b"\x1b[2~",
        "delete" => b"\x1b[3~",
        "f1" => b"\x1bOP",
        "f2" => b"\x1bOQ",
        "f3" => b"\x1bOR",
        "f4" => b"\x1bOS",
        "f5" => b"\x1b[15~",
        "f6" => b"\x1b[17~",
        "f7" => b"\x1b[18~",
        "f8" => b"\x1b[19~",
        "f9" => b"\x1b[20~",
        "f10" => b"\x1b[21~",
        "f11" => b"\x1b[23~",
        "f12" => b"\x1b[24~",
        _ => return None,
    };
    Some(bytes.to_vec())
}

/// `alt` is `ESC` before the key, the way a terminal has always said it.
fn with_alt(mut bytes: Vec<u8>, alt: bool) -> Vec<u8> {
    if alt {
        bytes.insert(0, 0x1b);
    }
    bytes
}

/// The two keystrokes the app answers itself instead of the child.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardKey {
    Copy,
    Paste,
}

/// A keystroke as a clipboard action, or `None` for one the child should get.
pub fn clipboard_key(keystroke: &Keystroke) -> Option<ClipboardKey> {
    let modifiers = keystroke.modifiers;
    let clipboard = (modifiers.platform || (modifiers.control && modifiers.shift))
        && !modifiers.alt
        && !modifiers.function;
    if !clipboard {
        return None;
    }
    // The key, not the character: a shifted `c` is spelled `C`.
    match keystroke.key.as_str() {
        "c" => Some(ClipboardKey::Copy),
        "v" => Some(ClipboardKey::Paste),
        _ => None,
    }
}

/// Pasted text as the bytes to hand the child.
pub fn paste_bytes(text: &str, bracketed: bool) -> Vec<u8> {
    if !bracketed {
        return text.as_bytes().to_vec();
    }
    let mut bytes = Vec::with_capacity(text.len() + 12);
    bytes.extend_from_slice(b"\x1b[200~");
    bytes.extend(text.bytes().filter(|byte| *byte != 0x1b));
    bytes.extend_from_slice(b"\x1b[201~");
    bytes
}

/// Clipboard images need a file path before a terminal agent can read them.
fn clipboard_paste_text(item: &ClipboardItem) -> Option<String> {
    let text = match item.entries().first() {
        Some(ClipboardEntry::Image(image)) => {
            let path = std::env::temp_dir().join(format!(
                "syro-paste-{}.{}",
                image.id(),
                image.format.extension()
            ));
            if let Err(error) = std::fs::write(&path, &image.bytes) {
                tracing::warn!(%error, "saving pasted image failed");
                return None;
            }
            shell_escape(&path)
        }
        Some(ClipboardEntry::ExternalPaths(paths)) => paths
            .0
            .iter()
            .map(|path| shell_escape(path))
            .collect::<Vec<_>>()
            .join(" "),
        _ => item.text()?,
    };
    (!text.is_empty()).then_some(text)
}

/// A path as a shell will read it back whole.
pub fn shell_escape(path: &Path) -> String {
    let text = path.to_string_lossy();
    if !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '.' | '_' | '-' | '/'))
    {
        return text.into_owned();
    }
    let mut escaped = String::with_capacity(text.len() + 2);
    escaped.push('\'');
    for c in text.chars() {
        if c == '\'' {
            escaped.push_str("'\\''");
        } else {
            escaped.push(c);
        }
    }
    escaped.push('\'');
    escaped
}

/// The one place a composition becomes bytes for the child.

/// `committed` is what the input method handed over: the text on commit, and `None` when the
/// composition ended without one — Escape, a click elsewhere, the input method changing its
/// mind.
pub fn composition_bytes(committed: Option<&str>) -> Option<Vec<u8>> {
    let text = committed?;
    // An empty commit is the input method confirming nothing; the protocol
    // reports it, and there is no byte for it.
    (!text.is_empty()).then(|| text.as_bytes().to_vec())
}

#[cfg(test)]
mod tests {
    // Not `use super::*`: the module above globs gpui in, and gpui's own `test`
    // attribute comes with it — under `cfg(test)` it shadows the built-in one,
    // and every `#[test]` below then expands into itself.
    use gpui_kit::{ClipboardItem, Hsla, Image, ImageFormat, Keystroke};

    use std::path::Path;

    use super::{
        ClipboardKey, Palette, clipboard_key, clipboard_paste_text, composition_bytes, key_to_bytes,
        manual_agent, paste_bytes, shell_escape, url_in_chars, xterm256,
    };

    #[test]
    fn plain_urls_open_only_when_the_clicked_cell_is_inside_the_url() {
        let text = "(https://pi.dev/changelog).";
        let chars: Vec<char> = text.chars().collect();
        assert_eq!(
            url_in_chars(&chars, 10),
            Some("https://pi.dev/changelog".into())
        );
        assert_eq!(url_in_chars(&chars, 0), None);
        assert_eq!(url_in_chars(&chars, chars.len() - 1), None);
        assert_eq!(
            url_in_chars(&"file:///tmp/a".chars().collect::<Vec<_>>(), 8),
            None
        );
    }

    #[test]
    fn manually_started_builtin_agents_are_recognized_from_the_command() {
        assert_eq!(
            manual_agent("codex --yolo").map(|agent| agent.id),
            Some("codex".into())
        );
        assert_eq!(
            manual_agent("claude").map(|agent| agent.id),
            Some("claude".into())
        );
        assert_eq!(
            manual_agent("/opt/homebrew/bin/codex resume").map(|agent| agent.id),
            Some("codex".into())
        );
        // Qoder's preset id and command differ; either spelling is the agent.
        assert_eq!(
            manual_agent("qodercli --resume abc").map(|agent| agent.id),
            Some("qoder".into())
        );
        assert_eq!(
            manual_agent("qoder").map(|agent| agent.id),
            Some("qoder".into())
        );
        assert_eq!(manual_agent("echo codex").map(|agent| agent.id), None);
        assert_eq!(manual_agent("./codex").map(|agent| agent.id), None);
    }

    /// The parser is the one gpui itself reads a keymap with, so the test table
    /// says the same thing a user's binding would.
    fn bytes(binding: &str) -> Option<Vec<u8>> {
        let keystroke = Keystroke::parse(binding).expect("a parsable keystroke");
        key_to_bytes(&keystroke)
    }

    fn text(binding: &str) -> String {
        String::from_utf8(bytes(binding).expect("bytes")).expect("utf-8")
    }

    #[test]
    fn named_keys_use_their_escape_sequences() {
        assert_eq!(bytes("enter"), Some(b"\r".to_vec()));
        assert_eq!(bytes("tab"), Some(b"\t".to_vec()));
        assert_eq!(bytes("shift-tab"), Some(b"\x1b[Z".to_vec()));
        assert_eq!(bytes("backspace"), Some(b"\x7f".to_vec()));
        assert_eq!(bytes("escape"), Some(b"\x1b".to_vec()));
        assert_eq!(bytes("space"), Some(b" ".to_vec()));
        assert_eq!(bytes("up"), Some(b"\x1b[A".to_vec()));
        assert_eq!(bytes("down"), Some(b"\x1b[B".to_vec()));
        assert_eq!(bytes("right"), Some(b"\x1b[C".to_vec()));
        assert_eq!(bytes("left"), Some(b"\x1b[D".to_vec()));
        assert_eq!(bytes("home"), Some(b"\x1b[H".to_vec()));
        assert_eq!(bytes("end"), Some(b"\x1b[F".to_vec()));
        assert_eq!(bytes("pageup"), Some(b"\x1b[5~".to_vec()));
        assert_eq!(bytes("pagedown"), Some(b"\x1b[6~".to_vec()));
        assert_eq!(bytes("delete"), Some(b"\x1b[3~".to_vec()));
        assert_eq!(bytes("f1"), Some(b"\x1bOP".to_vec()));
        assert_eq!(bytes("f12"), Some(b"\x1b[24~".to_vec()));
    }

    #[test]
    fn control_letters_are_their_c0_bytes() {
        assert_eq!(bytes("ctrl-c"), Some(vec![0x03]));
        assert_eq!(bytes("ctrl-d"), Some(vec![0x04]));
        assert_eq!(bytes("ctrl-a"), Some(vec![0x01]));
        assert_eq!(bytes("ctrl-z"), Some(vec![0x1a]));
        assert_eq!(bytes("ctrl-space"), Some(vec![0x00]));
        assert_eq!(bytes("ctrl-["), Some(vec![0x1b]));
        assert_eq!(bytes("ctrl-\\"), Some(vec![0x1c]));
        // Every letter, not a spot check: the range is the whole point.
        for letter in b'a'..=b'z' {
            let binding = format!("ctrl-{}", letter as char);
            assert_eq!(
                bytes(&binding),
                Some(vec![letter - b'a' + 1]),
                "ctrl-{} should be 0x{:02x}",
                letter as char,
                letter - b'a' + 1
            );
        }
    }

    #[test]
    fn alt_prefixes_with_escape() {
        assert_eq!(bytes("alt-x"), Some(b"\x1bx".to_vec()));
        assert_eq!(bytes("alt-up"), Some(b"\x1b\x1b[A".to_vec()));
        assert_eq!(bytes("ctrl-alt-c"), Some(vec![0x1b, 0x03]));
    }

    #[test]
    fn plain_characters_are_what_was_typed() {
        // The `->` form is how gpui's parser spells "this key, that character",
        // which is exactly the shift and layout distinction being tested.
        assert_eq!(text("a->a"), "a");
        assert_eq!(text("shift-a->A"), "A");
        assert_eq!(text("1->!"), "!");
        // `alt` is still `ESC`, even when the layout turned the key into a
        // character of its own: the child reads the modifier, not the glyph.
        assert_eq!(text("alt-s->ß"), "\u{1b}ß");
        // A key with no character of its own still types what it is.
        assert_eq!(text("a"), "a");
    }

    #[test]
    fn the_platform_modifier_belongs_to_the_app() {
        assert_eq!(bytes("cmd-s"), None);
        assert_eq!(bytes("cmd-t"), None);
        assert_eq!(bytes("cmd-ctrl-a"), None);
    }

    #[test]
    fn unknown_keys_are_not_ours() {
        // A bare modifier is a keystroke with a name no shell has a byte for.
        for binding in ["shift", "ctrl", "alt", "cmd", "fn"] {
            assert_eq!(bytes(binding), None, "{binding} should not reach the child");
        }
    }

    #[test]
    fn pasted_images_are_saved_and_pasted_as_absolute_paths() {
        let image = Image::from_bytes(ImageFormat::Png, b"image".to_vec());
        let item = ClipboardItem::new_image(&image);
        let pasted = clipboard_paste_text(&item).unwrap();
        let path = std::env::temp_dir().join(format!("syro-paste-{}.png", image.id()));
        assert!(path.is_absolute());
        assert_eq!(pasted, shell_escape(&path));
        assert_eq!(std::fs::read(&path).unwrap(), b"image");
        let _ = std::fs::remove_file(path);
    }

    /// A shell reads a path back whole only if it is quoted, and the quoting has
    /// to survive the one character a single-quoted string cannot hold.
    #[test]
    fn a_path_is_quoted_when_the_shell_would_read_it_as_more_than_a_path() {
        // Nothing to quote: the shell would take it as-is.
        assert_eq!(shell_escape(Path::new("/tmp/notes.md")), "/tmp/notes.md");
        assert_eq!(shell_escape(Path::new("./a-b_c/d.rs")), "./a-b_c/d.rs");
        // A space is two arguments unless it is quoted, which is the case a
        // dropped file hits the moment someone names a file with one.
        assert_eq!(
            shell_escape(Path::new("/tmp/my notes.md")),
            "'/tmp/my notes.md'"
        );
        // Every one of these means something to `sh` and nothing inside quotes.
        for path in [
            "/tmp/a$b.md",
            "/tmp/a;rm.md",
            "/tmp/a&b.md",
            "/tmp/a|b.md",
            "/tmp/a
b.md",
        ] {
            assert!(
                shell_escape(Path::new(path)).starts_with('\''),
                "{path} was left for the shell to interpret"
            );
        }
        // The quote itself: closed, escaped, reopened — which is how `sh` spells
        // one inside a single-quoted string.
        assert_eq!(
            shell_escape(Path::new("/tmp/it's.md")),
            "'/tmp/it'\\''s.md'"
        );
        // An empty path has nothing bare to be, so it is quoted and empty.
        assert_eq!(shell_escape(Path::new("")), "''");
    }

    /// On a light background every named colour reads as text, white and bright
    /// white included; on a dark one the table is xterm's, and the cube is xterm's
    /// on both.
    #[test]
    fn the_named_colours_read_on_a_light_background() {
        let palette = |light| Palette {
            background: gpui_kit::white(),
            foreground: gpui_kit::black(),
            cursor: gpui_kit::black(),
            selection: gpui_kit::black(),
            light,
        };
        let (light, dark) = (palette(true), palette(false));
        for index in 0..16 {
            // WCAG's contrast against white: 3 is the floor for large text, and
            // xterm's own yellow (1.4) and white (1.3) are nowhere near it.
            let rgb = light.indexed(index).to_rgb();
            let linear = |c: f32| {
                if c <= 0.04045 {
                    c / 12.92
                } else {
                    ((c + 0.055) / 1.055).powf(2.4)
                }
            };
            let luminance =
                0.2126 * linear(rgb.r) + 0.7152 * linear(rgb.g) + 0.0722 * linear(rgb.b);
            let contrast = 1.05 / (luminance + 0.05);
            assert!(
                contrast >= 3.,
                "colour {index} is washed out on white: {contrast}"
            );
            assert_eq!(dark.indexed(index), xterm256(index));
        }
        assert_eq!(light.indexed(16), xterm256(16));
        assert_eq!(light.indexed(255), xterm256(255));
    }

    #[test]
    fn the_xterm_table_has_the_ends_it_should() {
        // The cube's corners and the grey ramp, which is where an off-by-one in
        // the index arithmetic shows up.
        assert_eq!(xterm256(16), xterm256_cube(0, 0, 0));
        assert_eq!(xterm256(21), xterm256_cube(0, 0, 5));
        assert_eq!(xterm256(231), xterm256_cube(5, 5, 5));
        assert_ne!(xterm256(232), xterm256(231));
        assert_ne!(xterm256(255), xterm256(232));
        // The sixteen names are distinct colours, not one palette entry.
        let named: Vec<Hsla> = (0..16).map(xterm256).collect();
        for (index, colour) in named.iter().enumerate() {
            assert_eq!(
                named.iter().filter(|other| *other == colour).count(),
                1,
                "colour {index} repeats another",
            );
        }
    }

    fn xterm256_cube(r: u16, g: u16, b: u16) -> Hsla {
        xterm256((16 + 36 * r + 6 * g + b) as u8)
    }

    #[test]
    fn paste_is_wrapped_only_when_the_child_asked_for_it() {
        assert_eq!(paste_bytes("ls\n", false), b"ls\n".to_vec());
        assert_eq!(
            paste_bytes("ls\n", true),
            b"\x1b[200~ls\n\x1b[201~".to_vec()
        );
        // A clipboard carrying the end marker must not be able to end the paste
        // early and have the rest arrive as typing.
        assert_eq!(
            paste_bytes("a\x1b[201~rm -rf /\nb", true),
            b"\x1b[200~a[201~rm -rf /\nb\x1b[201~".to_vec()
        );
        assert_eq!(paste_bytes("", true), b"\x1b[200~\x1b[201~".to_vec());
    }

    #[test]
    fn only_the_platform_clipboard_keys_are_the_apps() {
        let key = |binding: &str| clipboard_key(&Keystroke::parse(binding).expect("parsable"));

        assert_eq!(key("cmd-c"), Some(ClipboardKey::Copy));
        assert_eq!(key("cmd-v"), Some(ClipboardKey::Paste));
        assert_eq!(key("ctrl-shift-c"), Some(ClipboardKey::Copy));
        assert_eq!(key("ctrl-shift-v"), Some(ClipboardKey::Paste));

        // The interrupt, and the two keys a shell owns.
        assert_eq!(key("ctrl-c"), None);
        assert_eq!(key("ctrl-v"), None);
        assert_eq!(key("c"), None);
        // Another modifier is another binding, and nothing here knows it.
        assert_eq!(key("alt-c"), None);
        assert_eq!(key("cmd-x"), None);
        // And the byte path is untouched by all of it.
        assert_eq!(bytes("ctrl-c"), Some(vec![0x03]));
    }

    #[test]
    fn a_composition_becomes_its_utf8_or_nothing() {
        // Whatever the input method composed: kana, ideographs, an emoji — the
        // child gets the string's UTF-8 bytes, whole and in one piece, because
        // a terminal reads bytes and not characters.
        assert_eq!(
            composition_bytes(Some("日本語")),
            Some("日本語".as_bytes().to_vec())
        );
        assert_eq!(
            composition_bytes(Some("emoji 🐋")),
            Some("emoji 🐋".as_bytes().to_vec())
        );

        // A composition that ended without committing — Escape, a click elsewhere, the input method
        // changing its mind — hands over nothing.
        assert_eq!(composition_bytes(None), None);
        // And a commit of nothing is the same nothing.
        assert_eq!(composition_bytes(Some("")), None);
    }
}

/// The view end to end, in a headless window: a real child behind a real PTY, the canvas that
/// measures the grid, and the key path back down to the child.
#[cfg(test)]
mod window_tests {
    use std::cell::RefCell;
    use std::path::PathBuf;
    use std::rc::Rc;
    use std::time::Duration;

    use gpui_kit::base::{Root, StyledExt as _};
    use gpui_kit::test::TestWindowExt as _;
    // `to_platform_input`, the door a raw platform event goes in through.
    use gpui_kit::InputEvent as _;
    use gpui_kit::{
        AppContext, Bounds, ClipboardItem, Context, Entity, Focusable as _, IntoElement,
        ParentElement as _, Pixels, Point, Render, Styled as _, TestAppContext, Window,
        WindowBounds, WindowHandle, WindowOptions, div, px, size,
    };
    use syro_core::config::Settings;
    use syro_core::model::{AgentSpec, AgentStatus, AgentStatusRule, State, TabSpec};
    use syro_core::store::Store;
    use syro_core::term::{TermSize, TermSpec};

    use super::{PADDING_X, PADDING_Y, SessionRef};

    use alacritty_terminal::index::{Column, Line, Point as GridPoint, Side};
    use alacritty_terminal::selection::{Selection, SelectionType};

    use super::{PaneItem as _, TerminalView, shell_escape, term_spec_for_agent};
    use crate::file_tree::FileTreeView;
    use crate::state::AppState;
    use crate::state::fixture::{scratch_repo, temp_store};

    /// A child that prints something and stops, or keeps reading stdin.
    fn spec(script: &str) -> TermSpec {
        TermSpec {
            command: "/bin/sh".to_string(),
            args: vec!["-c".to_string(), script.to_string()],
            ..TermSpec::shell(std::env::temp_dir(), TermSize::new(80, 24))
        }
    }

    /// An 800x400 window — wider than the 80 columns the spec asks for, so a
    /// grid that still says 80 is a grid the canvas never measured.
    fn open(
        cx: &mut TestAppContext,
        spec: TermSpec,
        agent: Option<AgentSpec>,
    ) -> (WindowHandle<Root>, Entity<TerminalView>) {
        open_seeded(cx, spec, agent, None)
    }

    /// The same window, with the first message a seeded start carries.
    fn open_seeded(
        cx: &mut TestAppContext,
        spec: TermSpec,
        agent: Option<AgentSpec>,
        seed: Option<&str>,
    ) -> (WindowHandle<Root>, Entity<TerminalView>) {
        cx.update(|cx| {
            let bounds = Bounds {
                origin: Point::default(),
                size: size(px(800.), px(400.)),
            };
            let (window, content) = gpui_kit::open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    ..Default::default()
                },
                cx,
                |_, cx| {
                    cx.new(|cx| {
                        TerminalView::new(cx, spec, agent, None, seed).expect("spawn the terminal")
                    })
                },
            )
            .expect("open test window");
            (window.downcast::<Root>().expect("Base Root"), content)
        })
    }

    /// An agent preset whose "CLI" is the title it sets: `sh` prints the OSC title an agent would
    /// and then stays alive the way an agent does.
    fn title_agent(script: &str) -> AgentSpec {
        AgentSpec {
            id: "title-agent".to_string(),
            name: "Title Agent".to_string(),
            command: "/bin/sh".to_string(),
            args: vec!["-c".to_string(), script.to_string()],
            resume_args: Vec::new(),
            new_args: Vec::new(),
            env: Default::default(),
            status: vec![
                AgentStatusRule {
                    pattern: "needs you".to_string(),
                    requires: None,
                    state: AgentStatus::Permission,
                },
                AgentStatusRule {
                    pattern: "working".to_string(),
                    requires: None,
                    state: AgentStatus::Working,
                },
                AgentStatusRule {
                    pattern: "all done".to_string(),
                    requires: None,
                    state: AgentStatus::Idle,
                },
            ],
        }
    }

    /// The OSC title an agent's TUI sets, as the child would print it.
    fn setting_title(title: &str) -> String {
        format!("printf '\\033]0;{title}\\007'")
    }

    /// Draw frames until the grid says what we are waiting for. The child runs
    /// on its own thread, so this is the only way to see its output.
    fn draw_until(
        cx: &mut TestAppContext,
        window: WindowHandle<Root>,
        view: &Entity<TerminalView>,
        wanted: &str,
    ) -> bool {
        // The budget is wall clock, not a frame count.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while std::time::Instant::now() < deadline {
            cx.update_window(window.into(), |_, window, cx| {
                window.draw(cx).clear(cx);
            })
            .expect("draw a frame");
            if cx.update(|cx| view.read(cx).screen().text().contains(wanted)) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    /// Draw frames until the pane says what we are waiting for. The child runs
    /// on its own thread, so this is the only way to see anything it does.
    fn draw_until_status(
        cx: &mut TestAppContext,
        window: WindowHandle<Root>,
        view: &Entity<TerminalView>,
        done: impl Fn(&TerminalView, &gpui_kit::App) -> bool,
    ) -> bool {
        // The budget is wall clock, not a frame count.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while std::time::Instant::now() < deadline {
            // The view's repaint loop is a 16 ms timer, and a test's clock only
            // moves when it is told to: without this the timer never comes due
            // and the view never reads the events the child produced.
            cx.background_executor
                .advance_clock(Duration::from_millis(20));
            cx.run_until_parked();
            cx.update_window(window.into(), |_, window, cx| {
                window.draw(cx).clear(cx);
            })
            .expect("draw a frame");
            if cx.update(|cx| done(view.read(cx), cx)) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    /// The resume flags are the preset's, with the session's own id and path filled in, and they go
    /// after the preset's own args — which is where every agent's CLI wants them.
    #[test]
    fn resume_args_carry_the_session_id_and_path() {
        let mut agent = title_agent("sleep 30");
        agent.args = vec!["--dangerously-skip-permissions".to_string()];
        agent.resume_args = vec!["--session".to_string(), "{path}".to_string()];
        let resume = SessionRef {
            id: "01a09f17".to_string(),
            path: PathBuf::from("/home/ada/.pi/agent/sessions/--x--/a_01a09f17.jsonl"),
        };

        let spec = term_spec_for_agent(
            &agent,
            std::env::temp_dir(),
            TermSize::new(80, 24),
            Some(&resume),
        );
        assert_eq!(
            spec.args,
            vec![
                "--dangerously-skip-permissions",
                "--session",
                "/home/ada/.pi/agent/sessions/--x--/a_01a09f17.jsonl",
            ]
        );

        // An id-shaped flag, and an agent with no flags to add.
        agent.resume_args = vec!["--resume".to_string(), "{id}".to_string()];
        let spec = term_spec_for_agent(
            &agent,
            std::env::temp_dir(),
            TermSize::new(80, 24),
            Some(&resume),
        );
        assert_eq!(spec.args[1..], ["--resume", "01a09f17"]);

        agent.resume_args = Vec::new();
        let spec = term_spec_for_agent(
            &agent,
            std::env::temp_dir(),
            TermSize::new(80, 24),
            Some(&resume),
        );
        assert_eq!(spec.args, ["--dangerously-skip-permissions"]);
        // And without a session at all, nothing is appended.
        let spec = term_spec_for_agent(&agent, std::env::temp_dir(), TermSize::new(80, 24), None);
        assert_eq!(spec.args, ["--dangerously-skip-permissions"]);
    }

    #[test]
    fn a_new_session_id_is_a_v4_uuid() {
        let id = super::new_session_id();
        let parts: Vec<&str> = id.split('-').collect();
        assert_eq!(
            parts.iter().map(|part| part.len()).collect::<Vec<_>>(),
            [8, 4, 4, 4, 12]
        );
        assert!(parts[2].starts_with('4'), "{id}");
        assert!(matches!(&parts[3][..1], "8" | "9" | "a" | "b"), "{id}");
        assert_ne!(id, super::new_session_id());
    }

    /// A preset with `new_args` is started under an id the view picked and keeps.
    #[gpui_kit::test]
    fn a_new_session_is_started_under_the_views_own_id(cx: &mut TestAppContext) {
        let mut agent = title_agent("echo \"id=$0\"; sleep 30");
        agent.new_args = vec!["{id}".to_string()];
        let spec = term_spec_for_agent(&agent, std::env::temp_dir(), TermSize::new(80, 24), None);
        let (window, view) = open(cx, spec, Some(agent));
        let id = cx
            .update(|cx| view.read(cx).session_id.clone())
            .expect("an id");
        assert!(
            draw_until(cx, window, &view, &format!("id={id}")),
            "the child was not handed the id"
        );
        // The id is what a restart resumes by: the tab describes itself with it.
        let spec = cx.update(|cx| view.read(cx).to_spec(cx)).expect("a spec");
        assert!(
            matches!(&spec, TabSpec::Terminal { session: Some(saved), .. } if saved == &id),
            "{spec:?}"
        );
    }

    /// A `{prompt}` in `new_args` is the first message a seeded start carries; a
    /// blank start draws none.
    #[gpui_kit::test]
    fn a_new_session_carries_the_handoff_it_was_given(cx: &mut TestAppContext) {
        let mut agent = title_agent("echo carried=\"$1\"; sleep 30");
        agent.new_args = vec!["{id}".to_string(), "{prompt}".to_string()];
        let spec = term_spec_for_agent(&agent, std::env::temp_dir(), TermSize::new(80, 24), None);
        let (window, view) =
            open_seeded(cx, spec, Some(agent.clone()), Some("handoff from before"));
        assert!(
            draw_until(cx, window, &view, "carried=handoff from before"),
            "the seed never reached the child"
        );

        // The same preset with no seed: the placeholder goes away, so `$1` is
        // unset and the child draws no handoff.
        let spec = term_spec_for_agent(&agent, std::env::temp_dir(), TermSize::new(80, 24), None);
        let (window, view) = open(cx, spec, Some(agent));
        assert!(
            draw_until(cx, window, &view, "carried="),
            "the blank start never ran"
        );
        let drawn = cx.update(|cx| view.read(cx).screen().text());
        assert!(!drawn.contains("handoff from before"), "{drawn}");
    }

    /// `pi` typed at a shell prompt is still Pi, and its title's session name is its task.
    #[gpui_kit::test]
    fn a_shell_running_an_agent_is_named_after_it(cx: &mut TestAppContext) {
        let (window, view) = open(
            cx,
            spec(&format!(
                "{}; sleep 30",
                setting_title("\u{03c0} - review the diff - syro")
            )),
            None,
        );
        assert!(
            draw_until_status(cx, window, &view, |view, _| view.agent_name() == Some("Pi")),
            "the title never named the agent"
        );
        assert_eq!(
            cx.update(|cx| view.read(cx).detail().map(str::to_string))
                .as_deref(),
            Some("review the diff")
        );
        // And it is not a preset: nothing to restore it as. Its logo is still
        // Pi's, though — the readouts draw the agent the title named.
        assert_eq!(cx.update(|cx| view.read(cx).agent_id().cloned()), None);
        assert_eq!(
            cx.update(|cx| view.read(cx).running_agent_id().cloned()),
            Some("pi".to_string())
        );
    }

    #[gpui_kit::test]
    fn a_title_from_the_child_becomes_the_panes_status(cx: &mut TestAppContext) {
        let agent = title_agent(&format!(
            "{}; sleep 30",
            setting_title("Title Agent needs you")
        ));
        let spec = term_spec_for_agent(&agent, std::env::temp_dir(), TermSize::new(80, 24), None);
        let (window, view) = open(cx, spec, Some(agent));

        assert!(
            draw_until_status(cx, window, &view, |view, cx| view.status(cx)
                == Some(AgentStatus::Permission)),
            "the title never reached the pane's status",
        );
        // The same reading is the tab's label, which is what a tab bar shows.
        assert_eq!(
            cx.update(|cx| view.read(cx).title(cx)),
            "Title Agent needs you"
        );
    }

    /// Pi's title says `π - project` whatever it is doing; its `OSC 7501` report
    /// is what says it is working, and the row's length is that work's.
    #[gpui_kit::test]
    fn a_program_status_report_beats_the_title(cx: &mut TestAppContext) {
        let agent = title_agent(&format!(
            "{}; sleep 0.3; printf '\\033]7501;state=working:app=pi\\033\\\\'; sleep 0.3; {}; sleep 30",
            setting_title("all done"),
            setting_title("all done"),
        ));
        let spec = term_spec_for_agent(&agent, std::env::temp_dir(), TermSize::new(80, 24), None);
        let (window, view) = open(cx, spec, Some(agent));

        assert!(
            draw_until_status(cx, window, &view, |view, cx| view.status(cx)
                == Some(AgentStatus::Idle)),
            "the title never read as idle"
        );
        assert_eq!(
            cx.update(|cx| view.read(cx).span()),
            None,
            "an agent that has not worked has a length"
        );
        assert!(
            draw_until_status(cx, window, &view, |view, cx| view.status(cx)
                == Some(AgentStatus::Working)),
            "the report never reached the pane's status"
        );
        assert!(
            cx.update(|cx| view.read(cx).span())
                .is_some_and(|(_, end)| end.is_none())
        );
        // The idle title that follows is not read over the report.
        std::thread::sleep(Duration::from_millis(600));
        draw_until_status(cx, window, &view, |_, _| true);
        assert_eq!(
            cx.update(|cx| view.read(cx).status(cx)),
            Some(AgentStatus::Working)
        );
    }

    #[gpui_kit::test]
    fn an_agent_that_exits_gives_up_its_status(cx: &mut TestAppContext) {
        let agent = title_agent(&format!(
            "{}; sleep 0.4; exit 0",
            setting_title("Title Agent needs you")
        ));
        let spec = term_spec_for_agent(&agent, std::env::temp_dir(), TermSize::new(80, 24), None);
        let (window, view) = open(cx, spec, Some(agent));

        assert!(
            draw_until_status(cx, window, &view, |view, cx| view.status(cx)
                == Some(AgentStatus::Permission)),
            "the agent never read as waiting"
        );
        // A shell keeps whatever title the agent left behind, so the title still
        // says "needs you" after the agent is gone. The exit is what says the
        // agent is gone, and the dot has to go with it.
        assert!(
            draw_until_status(cx, window, &view, |view, cx| view.exit_status().is_some()
                && view.status(cx).is_none()),
            "the exited agent kept its status"
        );
    }

    #[gpui_kit::test]
    fn a_plain_shell_has_no_status_whatever_its_title(cx: &mut TestAppContext) {
        let (window, view) = open(cx, spec("printf '\\033]0;zsh\\007'; sleep 30"), None);
        // Wait for the title to have arrived, so "no status" is not just "no
        // title yet": a shell with no agent preset has nothing to read it
        // against, whatever it calls itself.
        assert!(
            draw_until_status(cx, window, &view, |view, cx| view.title(cx) == "zsh"),
            "the child never set a title"
        );
        assert_eq!(cx.update(|cx| view.read(cx).status(cx)), None);
    }

    #[gpui_kit::test]
    fn the_child_output_lands_on_the_grid(cx: &mut TestAppContext) {
        let (window, view) = open(cx, spec("printf 'hello from the pty\\r\\n'"), None);
        assert!(
            draw_until(cx, window, &view, "hello from the pty"),
            "the grid never showed the child's output: {:?}",
            cx.update(|cx| view.read(cx).screen())
        );
        let screen = cx.update(|cx| view.read(cx).screen());
        assert_eq!(screen.lines[0], "hello from the pty");
        // The canvas measured the window and resized the grid to it. Both axes:
        assert!(
            screen.columns > 40,
            "the grid never followed the window's width: {screen:?}"
        );
        assert!(
            screen.screen_lines > 10,
            "the grid never followed the window's height: {screen:?}"
        );
    }

    /// The window position of a cell, read from the grid the last frame painted.
    /// A test asks for cells; only the view knows where they ended up.
    fn cell_position(
        cx: &mut TestAppContext,
        view: &Entity<TerminalView>,
        line: usize,
        column: f32,
    ) -> Point<Pixels> {
        cx.update(|cx| {
            let view = view.read(cx);
            let layout = view.layout.get();
            let cell = view.metrics.cell;
            Point::new(
                layout.origin.x + cell.width * column,
                layout.origin.y + cell.height * (line as f32 + 0.5),
            )
        })
    }

    /// Drag across a row of the child's output and answer with what the drag
    /// left selected. `.25` into the first cell and `.75` into the last one are
    /// the halves either side of the cell boundary that decide the endpoints.
    fn drag_over_row(
        cx: &mut TestAppContext,
        window: WindowHandle<Root>,
        view: &Entity<TerminalView>,
        line: usize,
        from_column: f32,
        to_column: f32,
    ) {
        let from = cell_position(cx, view, line, from_column);
        let to = cell_position(cx, view, line, to_column);
        cx.update_window(window.into(), |_, window, cx| {
            window.drag(from, to, cx);
        })
        .expect("drag across the grid");
    }

    #[gpui_kit::test]
    fn dragging_a_row_selects_it(cx: &mut TestAppContext) {
        let (window, view) = open(cx, spec("printf 'select me now\\r\\n'"), None);
        assert!(
            draw_until(cx, window, &view, "select me now"),
            "the grid never showed the child's output: {:?}",
            cx.update(|cx| view.read(cx).screen())
        );

        drag_over_row(cx, window, &view, 0, 0.25, 12.75);
        assert_eq!(
            cx.update(|cx| view.read(cx).selection_text()).as_deref(),
            Some("select me now")
        );

        // And the drag is not a selection of the whole row: a narrower one is
        // exactly the cells it crossed, which is what tells the two apart.
        drag_over_row(cx, window, &view, 0, 0.25, 5.75);
        assert_eq!(
            cx.update(|cx| view.read(cx).selection_text()).as_deref(),
            Some("select")
        );
    }

    /// The grid starts a few pixels inside the pane: the child's first cell is
    /// not at the pane's corner, and a click in the corner selects nothing
    /// because it is padding rather than a cell.
    #[gpui_kit::test]
    fn the_grid_starts_inside_the_pane(cx: &mut TestAppContext) {
        let (window, view) = open(cx, spec("printf 'padded\r\n'"), None);
        assert!(
            draw_until(cx, window, &view, "padded"),
            "the grid never showed the child's output"
        );

        // The element fills the test window, so the pane's own origin is (0, 0)
        // and the grid's is the padding and nothing else.
        let origin = cx.update(|cx| view.read(cx).layout.get().origin);
        assert_eq!(
            origin,
            Point::new(px(PADDING_X), px(PADDING_Y)),
            "the grid does not start at the padding"
        );

        // The pane's very corner: padding, so nothing is selected.
        cx.update_window(window.into(), |_, window, cx| {
            window.drag(Point::default(), Point::default(), cx);
        })
        .expect("click the corner");
        assert_eq!(
            cx.update(|cx| view.read(cx).selection_text()),
            None,
            "a click in the padding selected a cell"
        );

        // A cell in from the edge is the first cell, and it does select.
        drag_over_row(cx, window, &view, 0, 0.25, 0.75);
        assert_eq!(
            cx.update(|cx| view.read(cx).selection_text()).as_deref(),
            Some("p")
        );
    }

    #[gpui_kit::test]
    fn cmd_c_puts_the_selection_on_the_clipboard(cx: &mut TestAppContext) {
        let (window, view) = open(cx, spec("printf 'copy me\\r\\n'"), None);
        assert!(draw_until(cx, window, &view, "copy me"), "no output");
        drag_over_row(cx, window, &view, 0, 0.25, 6.75);

        cx.update_window(window.into(), |_, window, cx| {
            window.press("cmd-c", cx);
        })
        .expect("press cmd-c");
        let copied = cx.update(|cx| cx.read_from_clipboard().and_then(|item| item.text()));
        assert_eq!(copied.as_deref(), Some("copy me"));
    }

    #[gpui_kit::test]
    fn pasting_reaches_the_child(cx: &mut TestAppContext) {
        let (window, view) = open(cx, spec("cat"), None);
        cx.update(|cx| cx.write_to_clipboard(ClipboardItem::new_string("pasted text".to_string())));
        cx.update_window(window.into(), |_, window, cx| {
            window.draw(cx).clear(cx);
            let focus = view.read(cx).focus_handle(cx);
            window.focus(&focus, cx);
            window.press("cmd-v", cx);
        })
        .expect("press cmd-v");
        // `cat` echoes, so the grid is the proof the clipboard bytes made the
        // whole trip: clipboard → key handler → session → pty → child → grid.
        assert!(
            draw_until(cx, window, &view, "pasted text"),
            "the child never echoed the paste: {:?}",
            cx.update(|cx| view.read(cx).screen())
        );
    }

    #[gpui_kit::test]
    fn an_unfocused_terminal_leaves_the_clipboard_alone(cx: &mut TestAppContext) {
        let (window, view) = open(cx, spec("printf 'copy me\\r\\n'"), None);
        assert!(draw_until(cx, window, &view, "copy me"), "no output");
        cx.update(|cx| cx.write_to_clipboard(ClipboardItem::new_string("untouched".to_string())));

        cx.update_window(window.into(), |_, window, cx| {
            // A selection to copy, and the keyboard somewhere else: a press on
            // another pane, a dialog opening — the terminal must sit this one
            // out rather than copy from under whoever is typing.
            let session = view.read(cx).session.clone();
            session.with_term(|term| {
                term.selection = Some(Selection::new(
                    SelectionType::Lines,
                    GridPoint::new(Line(0), Column(0)),
                    Side::Left,
                ));
            });
            window.blur(cx);
            window.press("cmd-c", cx);
        })
        .expect("press cmd-c while blurred");

        // The selection was there to be copied, and the clipboard is still what
        // it was: the key reached nobody. A line selection carries its newline.
        assert_eq!(
            cx.update(|cx| view.read(cx).selection_text()).as_deref(),
            Some("copy me\n")
        );
        let clipboard = cx.update(|cx| cx.read_from_clipboard().and_then(|item| item.text()));
        assert_eq!(clipboard.as_deref(), Some("untouched"));
    }

    #[gpui_kit::test]
    fn typing_makes_the_whole_trip(cx: &mut TestAppContext) {
        let (window, view) = open(cx, spec("cat"), None);
        cx.update_window(window.into(), |_, window, cx| {
            window.draw(cx).clear(cx);
            let focus = view.read(cx).focus_handle(cx);
            window.focus(&focus, cx);
            window.input("hi there", cx);
            window.press("enter", cx);
        })
        .expect("type at the terminal");
        // `cat` echoes, so the grid is the proof the bytes made the whole trip:
        // key → session → PTY → child → PTY → grid.
        assert!(
            draw_until(cx, window, &view, "hi there"),
            "the child never echoed what was typed: {:?}",
            cx.update(|cx| view.read(cx).screen())
        );
    }

    /// Start a composition, the way the input method does mid-word.
    fn mark(cx: &mut TestAppContext, view: &Entity<TerminalView>, text: &str) {
        cx.update(|cx| view.update(cx, |view, cx| view.set_marked_text(text, cx)));
    }

    /// Commit it, the way the input method does when the user picks a candidate.
    fn commit(cx: &mut TestAppContext, view: &Entity<TerminalView>, text: &str) {
        cx.update(|cx| view.update(cx, |view, cx| view.commit_text(text, cx)));
    }

    #[gpui_kit::test]
    fn a_committed_composition_reaches_the_child(cx: &mut TestAppContext) {
        let (window, view) = open(cx, spec("cat"), None);
        cx.update_window(window.into(), |_, window, cx| {
            window.draw(cx).clear(cx);
            let focus = view.read(cx).focus_handle(cx);
            window.focus(&focus, cx);
        })
        .expect("focus the terminal");

        // What a Japanese IME does: pre-edit text while the user picks, then the
        // committed text. A headless window cannot drive a real input method, so
        // these are the two calls the platform would make, in order.
        mark(cx, &view, "にほんご");
        assert!(cx.update(|cx| view.read(cx).marked_text_range()).is_some());
        commit(cx, &view, "日本語");
        assert!(
            cx.update(|cx| view.read(cx).marked_text_range()).is_none(),
            "the composition outlived its commit"
        );

        // `cat` echoes, so the grid is the proof the bytes made the whole trip:
        // commit → session → PTY → child → PTY → grid. The pre-edit text never
        // went anywhere, which is why only the commit is on the grid.
        assert!(
            draw_until(cx, window, &view, "日本語"),
            "the child never echoed the committed composition: {:?}",
            cx.update(|cx| view.read(cx).screen())
        );
        assert!(!cx.update(|cx| view.read(cx).screen().text().contains("にほんご")));
    }

    #[gpui_kit::test]
    fn a_cancelled_composition_hands_over_nothing(cx: &mut TestAppContext) {
        let (window, view) = open(cx, spec("cat"), None);
        cx.update_window(window.into(), |_, window, cx| {
            window.draw(cx).clear(cx);
            let focus = view.read(cx).focus_handle(cx);
            window.focus(&focus, cx);
        })
        .expect("focus the terminal");

        mark(cx, &view, "にほんご");
        cx.update(|cx| view.update(cx, |view, cx| view.clear_marked_text(cx)));
        assert!(cx.update(|cx| view.read(cx).marked_text_range()).is_none());

        // Nothing was committed, so nothing may reach the child. Draw a while:
        // if a byte had been written, `cat` would have echoed it back by now.
        let echoed = draw_until(cx, window, &view, "にほんご");
        assert!(
            !echoed,
            "a cancelled composition typed into the child: {:?}",
            cx.update(|cx| view.read(cx).screen())
        );
        assert_eq!(cx.update(|cx| view.read(cx).screen().text()), "");
    }

    /// A click on a child that asked for the mouse reaches it. A TUI draws its
    /// own buttons in the grid ("Jump to latest message" and the like), so a
    /// press that is acted on locally is a button that cannot be clicked.
    #[gpui_kit::test]
    fn a_click_reaches_a_child_that_asked_for_the_mouse(cx: &mut TestAppContext) {
        let (window, view) = open(
            cx,
            spec("stty -icanon -echo; printf '\\033[?1002h\\033[?1006hREADY'; cat -v"),
            None,
        );
        assert!(
            draw_until(cx, window, &view, "READY"),
            "the child never got going"
        );

        // One cell, pressed and released, the way a click arrives.
        let at = cell_position(cx, &view, 2, 3.);
        cx.update_window(window.into(), |_, window, cx| {
            window.dispatch_event(
                gpui_kit::MouseDownEvent {
                    button: gpui_kit::MouseButton::Left,
                    position: at,
                    modifiers: Default::default(),
                    click_count: 1,
                    first_mouse: false,
                }
                .to_platform_input(),
                cx,
            );
            window.render_frame(cx);
            window.dispatch_event(
                gpui_kit::MouseUpEvent {
                    button: gpui_kit::MouseButton::Left,
                    position: at,
                    modifiers: Default::default(),
                    click_count: 1,
                }
                .to_platform_input(),
                cx,
            );
            window.render_frame(cx);
        })
        .expect("click the terminal");

        // `cat -v` escapes what it receives, so the grid is the proof: the SGR
        // press names the button and the cell, and the release is the same with
        // a lower-case `m`.
        assert!(
            draw_until(cx, window, &view, "^[[<0;4;3M"),
            "the press never reached the child: {:?}",
            cx.update(|cx| view.read(cx).screen()).text()
        );
        assert!(
            draw_until(cx, window, &view, "^[[<0;4;3m"),
            "the release never reached the child: {:?}",
            cx.update(|cx| view.read(cx).screen()).text()
        );
        // And nothing was selected: the child owns the mouse while it asks for it.
        assert!(cx.update(|cx| view.read(cx).selection_text()).is_none());
    }

    /// A drag inside a child that asked for drag reports reaches it too: that is
    /// how a TUI's own scrollbar or text selection is dragged.
    #[gpui_kit::test]
    fn a_drag_reaches_a_child_that_asked_for_the_mouse(cx: &mut TestAppContext) {
        let (window, view) = open(
            cx,
            spec("stty -icanon -echo; printf '\\033[?1002h\\033[?1006hREADY'; cat -v"),
            None,
        );
        assert!(
            draw_until(cx, window, &view, "READY"),
            "the child never got going"
        );

        // Press on one cell and pull to another: the move with the button held is
        // what `?1002` is for, and the first cell is where the report starts.
        let from = cell_position(cx, &view, 2, 3.);
        let to = cell_position(cx, &view, 2, 9.);
        cx.update_window(window.into(), |_, window, cx| {
            window.drag(from, to, cx);
        })
        .expect("drag in the terminal");

        // Bit 5 of the code says "moved": `^[[<32;4;3M` for the press's cell.
        assert!(
            draw_until(cx, window, &view, "^[[<32;"),
            "the drag never reached the child: {:?}",
            cx.update(|cx| view.read(cx).screen()).text()
        );
        // And the pointer really moved: a report per cell it passed over, not one
        // standing still. (The grid wraps the reports across its lines, so the
        // count is the assertion and not the coordinates.)
        let screen = cx.update(|cx| view.read(cx).screen()).text();
        assert!(
            screen.matches("^[[<32;").count() > 1,
            "the drag only reported once: {screen:?}"
        );
    }

    /// Option takes the mouse back, which is how text is selected inside a TUI
    /// whose buttons are in the grid.
    #[gpui_kit::test]
    fn holding_option_keeps_the_click_from_the_child(cx: &mut TestAppContext) {
        let (window, view) = open(
            cx,
            spec("stty -icanon -echo; printf '\\033[?1002h\\033[?1006hREADY'; cat -v"),
            None,
        );
        assert!(
            draw_until(cx, window, &view, "READY"),
            "the child never got going"
        );

        let at = cell_position(cx, &view, 2, 3.);
        cx.update_window(window.into(), |_, window, cx| {
            window.dispatch_event(
                gpui_kit::MouseDownEvent {
                    button: gpui_kit::MouseButton::Left,
                    position: at,
                    modifiers: gpui_kit::Modifiers {
                        alt: true,
                        ..Default::default()
                    },
                    click_count: 1,
                    first_mouse: false,
                }
                .to_platform_input(),
                cx,
            );
            window.render_frame(cx);
        })
        .expect("option-click the terminal");

        // Nothing reached the child, and the bytes would be back by now — a PTY
        // round trip is microseconds and `cat -v` has already answered once.
        for _ in 0..5 {
            cx.update_window(window.into(), |_, window, cx| {
                window.draw(cx).clear(cx);
            })
            .expect("draw a frame");
        }
        let screen = cx.update(|cx| view.read(cx).screen()).text();
        assert!(
            !screen.contains("^["),
            "option-click still went to the child: {screen:?}"
        );
    }

    /// A wheel notch, as the platform reports it with a mouse: whole lines, and
    /// the grid follows them one for one.
    #[gpui_kit::test]
    fn a_wheel_notch_scrolls_whole_lines(cx: &mut TestAppContext) {
        let (window, view) = open(cx, spec(LONG_OUTPUT), None);
        assert!(
            draw_until(cx, window, &view, "line40"),
            "no history to scroll into"
        );
        assert_eq!(cx.update(|cx| view.read(cx).screen().display_offset), 0);

        let at = cell_position(cx, &view, 4, 4.);
        wheel(
            cx,
            window,
            at,
            gpui_kit::ScrollDelta::Lines(gpui_kit::point(0., 3.)),
        );

        assert_eq!(
            cx.update(|cx| view.read(cx).screen().display_offset),
            3,
            "a wheel notch did not move the viewport"
        );
    }

    /// A trackpad reports pixels, and one event is usually a fraction of a line.
    #[gpui_kit::test]
    fn a_small_pixel_scroll_adds_up_to_a_line(cx: &mut TestAppContext) {
        let (window, view) = open(cx, spec(LONG_OUTPUT), None);
        assert!(
            draw_until(cx, window, &view, "line40"),
            "no history to scroll into"
        );
        assert_eq!(cx.update(|cx| view.read(cx).screen().display_offset), 0);

        let cell = cx.update(|cx| view.read(cx).metrics.cell.height.as_f32());
        let at = cell_position(cx, &view, 4, 4.);
        let third = px(cell / 3.);
        for _ in 0..4 {
            wheel(
                cx,
                window,
                at,
                gpui_kit::ScrollDelta::Pixels(gpui_kit::point(px(0.), third)),
            );
        }

        let offset = cx.update(|cx| view.read(cx).screen().display_offset);
        assert!(
            offset >= 1,
            "a slow trackpad drag scrolled nothing: {offset}"
        );
    }

    /// The wheel goes to the child that asked for the mouse — an agent's TUI,
    /// which lives on the alternate screen where there is no history of ours to
    /// move. `cat -v` escapes what it receives, so the grid is the proof.
    #[gpui_kit::test]
    fn the_wheel_reaches_a_child_that_asked_for_the_mouse(cx: &mut TestAppContext) {
        let (window, view) = open(
            cx,
            spec("stty -icanon -echo; printf '\\033[?1049h\\033[?1002h\\033[?1006hREADY'; cat -v"),
            None,
        );
        // The child's own marker: the grid showing it is what proves the mode
        // sequences before it were parsed, so the wheel below meets a child that
        // has already asked for the mouse.
        assert!(
            draw_until(cx, window, &view, "READY"),
            "the child never got going"
        );

        let at = cell_position(cx, &view, 2, 3.);
        wheel(
            cx,
            window,
            at,
            gpui_kit::ScrollDelta::Lines(gpui_kit::point(0., 1.)),
        );

        assert!(
            draw_until(cx, window, &view, "^[[<64;"),
            "the wheel never reached the child: {:?}",
            cx.update(|cx| view.read(cx).screen()).text()
        );
        // And the viewport did not move: the child owns the scroll.
        assert_eq!(cx.update(|cx| view.read(cx).screen().display_offset), 0);
    }

    /// A child on the alternate screen that did *not* ask for the mouse gets the
    /// arrow keys xterm sends instead — a pager, an editor, or an agent before
    /// it turns the mouse on.
    #[gpui_kit::test]
    fn the_wheel_sends_arrows_to_a_child_on_the_alternate_screen(cx: &mut TestAppContext) {
        let (window, view) = open(
            cx,
            spec("stty -icanon -echo; printf '\\033[?1049hREADY'; cat -v"),
            None,
        );
        // The marker first: the grid showing it is what proves the switch to the
        // alternate screen before it was parsed.
        assert!(
            draw_until(cx, window, &view, "READY"),
            "the child never got going"
        );
        let at = cell_position(cx, &view, 2, 3.);

        wheel(
            cx,
            window,
            at,
            gpui_kit::ScrollDelta::Lines(gpui_kit::point(0., 1.)),
        );
        assert!(
            draw_until(cx, window, &view, "^[[A"),
            "no upwards arrow reached the child: {:?}",
            cx.update(|cx| view.read(cx).screen()).text()
        );

        wheel(
            cx,
            window,
            at,
            gpui_kit::ScrollDelta::Lines(gpui_kit::point(0., -1.)),
        );
        assert!(
            draw_until(cx, window, &view, "^[[B"),
            "no downwards arrow reached the child: {:?}",
            cx.update(|cx| view.read(cx).screen()).text()
        );
    }

    /// The wheel goes to the terminal under the pointer, at the position the
    /// platform reports it — which is what the hit test above the grid is for.
    fn wheel(
        cx: &mut TestAppContext,
        window: WindowHandle<Root>,
        at: Point<Pixels>,
        delta: gpui_kit::ScrollDelta,
    ) {
        cx.update_window(window.into(), |_, window, cx| {
            window.dispatch_event(
                gpui_kit::ScrollWheelEvent {
                    position: at,
                    delta,
                    ..Default::default()
                }
                .to_platform_input(),
                cx,
            );
            window.render_frame(cx);
        })
        .expect("scroll the terminal");
    }

    /// Forty lines into a grid that holds half of them: a history to scroll into,
    /// whatever the window is.
    const LONG_OUTPUT: &str =
        "i=1; while [ $i -le 40 ]; do echo line$i; i=$((i+1)); done; sleep 30";

    #[gpui_kit::test]
    fn dragging_past_the_top_edge_scrolls_the_selection_into_the_history(cx: &mut TestAppContext) {
        // More lines than the grid holds, so there is a history to scroll into.
        let script = "i=1; while [ $i -le 40 ]; do echo line$i; i=$((i+1)); done; sleep 30";
        let (window, view) = open(cx, spec(script), None);
        assert!(
            draw_until(cx, window, &view, "line40"),
            "the child never finished printing: {:?}",
            cx.update(|cx| view.read(cx).screen())
        );
        let before = cx.update(|cx| view.read(cx).screen());
        assert_eq!(before.display_offset, 0);

        // Press near the top of the grid and pull the pointer above the top
        // edge: the selection has to follow the history up, or it can only ever
        // reach what happened to be on screen.
        let from = cell_position(cx, &view, 8, 4.25);
        let to = cx.update(|cx| {
            let layout = view.read(cx).layout.get();
            let cell = view.read(cx).metrics.cell;
            Point::new(
                layout.origin.x + cell.width * 4.,
                layout.origin.y - cell.height * 3.5,
            )
        });
        cx.update_window(window.into(), |_, window, cx| {
            window.drag(from, to, cx);
        })
        .expect("drag past the top edge");
        cx.update_window(window.into(), |_, window, cx| {
            window.draw(cx).clear(cx);
        })
        .expect("draw the scrolled frame");

        let screen = cx.update(|cx| view.read(cx).screen());
        assert!(
            screen.display_offset > 0,
            "the drag never scrolled the viewport: {screen:?}"
        );
        let top_now = screen.lines[0].clone();
        assert!(
            !before.lines.contains(&top_now),
            "the viewport did not move"
        );

        // And the selection reaches what scrolled in: it begins on the row the drag pulled to the top
        // of the grid, which is a row that was above the screen when the press landed.
        let selected = cx
            .update(|cx| view.read(cx).selection_text())
            .expect("a selection");
        let first_selected = selected.lines().next().unwrap_or_default();
        assert!(
            top_now.ends_with(first_selected) && !first_selected.is_empty(),
            "the selection stopped at the old edge: {selected:?}"
        );
        assert!(
            !before.lines.iter().any(|line| line == first_selected),
            "the selection never left the first screenful: {selected:?}"
        );
    }
    /// A view holding a tree and a terminal side by side, the way the workspace
    /// arranges them. A test-only view: the real one is `Workspace`.
    struct Pair {
        tree: Entity<FileTreeView>,
        terminal: Entity<TerminalView>,
    }

    impl Render for Pair {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .h_flex()
                // The tree is a panel of the right sidebar and says `w_full`:
                // in a row it needs the sidebar's width around it, the way the
                // real workspace gives it one.
                .child(
                    div()
                        .w(px(260.))
                        .h_full()
                        .flex_shrink_0()
                        .child(self.tree.clone()),
                )
                // `h_full` for the same reason the workspace's pane column has
                // it: in a row, `flex_1` is the width, and without a height the
                // pane collapses to nothing and paints nothing.
                .child(
                    div()
                        .flex_1()
                        .h_full()
                        .min_h_0()
                        .child(self.terminal.clone()),
                )
        }
    }

    #[gpui_kit::test]
    fn a_path_dragged_from_the_tree_reaches_the_child(cx: &mut TestAppContext) {
        // The whole trip, with a real row at one end and a real pty at the other: tree row → drag
        // payload → terminal → shell-escaped bytes → child → grid.
        let (dir, repo, _worktrees) = scratch_repo("terminal-drop");
        let root = dir.join("main");
        let file = root.join("my notes.md");
        std::fs::write(&file, "notes\n").unwrap();
        let (state_path, _store_dir) = temp_store("terminal-drop");
        Store::new(state_path.clone())
            .save(&State {
                repos: vec![repo],
                active_worktree: Some(
                    root.canonicalize()
                        .unwrap_or_else(|_| root.clone())
                        .display()
                        .to_string(),
                ),
                ..Default::default()
            })
            .unwrap();
        let state = cx.update(|cx| {
            cx.new(|cx| {
                AppState::open(Store::new(state_path.clone()), dir.join("workspaces"), cx)
                    .with_settings(Settings::default())
            })
        });
        let shell_root = root.canonicalize().unwrap_or(root);
        let cat = TermSpec {
            command: "/bin/sh".to_string(),
            args: vec!["-c".to_string(), "cat".to_string()],
            ..TermSpec::shell(shell_root.clone(), TermSize::new(80, 24))
        };

        let built: Rc<RefCell<Option<(Entity<FileTreeView>, Entity<TerminalView>)>>> =
            Rc::new(RefCell::new(None));
        let slot = built.clone();
        let (window, _pair) = cx.update(|cx| {
            let bounds = Bounds {
                origin: Point::default(),
                size: size(px(800.), px(400.)),
            };
            let (window, content) = gpui_kit::open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    ..Default::default()
                },
                cx,
                move |_, cx| {
                    let tree = cx.new(|cx| FileTreeView::new(state, cx));
                    let terminal = cx.new(|cx| {
                        TerminalView::new(cx, cat, None, None, None).expect("spawn the terminal")
                    });
                    slot.borrow_mut().replace((tree.clone(), terminal.clone()));
                    cx.new(|_| Pair { tree, terminal })
                },
            )
            .expect("open test window");
            (window.downcast::<Root>().expect("Base Root"), content)
        });
        let (tree, view) = built.borrow().clone().expect("both panes were built");
        let _ = tree;

        cx.update_window(window.into(), |_, window, cx| {
            window.draw(cx).clear(cx);
        })
        .expect("draw");

        let row = format!("tree-{}", shell_root.join("my notes.md").display());
        let terminal_id = cx.update(|cx| view.read(cx).terminal_id().clone());
        cx.update_window(window.into(), |_, window, cx| {
            window.drag_to(row, terminal_id, cx);
        })
        .expect("drag the row onto the terminal");

        let expected = shell_escape(&shell_root.join("my notes.md"));
        // The grid wraps at its own columns, so the assertion is about what the
        // child received rather than about where the terminal drew the break.
        assert!(
            draw_until(cx, window, &view, "my notes.md'"),
            "the child never received the path: {:?}",
            cx.update(|cx| view.read(cx).screen())
        );
        let seen = cx.update(|cx| view.read(cx).screen().text());
        let echoed: String = seen
            .chars()
            .filter(|character| *character != '\n')
            .collect();
        assert_eq!(
            echoed.trim(),
            expected,
            "the child was handed something other than the escaped path"
        );
        // Quoted, not bare: the space is what tells them apart.
        assert!(expected.starts_with('\''), "the fixture is not a hard case");
    }
}
