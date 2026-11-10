//! The terminal session: PTY child + alacritty `Term` + an event channel.

use std::io::{Read, Write};
use std::process::ExitStatus;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use alacritty_terminal::event::{EventListener, WindowSize};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config as TermConfig, Term, TermMode};
use alacritty_terminal::vte::ansi::{CursorShape, Processor, Rgb};
use anyhow::{Context, Result};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize};

use crate::model::AgentStatus;

/// Grid size; implements alacritty's `Dimensions`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TermSize {
    pub columns: usize,
    pub screen_lines: usize,
}

impl TermSize {
    pub fn new(columns: usize, screen_lines: usize) -> Self {
        Self {
            columns,
            screen_lines,
        }
    }

    pub fn pty_size(&self) -> PtySize {
        PtySize {
            rows: self.screen_lines as u16,
            cols: self.columns as u16,
            pixel_width: 0,
            pixel_height: 0,
        }
    }
}

impl Dimensions for TermSize {
    fn total_lines(&self) -> usize {
        self.screen_lines
    }

    fn screen_lines(&self) -> usize {
        self.screen_lines
    }

    fn columns(&self) -> usize {
        self.columns
    }
}

/// What to start. `command` is a program name looked up in `PATH`.
#[derive(Debug, Clone)]
pub struct TermSpec {
    pub command: String,
    pub args: Vec<String>,
    pub cwd: std::path::PathBuf,
    /// Extra environment on top of the inherited one. `TERM` is always set.
    pub env: Vec<(String, String)>,
    pub size: TermSize,
    /// Lines of scrollback kept above the viewport.
    pub scrollback: usize,
}

impl TermSpec {
    /// A login shell in `cwd`: `$SHELL` (or `/bin/zsh`) on Unix, `%COMSPEC%` on Windows.
    pub fn shell(cwd: impl Into<std::path::PathBuf>, size: TermSize) -> Self {
        #[cfg(windows)]
        let command = std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_string());
        #[cfg(not(windows))]
        let command = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string());
        Self {
            command,
            args: Vec::new(),
            cwd: cwd.into(),
            env: Vec::new(),
            size,
            // The same as `config::default_scrollback`, and for the same memory reason.
            scrollback: 2000,
        }
    }

    /// An agent (or anything else) in `cwd`.
    pub fn command(cwd: impl Into<std::path::PathBuf>, size: TermSize, command: &str) -> Self {
        Self {
            command: command.to_string(),
            ..Self::shell(cwd, size)
        }
    }
}

/// Something the terminal wants the front end to know about: the variants it cannot answer itself.
#[derive(Debug, Clone, PartialEq)]
pub enum TermEvent {
    /// New bytes reached the grid: repaint. Coalesced by the reader thread.
    Wakeup,
    /// `OSC 0` / `OSC 2`: the child set the window title — what the agent status rules read.
    Title(String),
    /// `OSC 0` / `OSC 2` with no title: back to the default.
    ResetTitle,
    /// `OSC 7501` (Program Status Protocol): the child said what it is doing — Pi does this instead
    /// of putting it in the title. `None` is `state=clear`: it stopped reporting, the title rules again.
    ProgramStatus(Option<AgentStatus>),
    /// `BEL`.
    Bell,
    /// `OSC 52` copy: the child asked to put `text` on the clipboard.
    ClipboardStore(String),
    /// The child exited. Emitted once, reaped by `drain_events`.
    Exit(ExitStatus),
    /// The PTY itself failed (spawn, read, write). The session is dead after.
    Error(String),
}

/// The terminal's view of the front end, seen only inside [`TermSession::with_term`].
pub struct EventProxy {
    events: Sender<TermEvent>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    size: Arc<Mutex<TermSize>>,
}

impl EventListener for EventProxy {
    fn send_event(&self, event: alacritty_terminal::event::Event) {
        use alacritty_terminal::event::Event;
        let translated = match event {
            Event::Wakeup => TermEvent::Wakeup,
            Event::Title(title) => TermEvent::Title(title),
            Event::ResetTitle => TermEvent::ResetTitle,
            Event::Bell => TermEvent::Bell,
            Event::ClipboardStore(_, text) => TermEvent::ClipboardStore(text),
            Event::Exit => return,
            // Answered here, so the app has nothing to do.
            Event::PtyWrite(text) => {
                let _ = self.write_to_child(text.as_bytes());
                return;
            }
            Event::TextAreaSizeRequest(format) => {
                let size = *self.size.lock().unwrap();
                let report = format(WindowSize {
                    num_lines: size.screen_lines as u16,
                    num_cols: size.columns as u16,
                    cell_width: 0,
                    cell_height: 0,
                });
                let _ = self.write_to_child(report.as_bytes());
                return;
            }
            // ponytail: an OSC 52 read gets an empty answer rather than a hang.
            Event::ClipboardLoad(..) => {
                let _ = self.write_to_child(b"\x1b]52;c;\x07");
                return;
            }
            Event::ColorRequest(_index, format) => {
                let report = format(Rgb { r: 0, g: 0, b: 0 });
                let _ = self.write_to_child(report.as_bytes());
                return;
            }
            // Nothing for a front end that has no cursor to change or blink.
            Event::MouseCursorDirty | Event::CursorBlinkingChange | Event::ChildExit(_) => return,
        };
        let _ = self.events.send(translated);
    }
}

impl EventProxy {
    fn write_to_child(&self, bytes: &[u8]) -> Result<()> {
        let mut writer = self.writer.lock().unwrap();
        writer.write_all(bytes)?;
        writer.flush()?;
        Ok(())
    }
}

