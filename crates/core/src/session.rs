//! Past agent sessions: discovery of the transcripts the agents write, and the one-pass summary
//! the history panel lists.
//!
//! The file layouts are Claude Code (`~/.claude/projects/<encoded-cwd>/<id>.jsonl`), Pi
//! (`~/.pi/agent/sessions/--<encoded-cwd>--/*.jsonl`) and Codex's dated rollouts. OpenCode keeps
//! its history in SQLite instead, so it is read through the same database as its usage. Records
//! are counted through a shallow borrowed struct, so a 27 MB transcript costs a byte scan rather
//! than a `Value` per line.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fs::File;
use std::hash::{Hash as _, Hasher as _};
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Datelike as _, Local, NaiveDate};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use rust_i18n::t;

use crate::model::Worktree;

/// How many transcripts a scan parses, newest first; everything past it is reported as a total.
pub const DEFAULT_LIMIT: usize = 250;

/// How many days the usage summary keeps: a turn older than this is still listed
/// as a session, and is just not in the strip's numbers.
pub const USAGE_DAYS: i32 = 14;

/// The head zone: the first 64 KB, where the records that name the session are.
const HEAD_BYTES: u64 = 64 * 1024;

/// How many of the newest turn records are kept for the preview. A turn that ends in tool
/// traffic writes a run of textless records, so the newest record with text is often a dozen
/// lines up.
const TAIL_LINES: usize = 16;

/// How many characters of a turn's text the preview keeps.
const PREVIEW_CHARS: usize = 240;

/// How much of a transcript's tail a handoff reads, and how many characters it
/// keeps. A handoff is a first message, not a transcript: it carries the task and
/// the newest turns, and everything older is dropped.
const HANDOFF_TAIL_BYTES: u64 = 128 * 1024;
const HANDOFF_CHARS: usize = 4000;

/// How many characters one turn contributes to a handoff before it is cut.
const HANDOFF_TURN_CHARS: usize = 400;

/// The agent a transcript belongs to; its spelling is the `AgentSpec` id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionAgent {
    Claude,
    Pi,
    Codex,
    /// OpenCode keeps its history in SQLite rather than one transcript per
    /// session, so it is discovered and read through its database.
    #[serde(rename = "opencode")]
    OpenCode,
}

impl SessionAgent {
    pub fn id(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Pi => "pi",
            Self::Codex => "codex",
            Self::OpenCode => "opencode",
        }
    }

    /// The session layout an agent preset keeps, by its id.
    pub fn from_id(id: &str) -> Option<Self> {
        match id {
            "claude" => Some(Self::Claude),
            "pi" => Some(Self::Pi),
            "codex" => Some(Self::Codex),
            "opencode" => Some(Self::OpenCode),
            _ => None,
        }
    }

    pub fn display(self) -> &'static str {
        match self {
            Self::Claude => "Claude Code",
            Self::Pi => "Pi",
            Self::Codex => "Codex",
            Self::OpenCode => "OpenCode",
        }
    }
}

/// One past session, as the panel lists it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Session {
    pub agent: SessionAgent,
    /// The agent's own session id (the file stem for Pi, the `sessionId` record for Claude).
    pub id: String,
    /// The transcript itself — also what `pi --session` is handed.
    pub path: PathBuf,
    /// The directory the agent worked in; a session without one cannot be placed, so it is not listed.
    pub cwd: PathBuf,
    /// The last path component of `cwd`: what the panel groups by.
    pub workspace: String,
    pub title: String,
    pub preview: String,
    pub model: Option<String>,
    pub provider: Option<String>,
    pub messages: u64,
    /// Last write, in epoch milliseconds: what the rows sort by.
    pub updated_ms: i64,
}

/// Where the two agents keep their transcripts under a home directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRoots {
    pub claude: PathBuf,
    pub pi: PathBuf,
    /// Codex sorts its rollouts into year/month/day directories under this root rather
    /// than one bucket per project.
    pub codex: PathBuf,
    /// OpenCode's SQLite database: its history and its usage.
    pub opencode: PathBuf,
    /// Gemini CLI's `tmp`, one directory per project with a `chats` inside: usage only.
    pub gemini: PathBuf,
}

impl SessionRoots {
    pub fn from_home(home: &Path) -> Self {
        Self {
            claude: home.join(".claude").join("projects"),
            pi: home.join(".pi").join("agent").join("sessions"),
            codex: home.join(".codex").join("sessions"),
            opencode: home.join(".local/share/opencode/opencode.db"),
            gemini: home.join(".gemini").join("tmp"),
        }
    }
}

impl Default for SessionRoots {
    /// This user's roots, from the home directory.
    fn default() -> Self {
        Self::from_home(&crate::home_dir())
    }
}

/// The result of one scan: what to list, how much was there to list, and what the
/// transcripts it read have cost lately.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Scan {
    pub sessions: Vec<Session>,
    /// Every transcript discovered, before the cap.
    pub total: usize,
    pub usage: UsageSummary,
}

/// Tokens and money one stretch of transcripts used: recorded prices where available,
/// otherwise an OpenAI standard-rate estimate. Unpriced usage is not free usage.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Usage {
    pub tokens: u64,
    pub cost: f64,
    /// At least part of the cost was calculated from official OpenAI rates.
    pub estimated: bool,
    /// Some tokens have no known price; `cost` is only the priced subtotal.
    pub unpriced: bool,
}

impl Usage {
    fn add(&mut self, usage: Self) {
        self.tokens += usage.tokens;
        self.cost += usage.cost;
        self.estimated |= usage.estimated;
        self.unpriced |= usage.unpriced;
    }
}

/// One day of the usage chart, oldest first.
#[derive(Debug, Clone, PartialEq)]
pub struct UsageDay {
    /// The day as `MM-DD`, in the reader's own timezone.
    pub label: String,
    pub tokens: u64,
    pub cost: f64,
    /// One entry per model that spent anything that day, biggest first: the stack a
    /// chart draws in that day's column, and the reason the days are kept per model
    /// rather than as one number.
    pub models: Vec<UsageSlice>,
}

/// One model's share of one day.
#[derive(Debug, Clone, PartialEq)]
pub struct UsageSlice {
    /// The model the provider answered with (`claude-opus-5-5`), or the agent's own
    /// name for a turn that recorded none.
    pub model: String,
    pub tokens: u64,
    pub cost: f64,
    pub estimated: bool,
    pub unpriced: bool,
}

/// One agent, or one provider under an agent: the chart's second cut.
#[derive(Debug, Clone, PartialEq)]
pub struct UsageGroup {
    /// The provider that answered (`opencode-go`, `anthropic`), or the agent's own
    /// name for one that records no provider (`Claude Code`). Short on purpose: it
    /// is a label in a chart column, not a sentence.
    pub label: String,
    pub tokens: u64,
    pub cost: f64,
    pub estimated: bool,
    pub unpriced: bool,
}

/// What the transcripts a scan read have used over the last [`USAGE_DAYS`] days:
/// the strip's numbers, and both cuts of its chart.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UsageSummary {
    pub total: Usage,
    pub today: Usage,
    pub days: Vec<UsageDay>,
    /// The providers the tokens went through, biggest first, ties broken by name so
    /// two scans agree.
    pub providers: Vec<UsageGroup>,
    /// The models that answered, biggest first: the same turns cut the other way.
    pub models: Vec<UsageGroup>,
}

/// The local calendar day a moment falls on, as a key that sorts and subtracts.
/// Local, not UTC: a turn at nine in the morning belongs to the day the reader
/// was having, which is the day the chart's bars are.
fn local_day(ms: i64) -> Option<i32> {
    Some(
        DateTime::from_timestamp_millis(ms)?
            .with_timezone(&Local)
            .date_naive()
            .num_days_from_ce(),
    )
}

/// A day key back as the chart's `MM-DD`.
fn day_label(day: i32) -> String {
    NaiveDate::from_num_days_from_ce_opt(day)
        .map(|date| format!("{:02}-{:02}", date.month(), date.day()))
        .unwrap_or_default()
}

/// Each transcript's fold and bytes read, valid only while the file is the same size or longer.
#[derive(Debug, Default)]
pub struct Cache {
    entries: HashMap<PathBuf, Entry>,
    /// The price table the folds were costed with (see [`crate::pricing::generation`]).
    prices: u64,
}

#[derive(Debug, Clone)]
struct Entry {
    size: u64,
    fold: Fold,
}

/// Everything one transcript's pass accumulates.
#[derive(Debug, Default, Clone)]
struct Fold {
    /// Bytes consumed, always at a line boundary — where the next pass resumes.
    bytes: u64,
    messages: u64,
    id: Option<String>,
    cwd: Option<PathBuf>,
    model: Option<String>,
    provider: Option<String>,
    title: Option<String>,
    /// The preview's text, refreshed from the tail buffer at the end of a pass.
    preview: Option<String>,
    /// The response ids whose usage this transcript has already paid into the days: a
    /// resumed Codex session writes a whole new rollout and replays the earlier items
    /// into it, so the same response arrives twice, in two files. Hashed rather than
    /// kept whole: only identity matters, and a rollout holds a lot of them.
    seen: HashSet<u64>,
    /// The last `token_usage_record` this transcript carried, as the raw numbers it
    /// reported. Newer rollouts also write a `token_count` event saying the same thing
    /// about the same response — and, unlike the record, the event carries no id — so
    /// the event is recognised by those numbers and skipped, while an older rollout that
    /// has only the event is read from it.
    last_report: Option<(u64, u64, u64)>,
    /// What the priced turns in this transcript cost, by the local day they
    /// happened. The summary is a slice of this, and it is kept per day rather
    /// than per transcript because a session resumed for a week spends on several.
    days: BTreeMap<i32, DayFold>,
}

/// One priced turn's cost, and the two names a reader compares it by.
struct Spent {
    tokens: u64,
    cost: f64,
    estimated: bool,
    unpriced: bool,
    provider: String,
    model: String,
}

/// One transcript's usage on one local day.
#[derive(Debug, Default, Clone)]
struct DayFold {
    usage: Usage,
    /// That day's cut by provider.
    providers: BTreeMap<String, Usage>,
    /// And by model, which is what the day's column is stacked from.
    models: BTreeMap<String, Usage>,
}

impl DayFold {
    fn pay(&mut self, spent: Spent) {
        let usage = Usage {
            tokens: spent.tokens,
            cost: spent.cost,
            estimated: spent.estimated,
            unpriced: spent.unpriced,
        };
        self.usage.add(usage);
        self.providers.entry(spent.provider).or_default().add(usage);
        self.models.entry(spent.model).or_default().add(usage);
    }
}

/// One transcript file discovery found: what to parse and what to sort by.
#[derive(Debug, Clone)]
struct Candidate {
    agent: SessionAgent,
    path: PathBuf,
    mtime_ms: i64,
    size: u64,
}

/// Discover the newest `limit` transcripts under `roots` and summarise them. `cache` is carried
/// by the caller, so a later scan reads only what was appended.
pub fn scan(roots: &SessionRoots, cache: &mut Cache, limit: usize) -> Scan {
    scan_at(roots, cache, limit, now_ms())
}

