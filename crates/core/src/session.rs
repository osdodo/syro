//! Past agent sessions: discovery of the transcripts the agents write, and the one-pass summary
//! the history panel lists.
//!
//! Two layouts only — Claude Code (`~/.claude/projects/<encoded-cwd>/<id>.jsonl`) and Pi
//! (`~/.pi/agent/sessions/--<encoded-cwd>--/*.jsonl`). Records are counted through a shallow
//! borrowed struct, so a 27 MB transcript costs a byte scan rather than a `Value` per line.

use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use rust_i18n::t;

use crate::model::Worktree;

/// How many transcripts a scan parses, newest first; everything past it is reported as a total.
pub const DEFAULT_LIMIT: usize = 250;

/// The head zone: the first 64 KB, where the records that name the session are.
const HEAD_BYTES: u64 = 64 * 1024;

/// How many of the newest turn records are kept for the preview. A turn that ends in tool
/// traffic writes a run of textless records, so the newest record with text is often a dozen
/// lines up.
const TAIL_LINES: usize = 16;

/// How many characters of a turn's text the preview keeps.
const PREVIEW_CHARS: usize = 240;

/// The agent a transcript belongs to; its spelling is the `AgentSpec` id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionAgent {
    Claude,
    Pi,
}

impl SessionAgent {
    pub fn id(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Pi => "pi",
        }
    }

    /// The transcript layout an agent preset writes, by its id.
    pub fn from_id(id: &str) -> Option<Self> {
        match id {
            "claude" => Some(Self::Claude),
            "pi" => Some(Self::Pi),
            _ => None,
        }
    }

    pub fn display(self) -> &'static str {
        match self {
            Self::Claude => "Claude Code",
            Self::Pi => "Pi",
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
    pub messages: u64,
    /// Last write, in epoch milliseconds: what the rows sort by.
    pub updated_ms: i64,
}

/// Where the two agents keep their transcripts under a home directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRoots {
    pub claude: PathBuf,
    pub pi: PathBuf,
}

impl SessionRoots {
    pub fn from_home(home: &Path) -> Self {
        Self {
            claude: home.join(".claude").join("projects"),
            pi: home.join(".pi").join("agent").join("sessions"),
        }
    }
}

impl Default for SessionRoots {
    /// This user's roots, from `$HOME`.
    fn default() -> Self {
        let home = std::env::var_os("HOME").expect("HOME is not set");
        Self::from_home(Path::new(&home))
    }
}

/// The result of one scan: what to list, and how much was there to list.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Scan {
    pub sessions: Vec<Session>,
    /// Every transcript discovered, before the cap.
    pub total: usize,
}

/// Each transcript's fold and bytes read, valid only while the file is the same size or longer.
#[derive(Debug, Default)]
pub struct Cache {
    entries: HashMap<PathBuf, Entry>,
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
    title: Option<String>,
    /// The preview's text, refreshed from the tail buffer at the end of a pass.
    preview: Option<String>,
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
    let mut candidates = discover(&roots.claude, SessionAgent::Claude);
    candidates.extend(discover(&roots.pi, SessionAgent::Pi));
    // Newest first, and the path breaks ties so two transcripts written in the
    // same millisecond keep one order between scans.
    candidates.sort_by(|left, right| {
        right
            .mtime_ms
            .cmp(&left.mtime_ms)
            .then_with(|| left.path.cmp(&right.path))
    });
    let total = candidates.len();
    candidates.truncate(limit);

    let live: Vec<&Path> = candidates.iter().map(|c| c.path.as_path()).collect();
    cache
        .entries
        .retain(|path, _| live.contains(&path.as_path()));

    let mut sessions: Vec<Session> = candidates
        .into_iter()
        .filter_map(|candidate| scan_one(&candidate, cache))
        .collect();
    sessions.sort_by(|left, right| {
        right
            .updated_ms
            .cmp(&left.updated_ms)
            .then_with(|| left.path.cmp(&right.path))
    });