/// A child's exit status, in the standard library's type — what the rest of Rust hands around,
/// and what [`TermEvent::Exit`] carries.
#[cfg(unix)]
fn std_status(status: portable_pty::ExitStatus) -> ExitStatus {
    use std::os::unix::process::ExitStatusExt;
    ExitStatus::from_raw((status.exit_code() << 8) as i32)
}

#[cfg(windows)]
fn std_status(status: portable_pty::ExitStatus) -> ExitStatus {
    use std::os::windows::process::ExitStatusExt;
    ExitStatus::from_raw(status.exit_code())
}

/// A second reader of a terminal's bytes, besides its own grid — how the web
/// panel watches a terminal. Made by [`TermSession::subscribe`]; dropping it ends
/// the subscription at the next read.
#[derive(Debug)]
pub struct Subscription {
    frames: Receiver<Vec<u8>>,
    lagged: Arc<AtomicBool>,
}

impl Subscription {
    /// The next chunk the child wrote, waiting up to `timeout`. The three
    /// outcomes a reader needs: a chunk, nothing yet, or the terminal is gone.
    pub fn recv_timeout(
        &self,
        timeout: Duration,
    ) -> Result<Vec<u8>, std::sync::mpsc::RecvTimeoutError> {
        self.frames.recv_timeout(timeout)
    }

    /// The next chunk, if one is already queued. `None` when there is not, or
    /// when the terminal is gone — [`Self::recv_timeout`] is what tells those
    /// apart.
    pub fn try_chunk(&self) -> Option<Vec<u8>> {
        self.frames.try_recv().ok()
    }

    /// Drop whatever is already queued, without reading it. For a reader that is
    /// about to be handed the screen itself, which already includes these frames.
    pub fn drain(&self) {
        while self.frames.try_recv().is_ok() {}
    }

    /// Whether a frame was dropped since the last call, clearing it. `true` means
    /// this reader fell behind and its view of the screen is wrong: it has to be
    /// repainted from the grid, because the bytes that would have updated it are
    /// gone.
    pub fn lagged(&self) -> bool {
        self.lagged.swap(false, Ordering::Relaxed)
    }
}

/// A live terminal: PTY child + grid + event channel.
pub struct TermSession {
    child: Mutex<Box<dyn Child + Send>>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    term: Arc<FairMutex<Term<EventProxy>>>,
    size: Arc<Mutex<TermSize>>,
    events: Receiver<TermEvent>,
    eof: Arc<AtomicBool>,
    exit_reported: AtomicBool,
    /// Everyone else reading the child's byte stream, besides the grid. Empty
    /// unless the web panel is watching a terminal; a subscriber that stops
    /// reading is dropped on the next send. See [`Self::subscribe`].
    subscribers: Arc<Mutex<Vec<Subscriber>>>,
}

/// How many reads one subscriber may fall behind by before its frames are
/// dropped. A frame is one read (up to 64 KiB), so this bounds one subscriber at
/// a few megabytes — a phone that stopped reading must not grow the app.
const SUBSCRIBER_QUEUE: usize = 64;

/// One reader of the child's bytes besides the grid, and whether it is still in
/// step with it. A subscriber whose queue filled has missed a frame, and its view
/// of the screen is wrong from then on until it is given the grid again —
/// [`Subscription::lagged`] is how it finds out.
struct Subscriber {
    frames: SyncSender<Vec<u8>>,
    lagged: Arc<AtomicBool>,
}

/// Hand one read to every subscriber, dropping the ones that are gone and, for a
/// subscriber that is behind, dropping the frame rather than waiting: the reader
/// thread is the child's only drain, and a browser must never stall it.
fn forward(subscribers: &Mutex<Vec<Subscriber>>, bytes: &[u8]) {
    let mut readers = subscribers.lock().unwrap();
    readers.retain_mut(
        |subscriber| match subscriber.frames.try_send(bytes.to_vec()) {
            Ok(()) => true,
            // Quoted for the reader's benefit: the frame is gone, so its screen is
            // wrong until it is repainted from the grid.
            Err(std::sync::mpsc::TrySendError::Full(_)) => {
                subscriber.lagged.store(true, Ordering::Relaxed);
                true
            }
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => false,
        },
    );
}

/// The locale a child gets when the app has none. A `.app` launched from Finder
/// inherits no `LANG`, and a shell in the `C` locale shreds every multi-byte
/// character into single bytes — `宁` is echoed back as `<0081>` at the prompt.
/// `None` when the environment already names a locale, which is respected.
fn fallback_locale(
    lookup: impl Fn(&str) -> Option<std::ffi::OsString>,
) -> Option<(&'static str, &'static str)> {
    ["LC_ALL", "LC_CTYPE", "LANG"]
        .iter()
        .all(|name| lookup(name).is_none_or(|value| value.is_empty()))
        .then_some(("LANG", "en_US.UTF-8"))
}