/// The same scan, with the clock handed in: which days are "today" and inside the
/// usage window is the one thing a test cannot leave to the machine's own time.
pub fn scan_at(roots: &SessionRoots, cache: &mut Cache, limit: usize, now: i64) -> Scan {
    let mut candidates = discover(&roots.claude, SessionAgent::Claude);
    candidates.extend(discover(&roots.pi, SessionAgent::Pi));
    candidates.extend(discover_codex(&roots.codex));
    // Newest first, and the path breaks ties so two transcripts written in the
    // same millisecond keep one order between scans.
    candidates.sort_by(|left, right| {
        right
            .mtime_ms
            .cmp(&left.mtime_ms)
            .then_with(|| left.path.cmp(&right.path))
    });
    let mut total = candidates.len();
    candidates.truncate(limit);

    // A new price list re-costs everything: the folds hold costs, not prices.
    let prices = crate::pricing::generation();
    if cache.prices != prices {
        cache.entries.clear();
        cache.prices = prices;
    }
    let live: Vec<&Path> = candidates.iter().map(|c| c.path.as_path()).collect();
    cache
        .entries
        .retain(|path, _| live.contains(&path.as_path()));

    // The window's two ends: the day it is now, and the oldest day it keeps. A
    // record stamped in the future (a clock that ran ahead) is not in it.
    let today = local_day(now).unwrap_or_default();
    let oldest = today - (USAGE_DAYS - 1);
    // A resumed Codex session replays the earlier responses into its new rollout, so what
    // the folds already hold is what a replay is recognised against.
    let mut seen: HashSet<u64> = cache
        .entries
        .values()
        .flat_map(|entry| entry.fold.seen.iter().copied())
        .collect();
    let mut days: BTreeMap<i32, DayFold> = BTreeMap::new();
    let mut providers: BTreeMap<String, Usage> = BTreeMap::new();
    let mut models: BTreeMap<String, Usage> = BTreeMap::new();
    let mut usage = Usage::default();
    let mut merge = |fold_days: &BTreeMap<i32, DayFold>| {
        for (day, fold_day) in fold_days {
            if *day < oldest || *day > today {
                continue;
            }
            let merged = days.entry(*day).or_default();
            merged.usage.add(fold_day.usage);
            usage.add(fold_day.usage);
            for (label, group) in &fold_day.providers {
                providers.entry(label.clone()).or_default().add(*group);
            }
            for (label, group) in &fold_day.models {
                models.entry(label.clone()).or_default().add(*group);
                merged.models.entry(label.clone()).or_default().add(*group);
            }
        }
    };
    let mut sessions: Vec<Session> = Vec::new();
    for candidate in candidates {
        let listed = scan_one(&candidate, cache, &mut seen);
        // The fold lands in the cache whether or not the transcript could be listed,
        // so a session with no directory still pays into the numbers: the strip is
        // about the machine, not about the repository on screen.
        if let Some(fold) = cache.entries.get(&candidate.path).map(|entry| &entry.fold) {
            merge(&fold.days);
        }
        if let Some(session) = listed {
            sessions.push(session);
        }
    }
    // Gemini is counted but not listed: its chats are usage only. OpenCode's
    // history lives in the same database as its usage, so it is both.
    let since = now - (i64::from(USAGE_DAYS) + 1) * 86_400_000;
    let mut other: BTreeMap<i32, DayFold> = BTreeMap::new();
    for (ms, spent) in opencode_usage(&roots.opencode, since)
        .into_iter()
        .chain(gemini_usage(&roots.gemini, since))
    {
        if let Some(day) = local_day(ms) {
            other.entry(day).or_default().pay(spent);
        }
    }
    merge(&other);
    let (opencode, opencode_total) = opencode_sessions(&roots.opencode, limit);
    total += opencode_total;
    sessions.extend(opencode);
    sessions.sort_by(|left, right| {
        right
            .updated_ms
            .cmp(&left.updated_ms)
            .then_with(|| left.path.cmp(&right.path))
            .then_with(|| left.id.cmp(&right.id))
    });
    sessions.truncate(limit);

    // Both cuts, biggest first, ties broken by name so two scans agree.
    let ranked = |cuts: BTreeMap<String, Usage>| {
        let mut ranked: Vec<UsageGroup> = cuts
            .into_iter()
            .map(|(label, usage)| UsageGroup {
                label,
                tokens: usage.tokens,
                cost: usage.cost,
                estimated: usage.estimated,
                unpriced: usage.unpriced,
            })
            .collect();
        ranked.sort_by(|left, right| {
            right
                .tokens
                .cmp(&left.tokens)
                .then_with(|| left.label.cmp(&right.label))
        });
        ranked
    };
    let usage = UsageSummary {
        total: usage,
        today: days.get(&today).map(|day| day.usage).unwrap_or_default(),
        days: days
            .into_iter()
            .map(|(day, fold)| UsageDay {
                label: day_label(day),
                tokens: fold.usage.tokens,
                cost: fold.usage.cost,
                models: ranked(fold.models)
                    .into_iter()
                    .map(|group| UsageSlice {
                        model: group.label,
                        tokens: group.tokens,
                        cost: group.cost,
                        estimated: group.estimated,
                        unpriced: group.unpriced,
                    })
                    .collect(),
            })
            .collect(),
        providers: ranked(providers),
        models: ranked(models),
    };

    Scan {
        sessions,
        total,
        usage,
    }
}

/// Remove one session's stored history. For the file layouts that is the
/// transcript itself; for OpenCode it is the session's row in the database,
/// whose foreign keys cascade to its messages and queues. Returns whether
/// something was actually removed.
pub fn delete(session: &Session) -> bool {
    if session.agent != SessionAgent::OpenCode {
        return std::fs::remove_file(&session.path).is_ok();
    }
    let Ok(connection) = rusqlite::Connection::open(&session.path) else {
        return false;
    };
    let _ = connection.busy_timeout(std::time::Duration::from_secs(2));
    // Off by default in SQLite, and the cascade is what takes the messages with
    // the session.
    let _ = connection.execute_batch("PRAGMA foreign_keys = ON;");
    connection
        .execute(
            "delete from session_v2 where id = ?1",
            [session.id.as_str()],
        )
        .map(|removed| removed > 0)
        .unwrap_or(false)
}

/// The transcript of the session `id` that an agent started in `cwd`, once the agent has
/// written it: how a terminal that picked the id finds its own conversation.
pub fn transcript_of(
    roots: &SessionRoots,
    agent: SessionAgent,
    cwd: &Path,
    id: &str,
) -> Option<PathBuf> {
    match agent {
        // `<timestamp>_<id>.jsonl` in the cwd's bucket.
        SessionAgent::Pi => {
            let cwd = cwd.to_string_lossy();
            let bucket = format!(
                "--{}--",
                cwd.trim_start_matches(['/', '\\'])
                    .replace(['/', '\\', ':'], "-")
            );
            let suffix = format!("_{id}.jsonl");
            std::fs::read_dir(roots.pi.join(bucket))
                .ok()?
                .flatten()
                .map(|file| file.path())
                .find(|path| {
                    path.file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.ends_with(&suffix))
                })
        }
        // `rollout-<stamp>-<id>.jsonl` under the dated tree.
        SessionAgent::Codex => {
            let suffix = format!("-{id}.jsonl");
            discover_codex(&roots.codex)
                .into_iter()
                .map(|candidate| candidate.path)
                .find(|path| {
                    path.file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.ends_with(&suffix))
                })
        }
        // `<bucket>/<id>.jsonl`: the id is unique, so the bucket's encoding need not be guessed.
        SessionAgent::Claude => std::fs::read_dir(&roots.claude)
            .ok()?
            .flatten()
            .map(|bucket| bucket.path().join(format!("{id}.jsonl")))
            .find(|path| path.is_file()),
        // There is no file per session: the transcript is the database, and the
        // id says which row in it to read.
        SessionAgent::OpenCode => {
            let connection = open_opencode(&roots.opencode)?;
            connection
                .query_row("select 1 from session_v2 where id = ?1", [id], |_| Ok(()))
                .ok()
                .map(|()| roots.opencode.clone())
        }
    }
}

/// The first thing the user asked in a session, cut to the panel's title length.
/// Reads the head zone only, so a long transcript costs no more than a short one;
/// OpenCode's is one indexed row in its database.
pub fn first_prompt(agent: SessionAgent, id: &str, path: &Path) -> Option<String> {
    first_prompt_cut(agent, id, path, PREVIEW_CHARS)
}

/// The same read, cut to `limit`: the panel wants a title, a handoff wants the
/// whole request.
fn first_prompt_cut(agent: SessionAgent, id: &str, path: &Path, limit: usize) -> Option<String> {
    if agent == SessionAgent::OpenCode {
        return open_opencode(path)
            .and_then(|connection| first_user_text(&connection, id))
            .map(|text| one_line(&text, limit));
    }
    use std::io::Read as _;
    let mut reader = BufReader::new(File::open(path).ok()?.take(HEAD_BYTES));
    let (mut fold, mut tail) = (Fold::default(), VecDeque::new());
    let (mut first_user, mut line) = (String::new(), String::new());
    while first_user.is_empty() {
        line.clear();
        if reader.read_line(&mut line).ok()? == 0 {
            break;
        }
        consume(
            agent,
            &line,
            &mut fold,
            &mut tail,
            &mut first_user,
            true,
            &mut HashSet::new(),
        );
    }
    text_of(agent, &first_user).map(|text| one_line(&text, limit))
}

/// A compact handoff for continuing a transcript in a fresh session: the first
/// thing the user asked, then the newest turns that fit a budget, oldest first.
/// `None` when there is nothing readable to carry.
///
/// This is selection, not summarization — the newest turns are kept whole (cut to
/// one line each) rather than rewritten by a model. That is what makes "start a
/// new session" cost no model call and need no credentials. shortcut: selection
/// only, so a very long recent turn is cut; call an agent to summarise if a real
/// compression is wanted.
pub fn handoff(agent: SessionAgent, id: &str, path: &Path) -> Option<String> {
    let first = first_prompt_cut(agent, id, path, 2 * PREVIEW_CHARS);
    let turns = recent_turns(agent, id, path, HANDOFF_CHARS);
    if first.is_none() && turns.is_empty() {
        return None;
    }
    let mut out = String::from(
        "Continuing a previous session in a fresh context window. Below is what \
         carried over.\n\n",
    );
    if let Some(first) = first {
        out.push_str("Original request: ");
        out.push_str(&first);
        out.push_str("\n\n");
    }
    if !turns.is_empty() {
        out.push_str("Recent turns (oldest first):\n");
        for (role, text) in turns {
            out.push_str(&role);
            out.push_str(": ");
            out.push_str(&text);
            out.push('\n');
        }
    }
    out.push_str("\nPick up where this left off.");
    Some(out)
}

/// The newest readable turns in a transcript's tail, oldest first, each cut to one
/// line and all of them kept under `budget` characters.
fn recent_turns(
    agent: SessionAgent,
    id: &str,
    path: &Path,
    budget: usize,
) -> Vec<(String, String)> {
    if agent == SessionAgent::OpenCode {
        return opencode_recent_turns(path, id, budget);
    }
    let Ok(mut file) = File::open(path) else {
        return Vec::new();
    };
    let Ok(len) = file.metadata().map(|metadata| metadata.len()) else {
        return Vec::new();
    };
    let start = len.saturating_sub(HANDOFF_TAIL_BYTES);
    if file.seek(SeekFrom::Start(start)).is_err() {
        return Vec::new();
    }
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    // The tail may begin mid-record; that partial line is not one to read.
    if start > 0 {
        let _ = reader.read_line(&mut line);
    }
    let mut turns: VecDeque<(String, String)> = VecDeque::new();
    let mut total = 0usize;
    loop {
        line.clear();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        let Some((role, text)) = turn_of(agent, &line) else {
            continue;
        };
        let cut = one_line(&text, HANDOFF_TURN_CHARS);
        total += cut.chars().count() + role.chars().count() + 2;
        turns.push_back((role, cut));
        while total > budget && turns.len() > 1 {
            let (role, text) = turns.pop_front().expect("more than one turn");
            total -= text.chars().count() + role.chars().count() + 2;
        }
    }
    turns.into_iter().collect()
}

/// One readable turn from a transcript line: who said it and what they said.
/// Textless records — tool traffic, usage, model changes — are not turns.
fn turn_of(agent: SessionAgent, line: &str) -> Option<(String, String)> {
    let value: Value = serde_json::from_str(line).ok()?;
    let (role, content) = match agent {
        SessionAgent::Pi => {
            if value.get("type")?.as_str()? != "message" {
                return None;
            }
            let message = value.get("message")?;
            (
                message.get("role")?.as_str()?.to_string(),
                message.get("content")?,
            )
        }
        SessionAgent::Claude => {
            let kind = value.get("type")?.as_str()?;
            if kind != "user" && kind != "assistant" {
                return None;
            }
            (kind.to_string(), value.get("message")?.get("content")?)
        }
        SessionAgent::Codex => {
            let payload = value.get("payload")?;
            if payload.get("type")?.as_str()? != "message" {
                return None;
            }
            (
                payload.get("role")?.as_str()?.to_string(),
                payload.get("content")?,
            )
        }
        // OpenCode's turns are rows in SQLite, never lines in a transcript.
        SessionAgent::OpenCode => return None,
    };
    if role != "user" && role != "assistant" {
        return None;
    }
    texts(content).map(|text| (role, text))
}

/// The `<root>/<bucket>/*.jsonl` files under one agent's root — one level of subdirectory is
/// the shape both layouts have. A missing root is not an error: an agent that has never run has
/// no history.
fn discover(root: &Path, agent: SessionAgent) -> Vec<Candidate> {
    let Ok(buckets) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for bucket in buckets.flatten() {
        let Ok(files) = std::fs::read_dir(bucket.path()) else {
            continue;
        };
        for file in files.flatten() {
            let path = file.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("jsonl") {
                continue;
            }
            let Ok(metadata) = file.metadata() else {
                continue;
            };
            if !metadata.is_file() {
                continue;
            }
            found.push(Candidate {
                agent,
                mtime_ms: modified_ms(&metadata),
                size: metadata.len(),
                path,
            });
        }
    }
    found
}