    Scan { sessions, total }
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
        // `<bucket>/<id>.jsonl`: the id is unique, so the bucket's encoding need not be guessed.
        SessionAgent::Claude => std::fs::read_dir(&roots.claude)
            .ok()?
            .flatten()
            .map(|bucket| bucket.path().join(format!("{id}.jsonl")))
            .find(|path| path.is_file()),
    }
}

/// The first thing the user asked in a transcript, cut to the panel's title length. Reads the head zone only, so a long transcript costs no more
/// than a short one.
pub fn first_prompt(agent: SessionAgent, path: &Path) -> Option<String> {
    use std::io::Read as _;
    let mut reader = BufReader::new(File::open(path).ok()?.take(HEAD_BYTES));
    let (mut fold, mut tail) = (Fold::default(), VecDeque::new());
    let (mut first_user, mut line) = (String::new(), String::new());
    while first_user.is_empty() {
        line.clear();
        if reader.read_line(&mut line).ok()? == 0 {
            break;
        }
        consume(agent, &line, &mut fold, &mut tail, &mut first_user, true);
    }
    text_of(agent, &first_user).map(|text| one_line(&text, PREVIEW_CHARS))
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

fn modified_ms(metadata: &std::fs::Metadata) -> i64 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|since| since.as_millis() as i64)
        .unwrap_or(0)
}

/// Fold one transcript, resuming from where the cache left off.
fn scan_one(candidate: &Candidate, cache: &mut Cache) -> Option<Session> {
    let previous = cache.entries.get(&candidate.path);
    // A file that shrank was replaced: start again rather than read the middle
    // of a transcript whose head is gone.
    let start = match previous {
        Some(entry) if entry.size <= candidate.size => entry.fold.clone(),
        _ => Fold::default(),
    };
    let fold = fold_file(candidate, start).ok()?;
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
fn fold_file(candidate: &Candidate, mut fold: Fold) -> std::io::Result<Fold> {
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
fn consume(
    agent: SessionAgent,
    line: &str,
    fold: &mut Fold,
    tail: &mut VecDeque<String>,
    first_user: &mut String,
    head: bool,
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
                fold.model = record.model_id.map(str::to_string).or(fold.model.take());
            }
            if record.kind == Some("message") {
                let role = record.message.as_ref().and_then(|message| message.role);
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
    }

    if !is_turn {
        return;
    }
    fold.messages += 1;
    // The newest few turns for the preview, and the first user turn for the title.
    tail.push_back(trimmed.to_string());
    while tail.len() > TAIL_LINES {
        tail.pop_front();
    }
    if head && first_user.is_empty() && by_user {
        *first_user = trimmed.to_string();
    }
}

/// The text of a turn record, the only thing this module decodes a whole record for.
fn text_of(agent: SessionAgent, line: &str) -> Option<String> {
    if line.is_empty() {
        return None;
    }
    let value: Value = serde_json::from_str(line).ok()?;
    let content = match agent {
        SessionAgent::Pi => value.get("message")?.get("content")?,
        SessionAgent::Claude => value.get("message")?.get("content")?,
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

/// One record, as far as the count needs it: every field optional and borrowed, so a line costs
/// a scan rather than a document.
#[derive(Debug, Deserialize)]
struct Record<'a> {
    #[serde(rename = "type")]
    kind: Option<&'a str>,
    #[serde(rename = "sessionId")]
    session_id: Option<&'a str>,
    #[serde(rename = "modelId")]
    model_id: Option<&'a str>,
    cwd: Option<&'a str>,
    id: Option<&'a str>,
    message: Option<Message<'a>>,
}

#[derive(Debug, Deserialize)]
struct Message<'a> {
    role: Option<&'a str>,
    model: Option<&'a str>,
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
        // Two turns: the tool result is not one.
        assert_eq!(session.messages, 2);
        assert!(session.updated_ms > 0);
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
            first_prompt(SessionAgent::Pi, &pi).as_deref(),
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
            first_prompt(SessionAgent::Claude, &claude).as_deref(),
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
}