impl TermSession {
    /// Start `spec` and begin draining its output. A command that does not exist fails here, not on
    /// the first repaint.
    pub fn spawn(spec: TermSpec) -> Result<Self> {
        let pty = portable_pty::native_pty_system()
            .openpty(spec.size.pty_size())
            .context("open the pty")?;

        let mut command = CommandBuilder::new(&spec.command);
        command.args(&spec.args);
        // cmd.exe reads a `\\?\` path as UNC and starts in C:\Windows instead.
        command.cwd(dunce::simplified(&spec.cwd));
        command.env("TERM", "xterm-256color");
        if let Some((name, value)) = fallback_locale(|name| std::env::var_os(name)) {
            command.env(name, value);
        }
        for (key, value) in &spec.env {
            command.env(key, value);
        }
        let child = pty
            .slave
            .spawn_command(command)
            .with_context(|| format!("spawn {:?}", spec.command))?;
        // The parent's copy of the slave would keep the pty open and hide EOF from the master; from
        // here only the child holds it.
        drop(pty.slave);

        let mut reader = pty
            .master
            .try_clone_reader()
            .context("clone the pty reader")?;
        let writer = pty.master.take_writer().context("take the pty writer")?;

        let (events, incoming) = std::sync::mpsc::channel();
        let writer = Arc::new(Mutex::new(writer));
        let size = Arc::new(Mutex::new(spec.size));
        let eof = Arc::new(AtomicBool::new(false));
        let term = Arc::new(FairMutex::new(Term::new(
            TermConfig {
                scrolling_history: spec.scrollback,
                ..TermConfig::default()
            },
            &spec.size,
            EventProxy {
                events: events.clone(),
                writer: Arc::clone(&writer),
                size: Arc::clone(&size),
            },
        )));

        let reader_term = Arc::clone(&term);
        let reader_writer = Arc::clone(&writer);
        let reader_eof = Arc::clone(&eof);
        let subscribers = Arc::new(Mutex::new(Vec::new()));
        let reader_subscribers = Arc::clone(&subscribers);
        std::thread::Builder::new()
            .name("syro-term-reader".to_string())
            .spawn(move || {
                // The default timeout handler, which exists for the synchronized-update sequence.
                let mut processor: Processor = Processor::new();
                let mut program_status = ProgramStatusScanner::default();
                let mut buffer = [0u8; 64 * 1024];
                loop {
                    match reader.read(&mut buffer) {
                        // EOF: the child has hung up, so an `Err` below is a real failure.
                        Ok(0) => break,
                        Ok(bytes) => {
                            // alacritty ignores OSC 7501, so it is read here — and answered before
                            // `advance` answers the `DA` query that follows it: a reply after `DA`
                            // reads to the child as "not supported".
                            for body in program_status.feed(&buffer[..bytes]) {
                                if body.starts_with('?') {
                                    let mut writer = reader_writer.lock().unwrap();
                                    let _ = writer.write_all(b"\x1b]7501;?\x1b\\");
                                    let _ = writer.flush();
                                } else if let Some(status) = program_status_of(&body) {
                                    let _ = events.send(TermEvent::ProgramStatus(status));
                                }
                            }
                            let mut terminal = reader_term.lock();
                            processor.advance(&mut *terminal, &buffer[..bytes]);
                            // Released before the send: the UI paints under this lock, so it must not queue behind a channel fill.
                            drop(terminal);
                            forward(&reader_subscribers, &buffer[..bytes]);
                            // One event per read, not per byte.
                            if events.send(TermEvent::Wakeup).is_err() {
                                break;
                            }
                        }
                        Err(error) => {
                            let _ = events.send(TermEvent::Error(error.to_string()));
                            break;
                        }
                    }
                }
                // The exit is collected, not sent: `drain_events` reaps the child, and this flags that there is one.
                reader_eof.store(true, Ordering::Release);
                let _ = events.send(TermEvent::Wakeup);
            })
            .context("start the terminal reader thread")?;

        Ok(Self {
            child: Mutex::new(child),
            master: Mutex::new(pty.master),
            writer,
            term,
            size,
            events: incoming,
            eof,
            exit_reported: AtomicBool::new(false),
            subscribers,
        })
    }

    /// Every byte the child writes from now on, as it was read. The grid has
    /// already had them; this is for a second reader, which is the web panel.
    pub fn subscribe(&self) -> Subscription {
        let (frames, bytes) = std::sync::mpsc::sync_channel(SUBSCRIBER_QUEUE);
        let lagged = Arc::new(AtomicBool::new(false));
        self.subscribers.lock().unwrap().push(Subscriber {
            frames,
            lagged: Arc::clone(&lagged),
        });
        Subscription {
            frames: bytes,
            lagged,
        }
    }

    /// Bytes from the keyboard, straight to the child; the caller encodes the key.
    pub fn write(&self, bytes: &[u8]) -> Result<()> {
        let mut writer = self.writer.lock().unwrap();
        writer.write_all(bytes)?;
        writer.flush()?;
        Ok(())
    }

    /// Resize both the PTY and the grid, and return once the child has been told.
    pub fn resize(&self, columns: usize, screen_lines: usize) -> Result<()> {
        let size = TermSize::new(columns, screen_lines);
        // The pty first: a TUI reading `stty size` in its SIGWINCH handler must not be told a size its
        // grid is not yet.
        self.master
            .lock()
            .unwrap()
            .resize(size.pty_size())
            .context("resize the pty")?;
        self.term.lock().resize(size);
        *self.size.lock().unwrap() = size;
        Ok(())
    }

    /// Scroll the viewport: positive is up into the scrollback. Does not move the child's cursor.
    pub fn scroll(&self, lines: i32) {
        self.term.lock().scroll_display(Scroll::Delta(lines));
    }

    /// Whether the child asked for mouse reports (`?1000` / `?1002` / `?1003`). A TUI that asked
    /// wants the mouse itself, and has no history of ours to scroll.
    pub fn wants_mouse(&self) -> bool {
        self.term.lock().mode().intersects(TermMode::MOUSE_MODE)
    }

    /// Whether a held-button drag (`?1002`) or all motion (`?1003`) is reported.
    pub fn mouse_reports_drag(&self) -> bool {
        self.term
            .lock()
            .mode()
            .intersects(TermMode::MOUSE_DRAG | TermMode::MOUSE_MOTION)
    }