/// The rollouts under Codex's root: `YYYY/MM/DD/rollout-*.jsonl`, and — in older builds
/// — the same files loose at the root, so the walk goes by depth rather than by shape.
fn discover_codex(root: &Path) -> Vec<Candidate> {
    /// Enough for the dated tree with room to spare; a deeper tree is not a layout Codex
    /// has, and walking one would be walking the rest of the home directory.
    const DEPTH: usize = 4;

    let mut found = Vec::new();
    let mut dirs = vec![(root.to_path_buf(), 0_usize)];
    while let Some((dir, depth)) = dirs.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if depth < DEPTH {
                    dirs.push((path, depth + 1));
                }
                continue;
            }
            if path.extension().and_then(|extension| extension.to_str()) != Some("jsonl") {
                continue;
            }
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            if !metadata.is_file() {
                continue;
            }
            found.push(Candidate {
                agent: SessionAgent::Codex,
                mtime_ms: modified_ms(&metadata),
                size: metadata.len(),
                path,
            });
        }
    }
    found
}

fn modified_ms(metadata: &std::fs::Metadata) -> i64 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|since| since.as_millis() as i64)
        .unwrap_or(0)
}

/// Fold one transcript, resuming from where the cache left off.
fn scan_one(candidate: &Candidate, cache: &mut Cache, seen: &mut HashSet<u64>) -> Option<Session> {
    let previous = cache.entries.get(&candidate.path);
    // A file that shrank was replaced: start again rather than read the middle
    // of a transcript whose head is gone.
    let start = match previous {
        Some(entry) if entry.size <= candidate.size => entry.fold.clone(),
        _ => Fold::default(),
    };
    let fold = fold_file(candidate, start, seen).ok()?;
    let cwd = fold.cwd.clone()?;
    let workspace = cwd
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| cwd.display().to_string());
    let session = Session {
        agent: candidate.agent,
        id: fold
            .id
            .clone()
            .unwrap_or_else(|| file_stem(&candidate.path)),
        path: candidate.path.clone(),
        cwd,
        workspace,
        title: fold
            .title
            .clone()
            .unwrap_or_else(|| "(no prompt recorded)".to_string()),
        preview: fold.preview.clone().unwrap_or_default(),
        model: fold.model.clone(),
        provider: fold.provider.clone(),
        messages: fold.messages,
        updated_ms: candidate.mtime_ms,
    };
    cache.entries.insert(
        candidate.path.clone(),
        Entry {
            size: candidate.size,
            fold,
        },
    );
    Some(session)
}

fn file_stem(path: &Path) -> String {
    path.file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Read `candidate` from `start`'s byte offset to the end, folding records in.
fn fold_file(
    candidate: &Candidate,
    mut fold: Fold,
    seen: &mut HashSet<u64>,
) -> std::io::Result<Fold> {
    let head_zone = fold.bytes == 0;
    let mut reader = BufReader::new(File::open(&candidate.path)?);
    if fold.bytes > 0 {
        reader.seek(SeekFrom::Start(fold.bytes))?;
    }
    let mut tail: VecDeque<String> = VecDeque::new();
    // Only the first user turn is the title; a later one is just a prompt.
    let mut first_user = String::new();
    let mut line = String::new();
    loop {
        line.clear();
        let read = reader.read_line(&mut line)?;
        if read == 0 {
            break;
        }
        fold.bytes += read as u64;
        let head = head_zone && fold.bytes <= HEAD_BYTES;
        consume(
            candidate.agent,
            &line,
            &mut fold,
            &mut tail,
            &mut first_user,
            head,
            seen,
        );
    }

    if fold.title.is_none() {
        fold.title =
            text_of(candidate.agent, &first_user).map(|text| one_line(&text, PREVIEW_CHARS));
    }
    fold.preview = tail
        .iter()
        .rev()
        .find_map(|record| text_of(candidate.agent, record))
        .map(|text| one_line(&text, PREVIEW_CHARS));
    Ok(fold)
}

/// Fold one line in. `head` says the record is inside the head zone, which is
/// where the session's own name and directory are read from.
#[allow(clippy::too_many_arguments)]
fn consume(
    agent: SessionAgent,
    line: &str,
    fold: &mut Fold,
    tail: &mut VecDeque<String>,
    first_user: &mut String,
    head: bool,
    seen: &mut HashSet<u64>,
) {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return;
    }
    // Borrowed and shallow on purpose: this runs for every line, and `content` must not be walked.
    let record: Record = match serde_json::from_str(trimmed) {
        Ok(record) => record,
        Err(_) => return,
    };
    let mut is_turn = false;
    let mut by_user = false;
    match agent {
        SessionAgent::Pi => {
            if record.kind == Some("session") {
                fold.id = record.id.map(str::to_string).or(fold.id.take());
                fold.cwd = record.cwd.map(PathBuf::from).or(fold.cwd.take());
            }
            if record.kind == Some("model_change") {
                fold.provider = record.provider.map(str::to_string).or(fold.provider.take());
                fold.model = record.model_id.map(str::to_string).or(fold.model.take());
            }
            if record.kind == Some("message") {
                let role = record.message.as_ref().and_then(|message| message.role);
                is_turn = matches!(role, Some("user") | Some("assistant"));
                by_user = role == Some("user");
            }
        }
        SessionAgent::Codex => {
            let payload = record.payload.as_ref();
            if record.kind == Some("session_meta") {
                // The rollout's own name is the session's, and a sub-agent's rollout
                // carries its parent's there while `id` is its own.
                fold.id = payload
                    .and_then(|payload| payload.session_id.or(payload.id))
                    .map(str::to_string)
                    .or(fold.id.take());
                fold.cwd = payload
                    .and_then(|payload| payload.cwd)
                    .map(PathBuf::from)
                    .or(fold.cwd.take());
            }
            if record.kind == Some("turn_context") {
                fold.model = payload
                    .and_then(|payload| payload.model)
                    .map(str::to_string)
                    .or(fold.model.take());
            }
            if record.kind == Some("response_item")
                && payload.and_then(|payload| payload.kind) == Some("message")
            {
                let role = payload.and_then(|payload| payload.role);
                is_turn = matches!(role, Some("user") | Some("assistant"));
                by_user = role == Some("user");
            }
        }
        SessionAgent::Claude => {
            if let Some(cwd) = record.cwd
                && head
            {
                fold.cwd = Some(PathBuf::from(cwd));
            }
            if head && let Some(id) = record.session_id {
                fold.id = Some(id.to_string());
            }
            is_turn = matches!(record.kind, Some("user") | Some("assistant"));
            // Claude writes tool results as `user` records too: only one with text is the user's.
            by_user = record.kind == Some("user")
                && head
                && first_user.is_empty()
                && text_of(agent, trimmed).is_some();
            if let Some(model) = record
                .message
                .as_ref()
                .and_then(|message| message.model)
                .filter(|_| record.kind == Some("assistant"))
            {
                fold.model = Some(model.to_string());
            }
        }
        // OpenCode never comes through here: its history is read from SQLite.
        SessionAgent::OpenCode => return,
    }

    // Codex reports usage on records of its own, which are not turns: they are paid in
    // before anything else decides this line is not a turn and returns.
    if agent == SessionAgent::Codex {
        fold_codex(&record, fold, seen);
    }

    if !is_turn {
        return;
    }
    fold.messages += 1;
    // What the turn cost, under the day it happened. A turn that recorded no
    // usage — every user turn, and every tool result — adds nothing to either cut.
    if let Some(spent) = record.spent(agent) {
        // A record with no readable timestamp is filed under today rather than
        // dropped: both agents write one on every priced turn, and dropping the
        // rare one would quietly shrink the total.
        let day = record
            .day()
            .or_else(|| local_day(now_ms()))
            .unwrap_or_default();
        fold.days.entry(day).or_default().pay(spent);
    }
    // The newest few turns for the preview, and the first user turn for the title.
    tail.push_back(trimmed.to_string());
    while tail.len() > TAIL_LINES {
        tail.pop_front();
    }
    if head && first_user.is_empty() && by_user {
        *first_user = trimmed.to_string();
    }
}

/// Pay one Codex usage report into the fold, whichever of the two reports it is.
///
/// Interactive Codex sessions have been reported not to write either report at all, in
/// which case a rollout has no tokens in it to find: what is read here is what the file
/// says, and a session the agent did not account for shows as a session with no usage.
///
/// A newer rollout writes a `token_usage_record` per response and a `token_count` event
/// saying the same thing about it, and the event carries no id to say so — the numbers
/// are what matches the pair, in whichever order they arrive. An older rollout has only
/// the event, and is read from it.
fn fold_codex(record: &Record<'_>, fold: &mut Fold, seen: &mut HashSet<u64>) {
    let Some(payload) = record.payload.as_ref() else {
        return;
    };
    let usage = if record.kind == Some("token_usage_record") {
        payload.usage.as_ref()
    } else if record.kind == Some("event_msg") && payload.kind == Some("token_count") {
        payload
            .info
            .as_ref()
            .and_then(|info| info.last_token_usage.as_ref())
    } else {
        return;
    };
    let Some(usage) = usage else {
        return;
    };

    let (tokens, numbers) = usage.spent();
    let repeated = fold.last_report == Some(numbers);
    fold.last_report = Some(numbers);
    if repeated || tokens == 0 {
        return;
    }
    // A resumed session replays the responses it already had into a new rollout; the
    // record's own id is what says one has been paid in already.
    if let Some(id) = payload.response_id {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        id.hash(&mut hasher);
        if !seen.insert(hasher.finish()) {
            return;
        }
        fold.seen.insert(hasher.finish());
    }

    let day = record
        .day()
        .or_else(|| local_day(now_ms()))
        .unwrap_or_default();
    let name = SessionAgent::Codex.display().to_string();
    let model = fold.model.clone().unwrap_or_else(|| name.clone());
    let price = usage.estimate(&model);
    let spent = Usage {
        tokens,
        cost: price.unwrap_or(0.),
        estimated: price.is_some(),
        unpriced: price.is_none(),
    };
    let fold_day = fold.days.entry(day).or_default();
    fold_day.usage.add(spent);
    fold_day.providers.entry(name).or_default().add(spent);
    fold_day.models.entry(model).or_default().add(spent);
}

/// A read-only connection to OpenCode's database. `None` when the agent has never
/// run here, or when the database is locked in a way SQLite's read-only mode
/// cannot recover from; both mean the same thing to a scan: nothing to read.
fn open_opencode(db: &Path) -> Option<rusqlite::Connection> {
    if !db.is_file() {
        return None;
    }
    let connection = rusqlite::Connection::open_with_flags(
        db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()?;
    // OpenCode writes while it runs; a short wait reads the committed rows
    // instead of reporting no usage for that sweep.
    let _ = connection.busy_timeout(std::time::Duration::from_secs(2));
    Some(connection)
}

/// The newest OpenCode sessions, as the history panel lists them, and how many
/// there are before the cap. OpenCode keeps one row per session in `session_v2`
/// and the turns in `session_message`, so both the row and its message count come
/// from the same database.
///
/// Only layouts that have `session_v2` are listed; an older OpenCode still pays
/// into the usage numbers through [`opencode_usage`], which reads messages.
fn opencode_sessions(db: &Path, limit: usize) -> (Vec<Session>, usize) {
    #[derive(Deserialize, Default)]
    struct Model {
        id: Option<String>,
        #[serde(rename = "providerID")]
        provider: Option<String>,
    }
    let Some(connection) = open_opencode(db) else {
        return (Vec::new(), 0);
    };
    let total: usize = connection
        .query_row(
            "select count(*) from session_v2 \
             where directory is not null and directory <> ''",
            [],
            |row| row.get::<_, i64>(0),
        )
        .unwrap_or(0)
        .max(0) as usize;
    if limit == 0 || total == 0 {
        return (Vec::new(), total);
    }
    let Ok(mut statement) = connection.prepare(
        "select id, directory, title, time_updated, model, \
                (select count(*) from session_message m \
                  where m.session_id = s.id and m.type in ('user', 'assistant')) \
         from session_v2 s \
         where directory is not null and directory <> '' \
         order by time_updated desc \
         limit ?1",
    ) else {
        return (Vec::new(), total);
    };
    let Ok(rows) = statement.query_map([limit as i64], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, i64>(3)?,
            row.get::<_, Option<String>>(4)?,
            row.get::<_, i64>(5)?,
        ))
    }) else {
        return (Vec::new(), total);
    };
    let sessions = rows
        .flatten()
        .filter_map(|(id, directory, title, updated, model, messages)| {
            let cwd = PathBuf::from(directory);
            if cwd.as_os_str().is_empty() {
                return None;
            }
            let workspace = cwd
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| cwd.display().to_string());
            // A session whose title OpenCode has not generated yet is named by
            // the first thing that was asked, as the file layouts are.
            let title = title
                .filter(|title| !title.trim().is_empty())
                .map(|title| one_line(&title, PREVIEW_CHARS))
                .or_else(|| {
                    first_user_text(&connection, &id).map(|text| one_line(&text, PREVIEW_CHARS))
                })
                .unwrap_or_else(|| "(no prompt recorded)".to_string());
            let model: Model = model
                .and_then(|model| serde_json::from_str(&model).ok())
                .unwrap_or_default();
            Some(Session {
                agent: SessionAgent::OpenCode,
                id,
                path: db.to_path_buf(),
                cwd,
                workspace,
                title,
                // The database has no per-row preview; the title and the meta
                // line are what the row draws.
                preview: String::new(),
                model: model.id,
                provider: model.provider,
                messages: messages.max(0) as u64,
                updated_ms: updated,
            })
        })
        .collect();
    (sessions, total)
}