    /// Whether the child is on the alternate screen, which has no scrollback.
    pub fn on_alternate_screen(&self) -> bool {
        self.term.lock().mode().contains(TermMode::ALT_SCREEN)
    }

    /// One wheel notch in the protocol the child negotiated, at `(column, line)`.
    pub fn wheel_report(
        &self,
        up: bool,
        column: usize,
        line: usize,
        modifiers: MouseModifiers,
    ) -> Vec<u8> {
        let button = if up {
            MouseButton::WheelUp
        } else {
            MouseButton::WheelDown
        };
        encode_mouse(
            *self.term.lock().mode(),
            MouseReport::Press(button),
            column,
            line,
            modifiers,
        )
    }

    /// One mouse event in the protocol the child negotiated, at `(column, line)`.
    pub fn mouse_report(
        &self,
        event: MouseReport,
        column: usize,
        line: usize,
        modifiers: MouseModifiers,
    ) -> Vec<u8> {
        encode_mouse(*self.term.lock().mode(), event, column, line, modifiers)
    }

    /// The child's pid, for the resource readout; `None` once the child is reaped.
    pub fn process_id(&self) -> Option<u32> {
        self.child.lock().unwrap().process_id()
    }

    /// The current grid size, as last set by `spawn` or `resize`.
    pub fn size(&self) -> TermSize {
        *self.size.lock().unwrap()
    }

    /// Read the grid with the `Term` locked: paint from it and return — no blocking, no callbacks.
    pub fn with_term<R>(&self, f: impl FnOnce(&mut Term<EventProxy>) -> R) -> R {
        f(&mut self.term.lock())
    }

    /// Everything since the last call, without blocking. Reaps a child at EOF and emits `Exit` once.
    pub fn drain_events(&self) -> Vec<TermEvent> {
        let mut events: Vec<TermEvent> = self.events.try_iter().collect();

        // Reaping happens here and nowhere else; the flag keeps a slow reaper from reporting the same exit twice.
        if self.eof.load(Ordering::Acquire) && !self.exit_reported.load(Ordering::Acquire) {
            let status = self.child.lock().unwrap().try_wait();
            if let Ok(Some(status)) = status {
                self.exit_reported.store(true, Ordering::Release);
                events.push(TermEvent::Exit(std_status(status)));
            }
        }
        events
    }

    /// The child's exit status, once it has exited and been reaped.
    pub fn exit_status(&self) -> Option<ExitStatus> {
        // `try_wait` answers with the status it already reaped, so this stays readable after the exit is reported.
        self.child
            .lock()
            .unwrap()
            .try_wait()
            .ok()
            .flatten()
            .map(std_status)
    }

    /// The visible grid as plain text, for tests.
    pub fn snapshot(&self) -> TermSnapshot {
        self.with_term(|term| {
            let mode = *term.mode();
            let content = term.renderable_content();
            let display_offset = content.display_offset;
            let cursor = content.cursor;
            let grid = term.grid();
            let columns = grid.columns();
            let screen_lines = grid.screen_lines();

            let mut lines = Vec::with_capacity(screen_lines);
            for screen_line in 0..screen_lines {
                // Screen row 0 is the top of the viewport.
                let row = &grid[Line(screen_line as i32 - display_offset as i32)];
                let mut text = String::with_capacity(columns);
                for column in 0..columns {
                    let cell = &row[Column(column)];
                    // A wide character holds two cells; only the glyph is text.
                    if cell
                        .flags
                        .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
                    {
                        continue;
                    }
                    text.push(cell.c);
                }
                lines.push(text.trim_end().to_string());
            }

            TermSnapshot {
                columns,
                screen_lines,
                lines,
                // alacritty's cursor line counts from the top of the screen; history above it is not part of the count.
                cursor: (cursor.point.line.0.max(0) as usize, cursor.point.column.0),
                cursor_visible: mode.contains(TermMode::SHOW_CURSOR)
                    && !matches!(cursor.shape, CursorShape::Hidden),
                alt_screen: mode.contains(TermMode::ALT_SCREEN),
                display_offset,
            }
        })
    }

    /// Signal the child to stop and reap it. Idempotent.
    pub fn kill(&mut self) {
        let mut child = self.child.lock().unwrap();
        // Already gone: nothing to signal, and the status stays readable.
        if matches!(child.try_wait(), Ok(Some(_))) {
            return;
        }
        // Hangup with a grace period, then a kill; either half can find the child already gone.
        let _ = child.kill();
        let _ = child.wait();
    }
}

impl Drop for TermSession {
    fn drop(&mut self) {
        // The reader thread holds a clone of the pty master, so dropping the session alone would leave
        // the child running — tab close relies on this.
        self.kill();
    }
}

/// Picks `OSC 7501` bodies out of the child's output, across reads.
#[derive(Default)]
struct ProgramStatusScanner {
    /// An unfinished sequence, or what may be the start of one, from the last read.
    carry: Vec<u8>,
}

impl ProgramStatusScanner {
    const START: &'static [u8] = b"\x1b]7501;";
    /// A report is at most ~2.8 KB; anything longer is not one.
    const MAX: usize = 4096;

    fn feed(&mut self, bytes: &[u8]) -> Vec<String> {
        let joined;
        let data = if self.carry.is_empty() {
            bytes
        } else {
            joined = [std::mem::take(&mut self.carry).as_slice(), bytes].concat();
            &joined[..]
        };
        let mut bodies = Vec::new();
        let mut at = 0;
        while let Some(found) = find(&data[at..], Self::START) {
            let start = at + found;
            let body = start + Self::START.len();
            // BEL or ST ends it; any other ESC means it was cut off.
            match data[body..].iter().position(|&b| b == 0x07 || b == 0x1b) {
                Some(end) if data[body + end] == 0x07 => {
                    bodies.push(String::from_utf8_lossy(&data[body..body + end]).into_owned());
                    at = body + end + 1;
                }
                Some(end) if body + end + 1 == data.len() => {
                    // ESC as the last byte: ST may be split across reads.
                    self.carry = data[start..].to_vec();
                    return bodies;
                }
                Some(end) => {
                    if data[body + end + 1] == b'\\' {
                        bodies.push(String::from_utf8_lossy(&data[body..body + end]).into_owned());
                    }
                    at = body + end + 1;
                }
                None => {
                    if data.len() - start <= Self::MAX {
                        self.carry = data[start..].to_vec();
                    }
                    return bodies;
                }
            }
        }
        // A tail that could be the first bytes of the next sequence.
        let tail = data.len().saturating_sub(Self::START.len() - 1).max(at);
        if let Some(keep) = (tail..data.len()).find(|&i| Self::START.starts_with(&data[i..])) {
            self.carry = data[keep..].to_vec();
        }
        bodies
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// What a status report says, as an agent status: `Some(None)` is `clear`, `None` is not a report
/// this reads (an unknown state). `done` and `error` are a turn that ended, which is idle here.
fn program_status_of(body: &str) -> Option<Option<AgentStatus>> {
    let state = body
        .split(':')
        .find_map(|pair| pair.strip_prefix("state="))?;
    Some(match state {
        "working" => Some(AgentStatus::Working),
        "blocked" => Some(AgentStatus::Permission),
        "idle" | "done" | "error" => Some(AgentStatus::Idle),
        "clear" => None,
        _ => return None,
    })
}

/// A plain-text picture of the grid, for tests — not a render path.
#[derive(Debug, Clone, PartialEq)]
pub struct TermSnapshot {
    pub columns: usize,
    pub screen_lines: usize,
    /// Screen lines, top to bottom, trailing blanks trimmed.
    pub lines: Vec<String>,
    /// Cursor position as `(line, column)`, both from the top of the screen.
    pub cursor: (usize, usize),
    pub cursor_visible: bool,
    pub alt_screen: bool,
    pub display_offset: usize,
}

impl TermSnapshot {
    /// The whole grid joined with `\n`, trailing blank lines dropped.
    pub fn text(&self) -> String {
        let mut end = self.lines.len();
        while end > 0 && self.lines[end - 1].is_empty() {
            end -= 1;
        }
        self.lines[..end].join("\n")
    }
}

/// The buttons xterm gives a number to; the wheel is 64/65 in the same table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
    WheelUp,
    WheelDown,
}

/// One mouse event, for a child that asked to be told about them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseReport {
    Press(MouseButton),
    Release(MouseButton),
    /// The pointer moved; `None` is a bare move, which only `?1003` asks for.
    Motion(Option<MouseButton>),
}

/// The modifiers a mouse report can express, as xterm adds them to the button code (4, 8, 16).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MouseModifiers {
    pub shift: bool,
    pub alt: bool,
    pub control: bool,
}

/// xterm's encoding of one mouse event: buttons 0–2, the wheel's 64/65, modifiers added (4
/// shift, 8 alt, 16 control), motion as bit 5, cells counted from 1.
///
/// Three protocols, and `?1006` (SGR) is the one that names the button on release and can count
/// past column 223.
fn encode_mouse(
    mode: TermMode,
    event: MouseReport,
    column: usize,
    line: usize,
    modifiers: MouseModifiers,
) -> Vec<u8> {
    let sgr = mode.contains(TermMode::SGR_MOUSE);
    let released = matches!(event, MouseReport::Release(_));
    let mut code = match event {
        MouseReport::Press(button) => button_code(button),
        // Only SGR can say which button came up.
        MouseReport::Release(button) if sgr => button_code(button),
        MouseReport::Release(_) => 3,
        // Bit 5 is motion; `3` in the low bits is "no button", so a bare move is 35.
        MouseReport::Motion(held) => 32 + held.map(button_code).unwrap_or(3),
    };
    if modifiers.shift {
        code += 4;
    }
    if modifiers.alt {
        code += 8;
    }
    if modifiers.control {
        code += 16;
    }
    let column = column + 1;
    let line = line + 1;

    if sgr {
        let final_byte = if released { 'm' } else { 'M' };
        format!("\x1b[<{code};{column};{line}{final_byte}").into_bytes()
    } else if mode.contains(TermMode::UTF8_MOUSE) {
        let mut bytes = b"\x1b[M".to_vec();
        bytes.push((code + 32) as u8);
        for value in [32 + column, 32 + line] {
            let mut buffer = [0u8; 4];
            let encoded = char::from_u32(value as u32)
                .unwrap_or('?')
                .encode_utf8(&mut buffer);
            bytes.extend_from_slice(encoded.as_bytes());
        }
        bytes
    } else {
        // `code + 32` fits a byte for the button, but not for the two coordinates: xterm clamps them.
        vec![
            0x1b,
            b'[',
            b'M',
            (code + 32) as u8,
            (32 + column).min(255) as u8,
            (32 + line).min(255) as u8,
        ]
    }
}