/// The first thing the user asked in an OpenCode session, from its first user
/// message.
fn first_user_text(connection: &rusqlite::Connection, id: &str) -> Option<String> {
    let data: String = connection
        .query_row(
            "select data from session_message \
             where session_id = ?1 and type = 'user' \
             order by time_created limit 1",
            [id],
            |row| row.get(0),
        )
        .ok()?;
    let value: Value = serde_json::from_str(&data).ok()?;
    let text = value.get("text")?.as_str()?.trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// One readable OpenCode message: the user's text, or the assistant's text
/// blocks. Reasoning and tool traffic are not turns.
fn opencode_message_text(kind: &str, data: &str) -> Option<String> {
    let value: Value = serde_json::from_str(data).ok()?;
    let text = match kind {
        "user" => value.get("text")?.as_str()?.to_string(),
        // Reasoning and tool parts carry text too; only the answer is a turn.
        "assistant" => value
            .get("content")?
            .as_array()?
            .iter()
            .filter(|part| part.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(" "),
        _ => return None,
    };
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// OpenCode's newest readable turns, oldest first, for a handoff. Reads the
/// session's tail through the database rather than re-reading the whole file the
/// way the transcript layouts do.
fn opencode_recent_turns(db: &Path, id: &str, budget: usize) -> Vec<(String, String)> {
    let Some(connection) = open_opencode(db) else {
        return Vec::new();
    };
    let Ok(mut statement) = connection.prepare(
        "select type, data from session_message \
         where session_id = ?1 and type in ('user', 'assistant') \
         order by time_created desc limit 40",
    ) else {
        return Vec::new();
    };
    let Ok(rows) = statement.query_map([id], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    }) else {
        return Vec::new();
    };
    let mut messages: Vec<(String, String)> = rows
        .flatten()
        .filter_map(|(kind, data)| opencode_message_text(&kind, &data).map(|text| (kind, text)))
        .collect();
    messages.reverse();
    let mut turns: VecDeque<(String, String)> = VecDeque::new();
    let mut total = 0usize;
    for (role, text) in messages {
        let cut = one_line(&text, HANDOFF_TURN_CHARS);
        total += cut.chars().count() + role.chars().count() + 2;
        turns.push_back((role, cut));
        while total > budget && turns.len() > 1 {
            let (role, text) = turns.pop_front().expect("more than one turn");
            total -= text.chars().count() + role.chars().count() + 2;
        }
    }
    turns.into_iter().collect()
}

/// OpenCode's assistant turns since `since_ms`, from its SQLite database: when, and what
/// each cost. OpenCode records its own price, so nothing here is estimated.
///
/// The database is linked rather than read through a `sqlite3` binary: macOS ships
/// one, Linux does not, and a missing binary used to turn every OpenCode number
/// into no data at all. The connection is read-only, so OpenCode can keep writing.
fn opencode_usage(db: &Path, since_ms: i64) -> Vec<(i64, Spent)> {
    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct Turn {
        model: Model,
        cost: Option<f64>,
        tokens: Tokens,
    }
    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct Model {
        id: Option<String>,
        #[serde(rename = "providerID")]
        provider: Option<String>,
    }
    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct Tokens {
        input: u64,
        output: u64,
        reasoning: u64,
        cache: CacheTokens,
    }
    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct CacheTokens {
        read: u64,
        write: u64,
    }

    let Some(connection) = open_opencode(db) else {
        return Vec::new();
    };
    let Ok(mut statement) = connection.prepare(
        "select time_created, data from session_message \
         where type = 'assistant' and time_created >= ?1",
    ) else {
        return Vec::new();
    };
    let Ok(rows) = statement.query_map([since_ms], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
    }) else {
        return Vec::new();
    };
    let name = || "OpenCode".to_string();
    rows.flatten()
        .filter_map(|(time_created, data)| {
            let turn: Turn = serde_json::from_str(&data).ok()?;
            let tokens = &turn.tokens;
            let total = tokens.input
                + tokens.output
                + tokens.reasoning
                + tokens.cache.read
                + tokens.cache.write;
            let cost = turn.cost.filter(|cost| cost.is_finite() && *cost >= 0.);
            (total > 0).then(|| {
                (
                    time_created,
                    Spent {
                        tokens: total,
                        cost: cost.unwrap_or(0.),
                        estimated: false,
                        unpriced: cost.is_none(),
                        provider: turn.model.provider.unwrap_or_else(name),
                        model: turn.model.id.unwrap_or_else(name),
                    },
                )
            })
        })
        .collect()
}

/// Gemini CLI's model turns in the chats written since `since_ms`:
/// `<root>/<project>/chats/session-*.jsonl`, sub-agents' one directory further down.
/// Gemini records no price, so every turn is unpriced.
///
/// A message is appended again, whole, every time it changes, so the last copy of an
/// id is the one counted. shortcut: whole files re-read every scan, and the legacy
/// single-document `.json` chats are not read.
fn gemini_usage(root: &Path, since_ms: i64) -> Vec<(i64, Spent)> {
    #[derive(Deserialize)]
    struct Message {
        id: Option<String>,
        #[serde(rename = "type")]
        kind: Option<String>,
        timestamp: Option<String>,
        model: Option<String>,
        tokens: Option<Tokens>,
    }
    #[derive(Deserialize)]
    struct Tokens {
        input: Option<u64>,
        output: Option<u64>,
        thoughts: Option<u64>,
        tool: Option<u64>,
        total: Option<u64>,
    }

    let Ok(projects) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = projects
        .flatten()
        .map(|project| project.path().join("chats"))
        .collect();
    let mut turns = Vec::new();
    while let Some(dir) = dirs.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            if metadata.is_dir() {
                dirs.push(path);
                continue;
            }
            if path.extension().and_then(|extension| extension.to_str()) != Some("jsonl")
                || modified_ms(&metadata) < since_ms
            {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let mut last: HashMap<String, Message> = HashMap::new();
            for line in text.lines() {
                if let Ok(message) = serde_json::from_str::<Message>(line)
                    && message.kind.as_deref() == Some("gemini")
                    && let Some(id) = message.id.clone()
                {
                    last.insert(id, message);
                }
            }
            turns.extend(last.into_values().filter_map(|message| {
                let ms = DateTime::parse_from_rfc3339(message.timestamp.as_deref()?)
                    .ok()?
                    .timestamp_millis();
                let tokens = message.tokens?;
                // Cached tokens are inside `input`, as in the API's own `promptTokenCount`.
                let total = tokens.total.unwrap_or_else(|| {
                    tokens.input.unwrap_or(0)
                        + tokens.output.unwrap_or(0)
                        + tokens.thoughts.unwrap_or(0)
                        + tokens.tool.unwrap_or(0)
                });
                let name = "Gemini CLI".to_string();
                (total > 0 && ms >= since_ms).then(|| {
                    (
                        ms,
                        Spent {
                            tokens: total,
                            cost: 0.,
                            estimated: false,
                            unpriced: true,
                            model: message.model.unwrap_or_else(|| name.clone()),
                            provider: name,
                        },
                    )
                })
            }));
        }
    }
    turns
}

/// The text of a turn record, the only thing this module decodes a whole record for.
fn text_of(agent: SessionAgent, line: &str) -> Option<String> {
    if line.is_empty() {
        return None;
    }
    let value: Value = serde_json::from_str(line).ok()?;
    let content = match agent {
        SessionAgent::Pi | SessionAgent::Claude => value.get("message")?.get("content")?,
        SessionAgent::Codex => value.get("payload")?.get("content")?,
        SessionAgent::OpenCode => return None,
    };
    texts(content)
}

/// The text of a message's content: a bare string, or the array's text blocks.
fn texts(content: &Value) -> Option<String> {
    let joined = match content {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(" "),
        _ => return None,
    };
    let trimmed = joined.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// Whitespace collapsed to single spaces, cut to `limit` characters.
fn one_line(text: &str, limit: usize) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= limit {
        return collapsed;
    }
    let mut cut: String = collapsed.chars().take(limit.saturating_sub(1)).collect();
    cut.push('…');
    cut
}

/// Codex's envelope, as far as the count needs it: `payload` is where everything a
/// record says lives, and which fields are set says what kind of record it is.
#[derive(Debug, Deserialize)]
struct Payload<'a> {
    /// `session_meta`: the session's own id, and the directory it ran in.
    #[serde(rename = "session_id")]
    session_id: Option<&'a str>,
    id: Option<&'a str>,
    cwd: Option<&'a str>,
    /// `turn_context`: the model that answered the turn.
    model: Option<&'a str>,
    /// The inner discriminator: `message` for a turn, `token_count` for an event.
    #[serde(rename = "type")]
    kind: Option<&'a str>,
    role: Option<&'a str>,
    /// `token_usage_record`: which response this usage was for.
    #[serde(rename = "response_id")]
    response_id: Option<&'a str>,
    usage: Option<Tokens>,
    /// `event_msg` of type `token_count`.
    info: Option<Info>,
}

#[derive(Debug, Deserialize)]
struct Info {
    last_token_usage: Option<Tokens>,
}

/// One usage report. `cached_input_tokens` is *inside* `input_tokens` — `total_tokens`
/// is the input plus the output, which is what says so — so the cache is not added to
/// the input a second time.
#[derive(Debug, Deserialize)]
struct Tokens {
    input_tokens: Option<u64>,
    cached_input_tokens: Option<u64>,
    #[serde(alias = "cache_write_tokens")]
    cache_write_input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    total_tokens: Option<u64>,
}

impl Tokens {
    fn estimate(&self, model: &str) -> Option<f64> {
        let read = self.cached_input_tokens.unwrap_or(0);
        let write = self.cache_write_input_tokens.unwrap_or(0);
        // Both cache categories are inside Codex's input, unlike Pi's separate counts.
        let input = self
            .input_tokens
            .unwrap_or(0)
            .checked_sub(read.checked_add(write)?)?;
        crate::pricing::cost(model, input, self.output_tokens.unwrap_or(0), read, write)
    }

    /// The tokens this report covers, and the pair a later event is matched against.
    fn spent(&self) -> (u64, (u64, u64, u64)) {
        let input = self.input_tokens.unwrap_or(0);
        let output = self.output_tokens.unwrap_or(0);
        (
            input + output,
            (input, output, self.total_tokens.unwrap_or(0)),
        )
    }
}

/// One record, as far as the count needs it: every field optional and borrowed, so a line costs
/// a scan rather than a document.
#[derive(Debug, Deserialize)]
struct Record<'a> {
    #[serde(rename = "type")]
    kind: Option<&'a str>,
    #[serde(rename = "sessionId")]
    session_id: Option<&'a str>,
    provider: Option<&'a str>,
    #[serde(rename = "modelId")]
    model_id: Option<&'a str>,
    cwd: Option<&'a str>,
    id: Option<&'a str>,
    /// When the record was written, as either agent stamps it: RFC 3339.
    timestamp: Option<&'a str>,
    /// Claude Code's own price for the turn, when it writes one.
    #[serde(rename = "costUSD")]
    cost_usd: Option<f64>,
    message: Option<Message<'a>>,
    /// Codex's envelope around everything a rollout line says.
    payload: Option<Payload<'a>>,
}

impl Record<'_> {
    /// What one assistant turn cost: its tokens, the price the agent recorded, the
    /// provider that answered and the model that did. `None` for everything that is
    /// not a priced turn.
    fn spent(&self, agent: SessionAgent) -> Option<Spent> {
        let assistant = match agent {
            SessionAgent::Pi => self.kind == Some("message"),
            SessionAgent::Claude => self.kind == Some("assistant"),
            // Codex reports usage on its own record, not on the turn: see
            // [`Record::tokens`]. OpenCode's usage is read from SQLite.
            SessionAgent::Codex | SessionAgent::OpenCode => return None,
        };
        let message = self.message.as_ref()?;
        if !assistant || message.role != Some("assistant") {
            return None;
        }
        let usage = message.usage.as_ref()?;
        let (tokens, cost) = match agent {
            // Handled above: Codex reports usage on records of its own, and
            // OpenCode never reaches here.
            SessionAgent::Codex | SessionAgent::OpenCode => return None,
            SessionAgent::Pi => (
                usage.input.unwrap_or(0)
                    + usage.output.unwrap_or(0)
                    + usage.cache_read.unwrap_or(0)
                    + usage.cache_write.unwrap_or(0),
                usage
                    .cost
                    .as_ref()
                    .and_then(|cost| cost.total)
                    .filter(|cost| cost.is_finite() && *cost > 0.),
            ),
            SessionAgent::Claude => (
                usage.input_tokens.unwrap_or(0)
                    + usage.output_tokens.unwrap_or(0)
                    + usage.cache_read_input_tokens.unwrap_or(0)
                    + usage.cache_creation_input_tokens.unwrap_or(0),
                self.cost_usd.filter(|cost| cost.is_finite() && *cost >= 0.),
            ),
        };
        if tokens == 0 && cost.unwrap_or(0.) == 0. {
            return None;
        }
        let recorded = cost;
        let cost = recorded.or_else(|| match agent {
            SessionAgent::Pi => crate::pricing::cost(
                message.model?,
                usage.input.unwrap_or(0),
                usage.output.unwrap_or(0),
                usage.cache_read.unwrap_or(0),
                usage.cache_write.unwrap_or(0),
            ),
            // Claude Code no longer writes `costUSD`; its counts are the API's.
            SessionAgent::Claude => crate::pricing::cost(
                message.model?,
                usage.input_tokens.unwrap_or(0),
                usage.output_tokens.unwrap_or(0),
                usage.cache_read_input_tokens.unwrap_or(0),
                usage.cache_creation_input_tokens.unwrap_or(0),
            ),
            SessionAgent::Codex | SessionAgent::OpenCode => None,
        });
        // An agent that records neither is named by the agent itself (`Claude Code`),
        // so a record is never labelled with nothing.
        Some(Spent {
            tokens,
            cost: cost.unwrap_or(0.),
            estimated: recorded.is_none() && cost.is_some(),
            unpriced: cost.is_none(),
            provider: message
                .provider
                .map(str::to_string)
                .unwrap_or_else(|| agent.display().to_string()),
            model: message
                .model
                .map(str::to_string)
                .unwrap_or_else(|| agent.display().to_string()),
        })
    }

    /// The local day the record was written, as the key the chart sorts by.
    fn day(&self) -> Option<i32> {
        local_day(
            DateTime::parse_from_rfc3339(self.timestamp?)
                .ok()?
                .timestamp_millis(),
        )
    }
}

#[derive(Debug, Deserialize)]
struct Message<'a> {
    role: Option<&'a str>,
    model: Option<&'a str>,
    /// Pi names the provider that answered; Claude Code has only the one.
    provider: Option<&'a str>,
    usage: Option<UsageRecord>,
}

/// The tokens one turn cost, as either agent writes them: Pi in camelCase, Claude
/// Code in the API's snake_case. Every field optional, because a record that used
/// no tokens at all is one to skip rather than one to fault.
#[derive(Debug, Deserialize)]
struct UsageRecord {
    input: Option<u64>,
    output: Option<u64>,
    #[serde(rename = "cacheRead")]
    cache_read: Option<u64>,
    #[serde(rename = "cacheWrite")]
    cache_write: Option<u64>,
    cost: Option<CostRecord>,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
    cache_creation_input_tokens: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct CostRecord {
    total: Option<f64>,
}

/// Whether `session` belongs to the repository on screen: under any worktree of the active
/// worktree's repository. A session's `cwd` is a path *inside* a worktree, so containment is the
/// test, not equality.
pub fn in_active_repo(
    session: &Session,
    active: Option<&Worktree>,
    worktrees: &[Worktree],
) -> bool {
    active.is_some_and(|active| {
        worktrees
            .iter()
            .filter(|worktree| worktree.repo_id == active.repo_id)
            .any(|worktree| under(&session.cwd, &worktree.path))
    })
}

/// The worktree a session can be resumed in: the one it was working in, or the nearest one above it.
pub fn resume_target<'a>(session: &Session, worktrees: &'a [Worktree]) -> Option<&'a Worktree> {
    worktrees
        .iter()
        .filter(|worktree| under(&session.cwd, &worktree.path))
        .max_by_key(|worktree| worktree.path.as_os_str().len())
}

/// Whether `path` is `root` or inside it, by path components — `…/app` must not claim `…/app-secret`.
fn under(path: &Path, root: &Path) -> bool {
    path == root || path.starts_with(root)
}

/// How long ago something happened, as the row's age: `just now`, `5m ago`,
/// `2h ago`, `3d ago`, `4mo ago`.
pub fn relative_time(now_ms: i64, then_ms: i64) -> String {
    let seconds = (now_ms - then_ms).max(0) / 1000;
    match seconds {
        0..=59 => t!("session.just_now").to_string(),
        60..=3599 => t!("session.minutes_ago", n = seconds / 60).to_string(),
        3600..=86_399 => t!("session.hours_ago", n = seconds / 3600).to_string(),
        86_400..=2_592_000 => t!("session.days_ago", n = seconds / 86_400).to_string(),
        _ => t!("session.months_ago", n = seconds / 2_592_000).to_string(),
    }
}