/// xterm's number for a button. The wheel's are 64 (up) and 65 (down).
fn button_code(button: MouseButton) -> usize {
    match button {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
        MouseButton::WheelUp => 64,
        MouseButton::WheelDown => 65,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn size() -> TermSize {
        TermSize::new(80, 24)
    }

    /// Poll until `check` passes, draining events so the reader is never blocked behind an unread channel.
    fn wait_for(mut check: impl FnMut() -> bool, _session: &TermSession) -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if check() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    fn sh(script: &str) -> TermSession {
        let spec = TermSpec {
            command: "/bin/sh".to_string(),
            args: vec!["-c".to_string(), script.to_string()],
            ..TermSpec::shell(std::env::temp_dir(), size())
        };
        TermSession::spawn(spec).expect("spawn")
    }

    #[cfg(windows)]
    #[test]
    fn the_default_shell_is_cmd() {
        let spec = TermSpec::shell(std::env::temp_dir(), size());
        assert!(
            spec.command.to_lowercase().ends_with("cmd.exe"),
            "{}",
            spec.command
        );
        let session = TermSession::spawn(spec).expect("spawn cmd");
        session.write(b"echo syro-ok\r\n").expect("write");
        assert!(
            wait_for(|| session.snapshot().text().contains("syro-ok"), &session),
            "cmd never answered: {:?}",
            session.snapshot()
        );
    }

    /// A verbatim (`\\?\`) cwd is what the app's canonical paths are on Windows,
    /// and cmd.exe treats it as UNC: it must be simplified before the spawn.
    #[cfg(windows)]
    #[test]
    fn a_verbatim_cwd_starts_the_shell_in_that_directory() {
        let dir = std::env::temp_dir().canonicalize().expect("temp dir");
        let spec = TermSpec {
            cwd: dir.clone(),
            ..TermSpec::shell(&dir, size())
        };
        let session = TermSession::spawn(spec).expect("spawn cmd");
        session.write(b"echo %CD%\r\n").expect("write");
        let expected = dunce::simplified(&dir)
            .to_string_lossy()
            .to_lowercase();
        assert!(
            wait_for(
                || session.snapshot().text().to_lowercase().contains(&expected),
                &session
            ),
            "cmd did not start in {expected}: {:?}",
            session.snapshot()
        );
    }

    #[test]
    fn a_child_gets_a_utf8_locale_only_when_the_app_has_none() {
        let empty = |_: &str| None;
        assert_eq!(fallback_locale(empty), Some(("LANG", "en_US.UTF-8")));

        let c = |_: &str| Some(std::ffi::OsString::from("C"));
        assert_eq!(fallback_locale(c), None, "an explicit locale is respected");

        // Any one of the three is a locale, even when it is empty of a value.
        let ctype = |name: &str| (name == "LC_CTYPE").then(|| std::ffi::OsString::new());
        assert_eq!(fallback_locale(ctype), Some(("LANG", "en_US.UTF-8")));
    }

    #[test]
    fn output_reaches_the_grid() {
        let session = sh("printf 'hello world\\r\\n'");
        assert!(
            wait_for(
                || session.snapshot().text().contains("hello world"),
                &session
            ),
            "grid never showed the child's output: {:?}",
            session.snapshot()
        );
        let snapshot = session.snapshot();
        assert_eq!(snapshot.columns, 80);
        assert_eq!(snapshot.lines[0], "hello world");
        // The child left the cursor on the line below what it printed.
        assert_eq!(snapshot.cursor, (1, 0));
    }

    #[test]
    fn a_subscriber_gets_the_same_bytes_the_grid_did() {
        let session = sh("printf 'seen by two readers\\r\\n'; sleep 5");
        let bytes = session.subscribe();
        let mut seen = Vec::new();
        assert!(
            wait_for(
                || {
                    while let Some(chunk) = bytes.try_chunk() {
                        seen.extend(chunk);
                    }
                    seen.windows(4).any(|window| window == b"read")
                },
                &session
            ),
            "the subscriber never saw the child's output: {:?}",
            String::from_utf8_lossy(&seen)
        );
        // The same bytes the grid was fed: the panel's terminal and the app's
        // grid must not disagree about what the child said.
        let text = String::from_utf8_lossy(&seen).into_owned();
        assert!(text.contains("seen by two readers"), "{text:?}");
        assert!(session.snapshot().text().contains("seen by two readers"));
    }

    #[test]
    fn a_subscriber_that_stopped_reading_is_dropped_not_waited_for() {
        // The receiver is dropped at once, so the first forward finds it gone.
        let session = sh("printf 'nobody is listening\\r\\n'; sleep 5");
        drop(session.subscribe());
        // A second reader must still be served, and the reader thread must not
        // have stalled on the dead one: the grid is the proof it kept reading.
        let live = session.subscribe();
        assert!(
            wait_for(
                || live.try_chunk().is_some() || session.snapshot().text().contains("nobody"),
                &session
            ),
            "the reader thread stalled behind a dropped subscriber"
        );
        assert!(
            wait_for(|| session.subscribers.lock().unwrap().len() <= 1, &session),
            "the dropped subscriber is still in the list"
        );
    }

    #[test]
    fn a_subscriber_that_falls_behind_is_told_it_is() {
        let (frames, bytes) = std::sync::mpsc::sync_channel(2);
        let lagged = Arc::new(AtomicBool::new(false));
        let subscribers = Mutex::new(vec![Subscriber {
            frames,
            lagged: Arc::clone(&lagged),
        }]);

        forward(&subscribers, b"first");
        forward(&subscribers, b"second");
        assert!(
            !lagged.load(Ordering::Relaxed),
            "a queued frame is not a drop"
        );
        // The queue is full: this frame is dropped, and the reader has to be told
        // its screen is no longer the grid.
        forward(&subscribers, b"third");
        assert!(lagged.load(Ordering::Relaxed), "the drop was not recorded");

        let subscription = Subscription {
            frames: bytes,
            lagged: Arc::clone(&lagged),
        };
        assert_eq!(
            subscription
                .recv_timeout(Duration::from_millis(10))
                .unwrap(),
            b"first"
        );
        // Taken, not just read: a reader asks once and then repaints.
        assert!(subscription.lagged());
        assert!(!subscription.lagged(), "lagged() did not clear the flag");

        // A reader that is gone is dropped from the list rather than written to
        // forever.
        drop(subscription);
        forward(&subscribers, b"fourth");
        assert!(
            subscribers.lock().unwrap().is_empty(),
            "a disconnected subscriber was kept"
        );
    }

    #[test]
    fn a_subscription_hands_over_what_it_caught_and_drains_the_rest() {
        let session = sh("printf 'one\\r\\ntwo\\r\\n'; sleep 5");
        let bytes = session.subscribe();
        assert!(
            wait_for(|| bytes.try_chunk().is_some(), &session),
            "nothing ever arrived"
        );
        bytes.drain();
        // Nothing left: the caller is about to be given the grid, which already
        // holds these frames.
        assert!(bytes.try_chunk().is_none(), "drain left a frame behind");
    }

    #[test]
    fn title_escape_becomes_an_event() {
        let session = sh("printf '\\033]0;agent is thinking\\007'");
        let mut titles = Vec::new();
        let seen = wait_for(
            || {
                for event in session.drain_events() {
                    if let TermEvent::Title(title) = event {
                        titles.push(title);
                    }
                }
                !titles.is_empty()
            },
            &session,
        );
        assert!(seen, "no title event arrived");
        assert_eq!(titles, vec!["agent is thinking".to_string()]);
    }

    #[test]
    fn program_status_is_read_across_reads() {
        let mut scanner = ProgramStatusScanner::default();
        assert_eq!(scanner.feed(b"text\x1b]75"), Vec::<String>::new());
        assert_eq!(scanner.feed(b"01;?\x1b"), Vec::<String>::new());
        assert_eq!(scanner.feed(b"\\\x1b[c"), vec!["?".to_string()]);
        assert_eq!(
            scanner.feed(b"\x1b]7501;state=working:app=pi\x07\x1b]7501;state=clear\x1b\\"),
            vec![
                "state=working:app=pi".to_string(),
                "state=clear".to_string()
            ]
        );
        assert_eq!(
            program_status_of("state=blocked:app=pi"),
            Some(Some(AgentStatus::Permission))
        );
        assert_eq!(program_status_of("state=clear"), Some(None));
        assert_eq!(program_status_of("state=dancing"), None);
    }

    #[test]
    fn program_status_query_is_answered_and_reports_become_events() {
        // The child reads our reply back and prints it, then reports a state.
        let session = sh(
            "stty raw -echo; printf '\\033]7501;?\\033\\\\'; r=$(dd bs=1 count=9 2>/dev/null | od -An -c | tr -d ' \\n'); echo \"got:$r\"; printf '\\033]7501;state=working:app=pi\\033\\\\'; sleep 30",
        );
        let mut statuses = Vec::new();
        let seen = wait_for(
            || {
                for event in session.drain_events() {
                    if let TermEvent::ProgramStatus(status) = event {
                        statuses.push(status);
                    }
                }
                !statuses.is_empty()
            },
            &session,
        );
        assert!(seen, "no status arrived: {:?}", session.snapshot());
        assert_eq!(statuses, vec![Some(AgentStatus::Working)]);
        assert!(
            session.snapshot().text().contains("got:033]7501;?033"),
            "the query was not answered: {:?}",
            session.snapshot().text()
        );
    }

    #[test]
    fn exit_status_is_reported_once() {
        let session = sh("exit 3");
        let mut exits = Vec::new();
        let seen = wait_for(
            || {
                for event in session.drain_events() {
                    if let TermEvent::Exit(status) = event {
                        exits.push(status);
                    }
                }
                !exits.is_empty()
            },
            &session,
        );
        assert!(seen, "the child's exit was never reported");
        assert_eq!(exits.len(), 1);
        assert_eq!(exits[0].code(), Some(3));
        assert_eq!(session.exit_status().and_then(|s| s.code()), Some(3));
    }

    #[test]
    fn typed_bytes_reach_the_child() {
        let session = sh("cat");
        session.write(b"typed input\n").expect("write");
        assert!(
            wait_for(
                || session.snapshot().text().contains("typed input"),
                &session
            ),
            "cat never echoed the input back: {:?}",
            session.snapshot()
        );
    }

    #[test]
    fn resize_reaches_the_grid_and_the_child() {
        let session = sh("sleep 30");
        session.resize(40, 10).expect("resize");
        assert_eq!(session.size(), TermSize::new(40, 10));
        let snapshot = session.snapshot();
        assert_eq!(snapshot.columns, 40);
        assert_eq!(snapshot.screen_lines, 10);
        // `stty size` asks the kernel, which is the PTY, not the grid.
        let session = sh("stty size");
        assert!(
            wait_for(|| session.snapshot().text().contains("24 80"), &session),
            "the child saw a different size: {:?}",
            session.snapshot()
        );
    }

    #[test]
    fn scrollback_is_reachable() {
        // 30 lines into a 24-line screen: the top 6 are history, not screen.
        let session = sh("i=1; while [ $i -le 30 ]; do echo line$i; i=$((i+1)); done");
        assert!(
            wait_for(|| session.snapshot().text().contains("line30"), &session),
            "the child never finished printing: {:?}",
            session.snapshot()
        );
        let bottom = session.snapshot();
        assert!(
            !bottom.text().contains("line1\n"),
            "line1 should have scrolled off"
        );
        session.scroll(100);
        let top = session.snapshot();
        assert!(
            top.text().contains("line1"),
            "scrollback lost line1: {:?}",
            top.text()
        );
        assert!(top.display_offset > 0);
    }

    /// The child that asked for the mouse gets the wheel, in the protocol it negotiated.
    #[test]
    fn a_child_that_asked_for_the_mouse_gets_the_wheel() {
        use super::MouseModifiers;

        // `?1002` + `?1006`: drag reporting in SGR form, which is what a TUI
        // asks for.
        let session = sh("printf '\\033[?1002h\\033[?1006h'; sleep 30");
        assert!(
            wait_for(|| session.wants_mouse(), &session),
            "the child never asked"
        );
        assert!(!session.on_alternate_screen());
        assert_eq!(
            session.wheel_report(true, 0, 0, MouseModifiers::default()),
            b"\x1b[<64;1;1M"
        );
        assert_eq!(
            session.wheel_report(false, 9, 4, MouseModifiers::default()),
            b"\x1b[<65;10;5M"
        );
        // Shift, alt and control are 4, 8 and 16 on top of the button.
        assert_eq!(
            session.wheel_report(
                true,
                0,
                0,
                MouseModifiers {
                    shift: true,
                    alt: true,
                    control: true,
                }
            ),
            b"\x1b[<92;1;1M"
        );

        // `?1005` is the same report with one UTF-8 character per coordinate.
        let utf8 = sh("printf '\\033[?1000h\\033[?1005h'; sleep 30");
        assert!(
            wait_for(|| utf8.wants_mouse(), &utf8),
            "the child never asked"
        );
        assert_eq!(
            utf8.wheel_report(true, 0, 0, MouseModifiers::default()),
            b"\x1b[M\x60\x21\x21"
        );

        // Neither: xterm's original three bytes, coordinates offset by 32.
        let legacy = sh("printf '\\033[?1000h'; sleep 30");
        assert!(
            wait_for(|| legacy.wants_mouse(), &legacy),
            "the child never asked"
        );
        assert_eq!(
            legacy.wheel_report(true, 0, 0, MouseModifiers::default()),
            b"\x1b[M\x60\x21\x21"
        );
    }

    /// A click on a child that asked for the mouse: the button is named on the way down and, in
    /// SGR, on the way up.
    #[test]
    fn a_child_that_asked_for_the_mouse_gets_the_clicks() {
        use super::{MouseButton, MouseModifiers, MouseReport};

        let session = sh("printf '\\033[?1002h\\033[?1006h'; sleep 30");
        assert!(
            wait_for(|| session.wants_mouse(), &session),
            "the child never asked"
        );
        assert!(session.mouse_reports_drag(), "?1002 is drag reporting");

        assert_eq!(
            session.mouse_report(
                MouseReport::Press(MouseButton::Left),
                0,
                0,
                MouseModifiers::default()
            ),
            b"\x1b[<0;1;1M"
        );
        // The release says *which* button: `m` and the same code.
        assert_eq!(
            session.mouse_report(
                MouseReport::Release(MouseButton::Left),
                3,
                2,
                MouseModifiers::default()
            ),
            b"\x1b[<0;4;3m"
        );
        assert_eq!(
            session.mouse_report(
                MouseReport::Press(MouseButton::Right),
                0,
                0,
                MouseModifiers::default()
            ),
            b"\x1b[<2;1;1M"
        );
        // Motion is bit 5 of the code, and a move with no button held is "button
        // 3" in the low bits.
        assert_eq!(
            session.mouse_report(
                MouseReport::Motion(Some(MouseButton::Left)),
                0,
                0,
                MouseModifiers::default()
            ),
            b"\x1b[<32;1;1M"
        );
        assert_eq!(
            session.mouse_report(MouseReport::Motion(None), 0, 0, MouseModifiers::default()),
            b"\x1b[<35;1;1M"
        );
        // Control is 16 on top of the button.
        assert_eq!(
            session.mouse_report(
                MouseReport::Press(MouseButton::Left),
                0,
                0,
                MouseModifiers {
                    control: true,
                    ..MouseModifiers::default()
                }
            ),
            b"\x1b[<16;1;1M"
        );

        // The original protocol has no "which button came up": a release is 3.
        let legacy = sh("printf '\\033[?1000h'; sleep 30");
        assert!(
            wait_for(|| legacy.wants_mouse(), &legacy),
            "the child never asked"
        );
        assert!(!legacy.mouse_reports_drag(), "?1000 is clicks only");
        assert_eq!(
            legacy.mouse_report(
                MouseReport::Release(MouseButton::Left),
                0,
                0,
                MouseModifiers::default()
            ),
            b"\x1b[M\x23\x21\x21"
        );
    }

    /// A child that asked for nothing: what the wheel's three targets are decided from.
    #[test]
    fn an_ordinary_child_asks_for_neither() {
        let session = sh("sleep 30");
        assert!(!session.wants_mouse());
        assert!(!session.on_alternate_screen());

        // And the alternate screen is what a TUI switches to: no scrollback
        // there, which is why the wheel has to go to the child instead.
        let alt = sh("printf '\\033[?1049h'; sleep 30");
        assert!(
            wait_for(|| alt.on_alternate_screen(), &alt),
            "the child never switched"
        );
        assert!(!alt.wants_mouse());
    }

    #[test]
    fn wide_characters_keep_their_columns() {
        let session = sh("printf '\\344\\270\\255\\346\\226\\207\\r\\n'");
        assert!(
            wait_for(|| session.snapshot().text().contains("中文"), &session),
            "CJK never landed: {:?}",
            session.snapshot()
        );
        // Two wide cells, and the cursor is past both of them.
        assert_eq!(session.snapshot().cursor, (1, 0));
    }
}