/// Now, in epoch milliseconds.
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Repo, Worktree};
    use std::fs;
    use std::io::Write as _;

    /// A scratch home with the two roots, plus the transcripts written into it.
    fn home(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "syro-sessions-{tag}-{}-{}",
            std::process::id(),
            now_ms()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Pi's bucket name for a working directory — `session-manager.ts`'s own
    /// encoding: one dash per separator, wrapped in `--`.
    fn pi_bucket(cwd: &str) -> String {
        format!(
            "--{}--",
            cwd.trim_start_matches('/').replace(['/', ':'], "-")
        )
    }

    fn write_pi(home: &Path, cwd: &str, id: &str, lines: &[&str]) -> PathBuf {
        let dir = home.join(".pi/agent/sessions").join(pi_bucket(cwd));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("2026-10-12T01-00-01-191Z_{id}.jsonl"));
        let mut body = format!(
            "{{\"type\":\"session\",\"version\":3,\"id\":\"{id}\",\"timestamp\":\"2026-10-12T01:00:01.191Z\",\"cwd\":\"{cwd}\"}}\n"
        );
        for line in lines {
            body.push_str(line);
            body.push('\n');
        }
        fs::write(&path, body).unwrap();
        path
    }

    fn write_claude(home: &Path, cwd: &str, id: &str, lines: &[&str]) -> PathBuf {
        let dir = home.join(".claude/projects").join(cwd.replace('/', "-"));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{id}.jsonl"));
        let mut body = String::new();
        for line in lines {
            body.push_str(line);
            body.push('\n');
        }
        fs::write(&path, body).unwrap();
        path
    }

    fn pi_user(text: &str) -> String {
        format!(
            "{{\"type\":\"message\",\"id\":\"a\",\"parentId\":null,\"timestamp\":\"2026-10-12T01:00:02.000Z\",\"message\":{{\"role\":\"user\",\"content\":[{{\"type\":\"text\",\"text\":\"{text}\"}}]}}}}"
        )
    }

    fn pi_assistant(text: &str) -> String {
        format!(
            "{{\"type\":\"message\",\"id\":\"b\",\"parentId\":\"a\",\"timestamp\":\"2026-10-12T01:00:03.000Z\",\"message\":{{\"role\":\"assistant\",\"content\":[{{\"type\":\"text\",\"text\":\"{text}\"}}]}}}}"
        )
    }

    fn pi_tool() -> String {
        "{\"type\":\"message\",\"id\":\"c\",\"parentId\":\"b\",\"timestamp\":\"2026-10-12T01:00:04.000Z\",\"message\":{\"role\":\"toolResult\",\"content\":[{\"type\":\"text\",\"text\":\"tool output that is not a turn\"}]}}".to_string()
    }

    /// A Pi assistant turn with usage on it: what the usage strip reads, in the
    /// shape `session-manager.ts` writes.
    fn pi_spent(at: &str, provider: &str, input: u64, output: u64, cost: f64) -> String {
        serde_json::json!({
            "type": "message",
            "id": "u",
            "parentId": null,
            "timestamp": at,
            "message": {
                "role": "assistant",
                "content": [{"type": "text", "text": "ok"}],
                "api": "anthropic-messages",
                "provider": provider,
                "model": "claude-opus-5",
                "usage": {
                    "input": input,
                    "output": output,
                    "cacheRead": 0,
                    "cacheWrite": 0,
                    "totalTokens": input + output,
                    "cost": {"total": cost},
                },
            },
        })
        .to_string()
    }

    fn pi_gpt(at: &str, model: &str, input: u64, output: u64, read: u64, write: u64) -> String {
        serde_json::json!({
            "type": "message", "timestamp": at,
            "message": {
                "role": "assistant", "provider": "relay", "model": model,
                "usage": {
                    "input": input, "output": output, "cacheRead": read, "cacheWrite": write,
                    "reasoning": output / 2, "cost": {"total": 0},
                },
            },
        })
        .to_string()
    }

    #[test]
    fn pi_gpt_zero_or_missing_prices_use_official_rates() {
        for (model, expected) in [
            ("gpt-6-astra", 1.255),
            ("gpt-6.1-sol", 0.243),
            ("gpt-6-sol", 0.251),
            ("gpt-5.6-sol", 0.502),
        ] {
            let line = pi_gpt(
                "2026-10-16T00:00:00Z",
                model,
                100_000,
                1_000,
                80_000,
                10_000,
            );
            let record: Record<'_> = serde_json::from_str(&line).unwrap();
            let spent = record.spent(SessionAgent::Pi).unwrap();
            assert_eq!(spent.tokens, 191_000);
            assert!(spent.estimated);
            assert!(!spent.unpriced);
            assert!(
                (spent.cost - expected).abs() < 1e-10,
                "{model}: {}",
                spent.cost
            );

            let mut value: Value = serde_json::from_str(&line).unwrap();
            value["message"]["usage"]
                .as_object_mut()
                .unwrap()
                .remove("cost");
            let missing = value.to_string();
            let record: Record<'_> = serde_json::from_str(&missing).unwrap();
            assert!((record.spent(SessionAgent::Pi).unwrap().cost - expected).abs() < 1e-10);
            value["message"]["usage"]["cost"] = serde_json::json!({"total": 0.75});
            let priced = value.to_string();
            let record: Record<'_> = serde_json::from_str(&priced).unwrap();
            let spent = record.spent(SessionAgent::Pi).unwrap();
            assert_eq!(spent.cost, 0.75);
            assert!(!spent.estimated && !spent.unpriced);
        }
    }

    #[test]
    fn pi_gpt_long_context_reprices_the_whole_request() {
        for (input, expected) in [(172_000, 0.412), (172_001, 0.819004)] {
            // Include both cache categories in the 272K threshold, but not output.
            let line = pi_gpt(
                "2026-10-16T00:00:00Z",
                "gpt-6.1-sol",
                input,
                1_000,
                80_000,
                20_000,
            );
            let record: Record<'_> = serde_json::from_str(&line).unwrap();
            assert!((record.spent(SessionAgent::Pi).unwrap().cost - expected).abs() < 1e-10);
        }
    }

    #[test]
    fn unsupported_models_and_invalid_cache_counts_remain_unpriced() {
        let line = pi_gpt("2026-10-16T00:00:00Z", "gpt-6.1-sol-unknown", 100, 10, 0, 0);
        let record: Record<'_> = serde_json::from_str(&line).unwrap();
        let spent = record.spent(SessionAgent::Pi).unwrap();
        assert_eq!(spent.cost, 0.);
        assert!(spent.unpriced && !spent.estimated);
        let counts: Tokens = serde_json::from_value(serde_json::json!({
            "input_tokens": 190_000, "output_tokens": 1_000,
            "cached_input_tokens": 80_000, "cache_write_input_tokens": 10_000,
        }))
        .unwrap();
        assert!((counts.estimate("gpt-6.1-sol").unwrap() - 0.243).abs() < 1e-10);
        let invalid: Tokens = serde_json::from_value(serde_json::json!({
            "input_tokens": 100, "cached_input_tokens": 80, "cache_write_input_tokens": 30,
        }))
        .unwrap();
        assert_eq!(invalid.estimate("gpt-6.1-sol"), None);
    }

    #[test]
    fn codex_gpt_estimates_survive_model_changes_and_cached_scans() {
        let home = home("codex-estimate");
        let now = now_ms();
        let at = iso_days_ago(now, 0);
        let path = write_codex(
            &home,
            "estimate",
            &[
                &codex_meta("/Users/ada/code", "estimate"),
                &codex_turn("gpt-6.1-sol"),
                &codex_usage_record(&at, "estimate_a", 190_000, 80_000, 1_000),
                &codex_token_event(&at, 190_000, 80_000, 1_000),
            ],
        );
        let roots = SessionRoots::from_home(&home);
        let mut cache = Cache::default();
        let first = scan_at(&roots, &mut cache, DEFAULT_LIMIT, now).usage;
        // Cached tokens are inside Codex input; reasoning is inside output.
        assert!((first.total.cost - 0.238).abs() < 1e-10);
        assert_eq!(first.total.tokens, 191_000);
        assert!(first.total.estimated && !first.total.unpriced);
        assert!(first.models[0].estimated && !first.models[0].unpriced);
        assert!(first.days[0].models[0].estimated);
        assert_eq!(first, scan_at(&roots, &mut cache, DEFAULT_LIMIT, now).usage);
        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(file, "{}", codex_turn("gpt-6-sol")).unwrap();
        writeln!(
            file,
            "{}",
            codex_usage_record(&at, "estimate_b", 200_000, 80_000, 1_000)
        )
        .unwrap();
        let next = scan_at(&roots, &mut cache, DEFAULT_LIMIT, now).usage;
        assert!((next.total.cost - (0.238 + 0.266)).abs() < 1e-10);
        assert_eq!(next.total.tokens, 392_000);
        assert_eq!(next.models.len(), 2);
        assert_eq!(next.total.cost, next.today.cost);
        assert_eq!(next.total.cost, next.providers[0].cost);
        assert_eq!(next.total.cost, next.days[0].cost);
        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(file, "{}", codex_turn("gpt-unknown")).unwrap();
        writeln!(
            file,
            "{}",
            codex_usage_record(&at, "estimate_c", 100, 0, 10)
        )
        .unwrap();
        let mixed = scan_at(&roots, &mut cache, DEFAULT_LIMIT, now).usage;
        assert_eq!(mixed.total.cost, next.total.cost);
        assert!(mixed.total.estimated && mixed.total.unpriced);
        assert!(mixed.today.unpriced && mixed.providers[0].unpriced);
        let _ = fs::remove_dir_all(&home);
    }

    /// A Claude Code assistant turn with usage on it. `cost` is `None` for a
    /// transcript that records no price, which the agent often does not.
    fn claude_spent(at: &str, input: u64, output: u64, cost: Option<f64>) -> String {
        let mut record = serde_json::json!({
            "type": "assistant",
            "sessionId": "8f2c1d4e",
            "cwd": "/Users/ada/code/syro",
            "timestamp": at,
            "message": {
                "role": "assistant",
                "model": "claude-sonnet-4-5",
                "content": [{"type": "text", "text": "On it."}],
                "usage": {
                    "input_tokens": input,
                    "output_tokens": output,
                    "cache_read_input_tokens": 0,
                    "cache_creation_input_tokens": 0,
                },
            },
        });
        if let Some(cost) = cost {
            record["costUSD"] = serde_json::json!(cost);
        }
        record.to_string()
    }

    /// A timestamp `days` days before `now`, at the same moment of the day, as
    /// both agents stamp a record.
    fn iso_days_ago(now: i64, days: i64) -> String {
        DateTime::from_timestamp_millis(now - days * 86_400_000)
            .expect("a timestamp")
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
    }

    /// A Codex rollout, in the dated tree Codex writes rather than the one-bucket layout
    /// the other two agents use.
    fn write_codex(home: &Path, id: &str, lines: &[&str]) -> PathBuf {
        let dir = home.join(".codex/sessions/2026/10/15");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("rollout-2026-10-15T01-00-00-{id}.jsonl"));
        let mut body = String::new();
        for line in lines {
            body.push_str(line);
            body.push('\n');
        }
        fs::write(&path, body).unwrap();
        path
    }

    fn codex_meta(cwd: &str, id: &str) -> String {
        serde_json::json!({
            "timestamp": "2026-10-15T01:00:00.000Z",
            "type": "session_meta",
            "payload": {"session_id": id, "id": id, "cwd": cwd},
        })
        .to_string()
    }

    fn codex_turn(model: &str) -> String {
        serde_json::json!({
            "timestamp": "2026-10-15T01:00:01.000Z",
            "type": "turn_context",
            "payload": {"model": model, "cwd": "/Users/ada/code"},
        })
        .to_string()
    }

    fn codex_message(at: &str, role: &str, text: &str) -> String {
        serde_json::json!({
            "timestamp": at,
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": role,
                "content": [{"type": "input_text", "text": text}],
            },
        })
        .to_string()
    }

    /// A per-response usage line: `usage` is the response's own, `turn_token_usage` a
    /// running total that must not be added as well.
    fn codex_usage_record(
        at: &str,
        response_id: &str,
        input: u64,
        cached: u64,
        output: u64,
    ) -> String {
        serde_json::json!({
            "timestamp": at,
            "type": "token_usage_record",
            "payload": {
                "response_id": response_id,
                "usage": {
                    "input_tokens": input,
                    "cached_input_tokens": cached,
                    "output_tokens": output,
                    "total_tokens": input + output,
                },
                "turn_token_usage": {
                    "input_tokens": input * 9,
                    "cached_input_tokens": cached,
                    "output_tokens": output * 9,
                    "total_tokens": (input + output) * 9,
                },
            },
        })
        .to_string()
    }

    /// The same numbers as a `token_count` event, which older rollouts have instead.
    fn codex_token_event(at: &str, input: u64, cached: u64, output: u64) -> String {
        serde_json::json!({
            "timestamp": at,
            "type": "event_msg",
            "payload": {
                "type": "token_count",
                "info": {"last_token_usage": {
                    "input_tokens": input,
                    "cached_input_tokens": cached,
                    "output_tokens": output,
                    "total_tokens": input + output,
                }},
            },
        })
        .to_string()
    }

    fn scan_home(home: &Path) -> Scan {
        scan(
            &SessionRoots::from_home(home),
            &mut Cache::default(),
            DEFAULT_LIMIT,
        )
    }

    #[test]
    fn a_pi_transcript_summarises_to_title_preview_and_turns() {
        let home = home("pi-basic");
        write_pi(
            &home,
            "/Users/ada/syro/workspaces/swap-aggregator/main-6",
            "01a09f17",
            &[
                "{\"type\":\"model_change\",\"id\":\"m\",\"provider\":\"anthropic\",\"modelId\":\"claude-opus-5\"}",
                &pi_user("查看 okx-mode.md，看看还有什么没做"),
                &pi_assistant("Reading the file first."),
                &pi_tool(),
            ],
        );

        let scan = scan_home(&home);
        assert_eq!(scan.total, 1);
        let session = &scan.sessions[0];
        assert_eq!(session.agent, SessionAgent::Pi);
        assert_eq!(session.id, "01a09f17");
        assert_eq!(session.workspace, "main-6");
        assert_eq!(session.title, "查看 okx-mode.md，看看还有什么没做");
        assert_eq!(session.preview, "Reading the file first.");
        assert_eq!(session.model.as_deref(), Some("claude-opus-5"));
        assert_eq!(session.provider.as_deref(), Some("anthropic"));
        // Two turns: the tool result is not one.
        assert_eq!(session.messages, 2);
        assert!(session.updated_ms > 0);
        let _ = fs::remove_dir_all(&home);
    }

    /// A Codex rollout folds into the same summary the other two agents' do.
    #[test]
    fn a_codex_rollout_summarises_to_title_preview_and_turns() {
        let home = home("codex-basic");
        let now = now_ms();
        let at = iso_days_ago(now, 0);
        write_codex(
            &home,
            "01a2b3c4",
            &[
                &codex_meta("/Users/ada/code/swap-aggregator", "01a2b3c4"),
                &codex_turn("gpt-5-codex"),
                &codex_message(&at, "user", "把 swap 的重试改成退避"),
                &codex_message(&at, "assistant", "On it."),
            ],
        );

        let scan = scan_home(&home);
        assert_eq!(scan.total, 1);
        let session = &scan.sessions[0];
        assert_eq!(session.agent, SessionAgent::Codex);
        assert_eq!(session.id, "01a2b3c4");
        assert_eq!(session.workspace, "swap-aggregator");
        assert_eq!(session.title, "把 swap 的重试改成退避");
        assert_eq!(session.preview, "On it.");
        assert_eq!(session.model.as_deref(), Some("gpt-5-codex"));
        // Two turns: the usage records are not turns.
        assert_eq!(session.messages, 2);
        let _ = fs::remove_dir_all(&home);
    }

    /// A newer rollout reports each response twice — a record and an event saying the same
    /// thing — and only one of them is counted. An older one has only the event. And the
    /// cached tokens are inside the input, not beside it: adding them would double the
    /// figure.
    #[test]
    fn codex_counts_a_response_once_and_cached_tokens_once() {
        let home = home("codex-usage");
        let now = now_ms();
        write_codex(
            &home,
            "01a2b3c4",
            &[
                &codex_meta("/Users/ada/code/swap-aggregator", "01a2b3c4"),
                &codex_turn("gpt-5-codex"),
                // 28_739 input of which 28_032 cached, 11 output: 28_750 tokens, not
                // 56_782.
                &codex_usage_record(&iso_days_ago(now, 0), "resp_a", 28_739, 28_032, 11),
                &codex_token_event(&iso_days_ago(now, 0), 28_739, 28_032, 11),
                // Only an older rollout would have this response, as an event alone.
                &codex_token_event(&iso_days_ago(now, 1), 1_000, 0, 500),
            ],
        );

        let usage = scan_at(
            &SessionRoots::from_home(&home),
            &mut Cache::default(),
            DEFAULT_LIMIT,
            now,
        )
        .usage;

        assert_eq!(usage.total.tokens, 28_750 + 1_500);
        assert_eq!(usage.today.tokens, 28_750);
        assert_eq!(usage.days.len(), 2);
        // No matching rate in our snapshot: unknown, not a free subscription.
        assert_eq!(usage.total.cost, 0.);
        assert!(usage.total.unpriced);
        assert!(!usage.total.estimated);
        assert_eq!(usage.providers[0].label, "Codex");
        assert_eq!(usage.models[0].label, "gpt-5-codex");
        assert_eq!(usage.models[0].tokens, 30_250);
        let _ = fs::remove_dir_all(&home);
    }

    /// Resuming a Codex session writes a second rollout with the earlier responses replayed
    /// into it: the response id is what keeps them from being counted twice.
    #[test]
    fn a_replayed_codex_response_is_counted_once() {
        let home = home("codex-resume");
        let now = now_ms();
        let at = iso_days_ago(now, 0);
        write_codex(
            &home,
            "01a2b3c4",
            &[
                &codex_meta("/Users/ada/code", "01a2b3c4"),
                &codex_turn("gpt-5-codex"),
                &codex_usage_record(&at, "resp_a", 100, 0, 10),
            ],
        );
        write_codex(
            &home,
            "01a2b3c5",
            &[
                &codex_meta("/Users/ada/code", "01a2b3c5"),
                &codex_turn("gpt-5-codex"),
                // The response the resume carried over, and the one it added.
                &codex_usage_record(&at, "resp_a", 100, 0, 10),
                &codex_usage_record(&at, "resp_b", 200, 0, 20),
            ],
        );

        let usage = scan_at(
            &SessionRoots::from_home(&home),
            &mut Cache::default(),
            DEFAULT_LIMIT,
            now,
        )
        .usage;
        assert_eq!(usage.total.tokens, 110 + 220);
        let _ = fs::remove_dir_all(&home);
    }

    /// A Codex id finds its rollout in the dated tree.
    #[test]
    fn a_codex_id_finds_its_rollout() {
        let home = home("codex-by-id");
        let roots = SessionRoots::from_home(&home);
        let rollout = write_codex(
            &home,
            "01a2b3c4",
            &[
                &codex_meta("/Users/ada/code", "01a2b3c4"),
                &codex_message("2026-10-15T01:00:02.000Z", "user", "fix the build"),
            ],
        );
        write_codex(
            &home,
            "01a2b3c5",
            &[&codex_meta("/Users/ada/code", "01a2b3c5")],
        );

        assert_eq!(
            transcript_of(
                &roots,
                SessionAgent::Codex,
                Path::new("/Users/ada/code"),
                "01a2b3c4"
            ),
            Some(rollout.clone())
        );
        assert_eq!(
            first_prompt(SessionAgent::Codex, "01a2b3c4", &rollout).as_deref(),
            Some("fix the build")
        );
        let _ = fs::remove_dir_all(&home);
    }

    /// A handoff carries the task and the newest turns, in order, and leaves the
    /// tool traffic behind.
    #[test]
    fn a_handoff_carries_the_task_and_the_newest_turns() {
        let home = home("handoff-pi");
        let path = write_pi(
            &home,
            "/Users/ada/code/syro",
            "hoff",
            &[
                &pi_user("fix the flaky test"),
                &pi_assistant("On it."),
                &pi_tool(),
                &pi_user("it still fails"),
                &pi_assistant("Looking again."),
            ],
        );
        let text = handoff(SessionAgent::Pi, "hoff", &path).expect("a handoff");
        assert!(
            text.contains("Original request: fix the flaky test"),
            "{text}"
        );
        assert!(text.contains("assistant: Looking again."), "{text}");
        // Oldest first: the newest turn is last, and the original request is not
        // repeated as a turn before its own place in the tail.
        let earlier = text.find("user: fix the flaky test").expect("a turn");
        let later = text.find("user: it still fails").expect("a newer turn");
        assert!(earlier < later, "{text}");
        assert!(!text.contains("tool output that is not a turn"), "{text}");
        let _ = fs::remove_dir_all(&home);
    }

    /// The same read against Claude's and Codex's records, whose role is the
    /// record type rather than a field inside the message.
    #[test]
    fn a_handoff_reads_claude_and_codex_turns() {
        let home = home("handoff-agents");
        let claude = write_claude(
            &home,
            "--x--",
            "8f2c",
            &[
                "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"ship it\"}}",
                "{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"Doing it.\"}]}}",
                "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"tool_result\",\"content\":\"noise\"}]}}",
            ],
        );
        let text = handoff(SessionAgent::Claude, "8f2c", &claude).expect("a Claude handoff");
        assert!(text.contains("Original request: ship it"), "{text}");
        assert!(text.contains("assistant: Doing it."), "{text}");
        assert!(!text.contains("noise"), "{text}");

        let codex = write_codex(
            &home,
            "01a2b3c4",
            &[
                &codex_meta("/Users/ada/code", "01a2b3c4"),
                &codex_message("2026-10-15T01:00:02.000Z", "user", "fix the build"),
                &codex_message("2026-10-15T01:00:03.000Z", "assistant", "On it."),
            ],
        );
        let text = handoff(SessionAgent::Codex, "01a2b3c4", &codex).expect("a Codex handoff");
        assert!(text.contains("Original request: fix the build"), "{text}");
        assert!(text.contains("assistant: On it."), "{text}");
        let _ = fs::remove_dir_all(&home);
    }

    /// The strip's numbers are the priced turns, each under the day it happened:
    /// both cuts of the chart are the same turns, read two ways.
    #[test]
    fn usage_is_summed_by_day_and_by_provider() {
        let home = home("usage-pi");
        let now = now_ms();
        write_pi(
            &home,
            "/Users/ada/code/syro",
            "u1",
            &[
                // Outside the window: still a session, not in the numbers.
                &pi_spent(&iso_days_ago(now, 20), "anthropic", 999, 0, 9.),
                &pi_spent(&iso_days_ago(now, 3), "iotex", 200, 0, 1.),
                &pi_spent(&iso_days_ago(now, 0), "anthropic", 40, 60, 0.5),
            ],
        );

        let scan = scan_at(
            &SessionRoots::from_home(&home),
            &mut Cache::default(),
            DEFAULT_LIMIT,
            now,
        );

        // The transcript itself is listed whole, however old its turns are.
        assert_eq!(scan.sessions[0].messages, 3);
        assert_eq!(scan.usage.total.tokens, 300);
        assert_eq!(scan.usage.total.cost, 1.5);
        assert_eq!(scan.usage.today.tokens, 100);
        // One bar per day that spent something, oldest first.
        let days: Vec<u64> = scan.usage.days.iter().map(|day| day.tokens).collect();
        assert_eq!(days, [200, 100]);
        // And the second cut, biggest first.
        let groups: Vec<(&str, u64, f64)> = scan
            .usage
            .providers
            .iter()
            .map(|group| (group.label.as_str(), group.tokens, group.cost))
            .collect();
        assert_eq!(groups, [("iotex", 200, 1.), ("anthropic", 100, 0.5)]);
        let _ = fs::remove_dir_all(&home);
    }

    /// Claude Code's turns count under its own name, and a transcript that records
    /// no price is a day of tokens rather than a day of nothing.
    #[test]
    fn claude_code_usage_is_counted_under_its_own_name() {
        let home = home("usage-claude");
        let now = now_ms();
        write_claude(
            &home,
            "/Users/ada/code/syro",
            "8f2c1d4e",
            &[
                &claude_spent(&iso_days_ago(now, 0), 10, 90, Some(0.25)),
                &claude_spent(&iso_days_ago(now, 1), 30, 20, None),
            ],
        );

        let usage = scan_at(
            &SessionRoots::from_home(&home),
            &mut Cache::default(),
            DEFAULT_LIMIT,
            now,
        )
        .usage;

        assert_eq!(usage.total.tokens, 150);
        assert_eq!(usage.total.cost, 0.25);
        assert_eq!(usage.days.len(), 2);
        assert_eq!(usage.days[0].tokens, 50);
        assert_eq!(usage.days[1].tokens, 100);
        assert_eq!(usage.providers[0].label, "Claude Code");
        assert_eq!(usage.providers[0].tokens, 150);
        // And the same turns cut by model: both days answered by the same one.
        assert_eq!(usage.models.len(), 1);
        assert_eq!(usage.models[0].label, "claude-sonnet-4-5");
        assert_eq!(usage.models[0].tokens, 150);
        assert_eq!(usage.days[1].models[0].model, "claude-sonnet-4-5");
        assert_eq!(usage.days[1].models[0].tokens, 100);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn a_claude_transcript_summarises_the_same_way() {
        let home = home("claude-basic");
        write_claude(
            &home,
            "/Users/ada/code/syro",
            "8f2c1d4e",
            &[
                "{\"type\":\"user\",\"sessionId\":\"8f2c1d4e\",\"cwd\":\"/Users/ada/code/syro\",\"timestamp\":\"2026-10-12T02:00:00.000Z\",\"message\":{\"role\":\"user\",\"content\":\"fix the flaky test\"}}",
                "{\"type\":\"assistant\",\"sessionId\":\"8f2c1d4e\",\"cwd\":\"/Users/ada/code/syro\",\"message\":{\"role\":\"assistant\",\"model\":\"claude-sonnet-4-5\",\"content\":[{\"type\":\"text\",\"text\":\"On it.\"},{\"type\":\"tool_use\",\"name\":\"Bash\"}]}}",
                // A tool result is written as a `user` record with no text in it:
                // it is neither a turn nor a title.
                "{\"type\":\"user\",\"sessionId\":\"8f2c1d4e\",\"cwd\":\"/Users/ada/code/syro\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"tool_result\",\"content\":\"ok\"}]}}",
            ],
        );

        let session = scan_home(&home).sessions.remove(0);
        assert_eq!(session.agent, SessionAgent::Claude);
        assert_eq!(session.id, "8f2c1d4e");
        assert_eq!(session.workspace, "syro");
        assert_eq!(session.title, "fix the flaky test");
        assert_eq!(session.preview, "On it.");
        assert_eq!(session.model.as_deref(), Some("claude-sonnet-4-5"));
        // The tool result counts as a turn for Claude: it is a `user` record and every one of them counts.
        assert_eq!(session.messages, 3);
        let _ = fs::remove_dir_all(&home);
    }

    /// A terminal that started a session under an id finds that transcript and its first prompt.
    #[test]
    fn a_session_id_finds_its_transcript_and_first_prompt() {
        let home = home("by-id");
        let roots = SessionRoots::from_home(&home);
        let cwd = "/Users/ada/code/syro";
        let pi = write_pi(
            &home,
            cwd,
            "0a1b",
            &[&pi_assistant("hi"), &pi_user("fix the build")],
        );
        write_pi(&home, cwd, "other", &[&pi_user("not this one")]);
        assert_eq!(
            transcript_of(&roots, SessionAgent::Pi, Path::new(cwd), "0a1b"),
            Some(pi.clone())
        );
        assert_eq!(
            first_prompt(SessionAgent::Pi, "0a1b", &pi).as_deref(),
            Some("fix the build")
        );
        let claude = write_claude(
            &home,
            cwd,
            "8f2c",
            &["{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"why is it red\"}}"],
        );
        assert_eq!(
            transcript_of(
                &roots,
                SessionAgent::Claude,
                Path::new("/elsewhere"),
                "8f2c"
            ),
            Some(claude.clone())
        );
        assert_eq!(
            first_prompt(SessionAgent::Claude, "8f2c", &claude).as_deref(),
            Some("why is it red")
        );
        // Not written yet: nothing to find.
        assert_eq!(
            transcript_of(&roots, SessionAgent::Pi, Path::new(cwd), "later"),
            None
        );
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn a_second_scan_reads_only_what_was_appended() {
        let home = home("incremental");
        let path = write_pi(
            &home,
            "/Users/ada/code/syro",
            "inc",
            &[&pi_user("first"), &pi_assistant("answer")],
        );
        let roots = SessionRoots::from_home(&home);
        let mut cache = Cache::default();

        let first = scan(&roots, &mut cache, DEFAULT_LIMIT);
        assert_eq!(first.sessions[0].messages, 2);
        let consumed = cache.entries[&path].fold.bytes;
        assert_eq!(consumed, fs::metadata(&path).unwrap().len());

        fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(format!("{}\n", pi_user("second")).as_bytes())
            .unwrap();

        let second = scan(&roots, &mut cache, DEFAULT_LIMIT);
        assert_eq!(
            second.sessions[0].messages, 3,
            "the new turn was not counted"
        );
        // The title is from the first pass and the preview from the second.
        assert_eq!(second.sessions[0].title, "first");
        assert_eq!(second.sessions[0].preview, "second");
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn a_truncated_transcript_is_read_again() {
        let home = home("truncated");
        let path = write_pi(
            &home,
            "/Users/ada/code/syro",
            "tr",
            &[&pi_user("one"), &pi_assistant("two"), &pi_user("three")],
        );
        let roots = SessionRoots::from_home(&home);
        let mut cache = Cache::default();
        assert_eq!(
            scan(&roots, &mut cache, DEFAULT_LIMIT).sessions[0].messages,
            3
        );

        // Rewritten shorter, as a rotated or replaced transcript is.
        write_pi(&home, "/Users/ada/code/syro", "tr", &[&pi_user("only")]);
        assert!(
            fs::metadata(&path).unwrap().len() < cache.entries[&path].size,
            "the fixture did not shrink, so this test proves nothing"
        );
        let again = scan(&roots, &mut cache, DEFAULT_LIMIT);
        assert_eq!(again.sessions[0].messages, 1);
        assert_eq!(again.sessions[0].title, "only");
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn the_cap_keeps_the_newest_and_the_total_says_what_was_cut() {
        let home = home("cap");
        for (index, name) in ["oldest", "middle", "newest"].iter().enumerate() {
            let path = write_pi(&home, "/Users/ada/code/syro", name, &[&pi_user("hi")]);
            let when =
                SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000 + index as u64);
            fs::File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_times(fs::FileTimes::new().set_modified(when))
                .unwrap();
        }

        let scan = scan(&SessionRoots::from_home(&home), &mut Cache::default(), 2);
        assert_eq!(scan.total, 3);
        assert_eq!(scan.sessions.len(), 2);
        assert_eq!(scan.sessions[0].workspace, "syro");
        let ids: Vec<&str> = scan.sessions.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["newest", "middle"]);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn a_transcript_with_no_directory_is_not_listed() {
        let home = home("no-cwd");
        // A `session` record with no cwd: there is nowhere to put this row.
        let dir = home.join(".pi/agent/sessions/--x--");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("bare.jsonl"),
            "{\"type\":\"session\",\"id\":\"bare\"}\n",
        )
        .unwrap();
        assert_eq!(scan_home(&home).total, 1);
        assert!(scan_home(&home).sessions.is_empty());
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn scope_asks_the_active_worktree_and_its_siblings() {
        let repo = Repo {
            id: "/Users/ada/code/syro".into(),
            name: "syro".into(),
            path: "/Users/ada/code/syro".into(),
            shared_paths: Vec::new(),
        };
        let worktrees = vec![
            Worktree {
                id: "/Users/ada/code/syro".into(),
                repo_id: repo.id.clone(),
                name: "syro".into(),
                path: "/Users/ada/code/syro".into(),
                branch: "main".into(),
                base_ref: "main".into(),
            },
            Worktree {
                id: "/Users/ada/syro/workspaces/syro/feature".into(),
                repo_id: repo.id.clone(),
                name: "feature".into(),
                path: "/Users/ada/syro/workspaces/syro/feature".into(),
                branch: "feature".into(),
                base_ref: "main".into(),
            },
            Worktree {
                id: "/Users/ada/code/other".into(),
                repo_id: "/Users/ada/code/other".into(),
                name: "other".into(),
                path: "/Users/ada/code/other".into(),
                branch: "main".into(),
                base_ref: "main".into(),
            },
        ];
        let session = |cwd: &str| Session {
            agent: SessionAgent::Pi,
            id: "s".into(),
            path: PathBuf::from("/tmp/s.jsonl"),
            cwd: PathBuf::from(cwd),
            workspace: "w".into(),
            title: "t".into(),
            preview: String::new(),
            model: None,
            provider: None,
            messages: 1,
            updated_ms: 0,
        };

        let inside = session("/Users/ada/syro/workspaces/syro/feature/packages/app");
        let sibling = session("/Users/ada/code/syro");
        let elsewhere = session("/Users/ada/code/other");

        let repo = |session| in_active_repo(session, Some(&worktrees[1]), &worktrees);
        assert!(repo(&inside));
        assert!(
            repo(&sibling),
            "a sibling worktree of the same repository belongs"
        );
        assert!(!repo(&elsewhere));
        // No active worktree means nothing belongs.
        assert!(!in_active_repo(&inside, None, &worktrees));

        assert_eq!(
            resume_target(&inside, &worktrees).map(|worktree| worktree.name.as_str()),
            Some("feature"),
            "a session under a worktree resumes in that worktree"
        );
        assert_eq!(
            resume_target(&session("/Users/ada/code/other-adjacent"), &worktrees).map(|w| &w.name),
            None,
            "a sibling prefix is not inside the worktree"
        );
    }

    #[test]
    fn ages_read_the_way_the_rows_say_them() {
        let now = 1_000_000_000_000;
        assert_eq!(relative_time(now, now), "just now");
        assert_eq!(relative_time(now, now - 59_000), "just now");
        assert_eq!(relative_time(now, now - 60_000), "1m ago");
        assert_eq!(relative_time(now, now - 2 * 3_600_000), "2h ago");
        assert_eq!(relative_time(now, now - 3 * 86_400_000), "3d ago");
        assert_eq!(relative_time(now, now - 90 * 86_400_000), "3mo ago");
        // A clock that went backwards is not "in the future".
        assert_eq!(relative_time(now, now + 5_000), "just now");
    }

    #[test]
    fn long_text_is_one_line_and_bounded() {
        assert_eq!(one_line("a\n  b\tc ", 100), "a b c");
        let cut = one_line(&"x".repeat(500), 10);
        assert_eq!(cut.chars().count(), 10);
        assert!(cut.ends_with('…'));
    }

    #[test]
    fn gemini_and_opencode_turns_are_counted() {
        let home = home("others");
        let now = now_ms();
        let at = iso_days_ago(now, 0);
        let old = iso_days_ago(now, 30);
        let chats = home.join(".gemini/tmp/proj/chats");
        fs::create_dir_all(chats.join("parent")).unwrap();
        fs::write(
            chats.join("session-a.jsonl"),
            [
                r#"{"sessionId":"s","projectHash":"h"}"#.to_string(),
                format!(r#"{{"id":"u","timestamp":"{at}","type":"user","content":"hi"}}"#),
                // Appended again once its tokens are in: only the last copy counts.
                format!(r#"{{"id":"g","timestamp":"{at}","type":"gemini","model":"gemini-3-pro"}}"#),
                format!(r#"{{"id":"g","timestamp":"{at}","type":"gemini","model":"gemini-3-pro","tokens":{{"input":100,"output":20,"cached":50,"thoughts":5,"tool":0,"total":125}}}}"#),
                format!(r#"{{"id":"o","timestamp":"{old}","type":"gemini","tokens":{{"total":999}}}}"#),
                r#"{"$set":{"lastUpdated":"x"}}"#.to_string(),
            ]
            .join("\n"),
        )
        .unwrap();
        fs::write(
            chats.join("parent/sub.jsonl"),
            format!(r#"{{"id":"s","timestamp":"{at}","type":"gemini","model":"gemini-3-flash","tokens":{{"input":10,"output":5}}}}"#),
        )
        .unwrap();

        let db = home.join(".local/share/opencode/opencode.db");
        fs::create_dir_all(db.parent().unwrap()).unwrap();
        let data = r#"{"model":{"id":"deepseek","providerID":"opencode"},"cost":0.5,"tokens":{"input":100,"output":10,"reasoning":5,"cache":{"read":80,"write":0}}}"#;
        let connection = rusqlite::Connection::open(&db).unwrap();
        connection
            .execute_batch(
                "create table session_message (type text, time_created integer, data text);",
            )
            .unwrap();
        for (kind, time) in [
            ("assistant", now),
            ("user", now),
            ("assistant", now - 30 * 86_400_000),
        ] {
            connection
                .execute(
                    "insert into session_message values (?1, ?2, ?3)",
                    rusqlite::params![kind, time, data],
                )
                .unwrap();
        }
        drop(connection);

        let roots = SessionRoots::from_home(&home);
        let usage = scan_at(&roots, &mut Cache::default(), DEFAULT_LIMIT, now).usage;
        let tokens = |label: &str| {
            usage
                .providers
                .iter()
                .find(|group| group.label == label)
                .map(|group| (group.tokens, group.cost, group.unpriced))
        };
        assert_eq!(tokens("Gemini CLI"), Some((140, 0., true)));
        assert_eq!(tokens("opencode"), Some((195, 0.5, false)));
        assert_eq!(usage.total.tokens, 335);
        assert!(
            usage
                .models
                .iter()
                .any(|group| group.label == "gemini-3-flash")
        );
        let _ = fs::remove_dir_all(&home);
    }

    /// OpenCode's history is listed from `session_v2`, with the first user
    /// message standing in for a title OpenCode has not generated yet. It is
    /// also found by id, which is how a restored tab re-reads its prompt.
    #[test]
    fn opencode_sessions_are_listed_from_the_database() {
        let home = home("opencode-sessions");
        let db = home.join(".local/share/opencode/opencode.db");
        fs::create_dir_all(db.parent().unwrap()).unwrap();
        let now = now_ms();
        let connection = rusqlite::Connection::open(&db).unwrap();
        connection
            .execute_batch(
                "create table session_v2 (
                    id text primary key,
                    directory text,
                    title text,
                    time_updated integer,
                    model text
                );
                create table session_message (
                    id text primary key,
                    session_id text,
                    type text,
                    time_created integer,
                    data text,
                    foreign key (session_id) references session_v2(id) on delete cascade
                );",
            )
            .unwrap();
        connection
            .execute(
                "insert into session_v2 values (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![
                    "ses_one",
                    "/Users/ada/code/syro",
                    "fix the build",
                    now,
                    r#"{"id":"gpt-5","providerID":"openai"}"#
                ],
            )
            .unwrap();
        for (id, kind, at, data) in [
            (
                "m1",
                "user",
                now - 2_000,
                r#"{"text":"fix the build"}"#.to_string(),
            ),
            (
                "m2",
                "assistant",
                now - 1_000,
                r#"{"content":[{"type":"text","text":"On it."}]}"#.to_string(),
            ),
        ] {
            connection
                .execute(
                    "insert into session_message values (?1, 'ses_one', ?2, ?3, ?4)",
                    rusqlite::params![id, kind, at, data],
                )
                .unwrap();
        }
        connection
            .execute(
                "insert into session_v2 values ('ses_two', '/Users/ada/code/syro', null, ?1, null)",
                [now - 5_000],
            )
            .unwrap();
        connection
            .execute(
                "insert into session_message values ('m3', 'ses_two', 'user', ?1, ?2)",
                rusqlite::params![now - 5_000, r#"{"text":"why is it red"}"#],
            )
            .unwrap();
        drop(connection);

        let roots = SessionRoots::from_home(&home);
        let scan = scan_at(&roots, &mut Cache::default(), DEFAULT_LIMIT, now);
        assert_eq!(scan.total, 2);
        assert_eq!(scan.sessions.len(), 2);
        let session = scan
            .sessions
            .iter()
            .find(|session| session.id == "ses_one")
            .expect("the titled session");
        assert_eq!(session.agent, SessionAgent::OpenCode);
        assert_eq!(session.title, "fix the build");
        assert_eq!(session.cwd, Path::new("/Users/ada/code/syro"));
        assert_eq!(session.workspace, "syro");
        assert_eq!(session.messages, 2);
        assert_eq!(session.model.as_deref(), Some("gpt-5"));
        assert_eq!(session.provider.as_deref(), Some("openai"));
        assert_eq!(session.path, db);
        assert_eq!(
            scan.sessions
                .iter()
                .find(|session| session.id == "ses_two")
                .expect("the untitled session")
                .title,
            "why is it red"
        );
        assert_eq!(
            transcript_of(&roots, SessionAgent::OpenCode, Path::new(""), "ses_one"),
            Some(db.clone())
        );
        assert_eq!(
            first_prompt(SessionAgent::OpenCode, "ses_two", &db).as_deref(),
            Some("why is it red")
        );
        let _ = fs::remove_dir_all(&home);
    }

    /// A handoff reads OpenCode's own turns, and deleting an OpenCode session
    /// removes its row and — through the same foreign key OpenCode declares — its
    /// messages, rather than unlinking a file that is the whole database.
    #[test]
    fn an_opencode_handoff_and_delete_read_the_database() {
        let home = home("opencode-handoff-delete");
        let db = home.join(".local/share/opencode/opencode.db");
        fs::create_dir_all(db.parent().unwrap()).unwrap();
        let now = now_ms();
        let connection = rusqlite::Connection::open(&db).unwrap();
        connection
            .execute_batch(
                "pragma foreign_keys = on;
                 create table session_v2 (
                    id text primary key,
                    directory text,
                    title text,
                    time_updated integer,
                    model text
                 );
                 create table session_message (
                    id text primary key,
                    session_id text,
                    type text,
                    time_created integer,
                    data text,
                    foreign key (session_id) references session_v2(id) on delete cascade
                 );
                 insert into session_v2 values ('ses_one', '/Users/ada/code/syro', 'ship it', 1, null);",
            )
            .unwrap();
        for (id, kind, at, data) in [
            (
                "m1",
                "user",
                now - 4_000,
                r#"{"text":"ship it"}"#.to_string(),
            ),
            (
                "m2",
                "assistant",
                now - 3_000,
                r#"{"content":[{"type":"reasoning","text":"thinking"},{"type":"text","text":"Doing it."}]}"#
                    .to_string(),
            ),
            (
                "m3",
                "user",
                now - 2_000,
                r#"{"text":"and the tests"}"#.to_string(),
            ),
        ] {
            connection
                .execute(
                    "insert into session_message values (?1, 'ses_one', ?2, ?3, ?4)",
                    rusqlite::params![id, kind, at, data],
                )
                .unwrap();
        }
        drop(connection);

        let text = handoff(SessionAgent::OpenCode, "ses_one", &db).expect("an OpenCode handoff");
        assert!(text.contains("Original request: ship it"), "{text}");
        assert!(text.contains("user: and the tests"), "{text}");
        assert!(text.contains("assistant: Doing it."), "{text}");
        assert!(!text.contains("thinking"), "{text}");

        let scan = scan_at(
            &SessionRoots::from_home(&home),
            &mut Cache::default(),
            DEFAULT_LIMIT,
            now,
        );
        let session = scan.sessions.first().expect("the listed session");
        assert!(delete(session), "the session was not deleted");
        assert!(
            scan_at(
                &SessionRoots::from_home(&home),
                &mut Cache::default(),
                DEFAULT_LIMIT,
                now,
            )
            .sessions
            .is_empty()
        );
        let connection = rusqlite::Connection::open(&db).unwrap();
        assert_eq!(
            connection
                .query_row("select count(*) from session_message", [], |row| {
                    row.get::<_, i64>(0)
                })
                .unwrap(),
            0,
            "the messages went with the session"
        );
        let _ = fs::remove_dir_all(&home);
    }
}
