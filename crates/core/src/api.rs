//! The phone panel's payloads: plain serde data and the two pieces of pure logic
//! behind them, so `cargo test -p syro-core` fixes the JSON shape with no window.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::model::{AgentId, AgentSpec, AgentStatus, TerminalId, WorktreeId};

/// Everything the panel shows, in one answer.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Snapshot {
    pub worktrees: Vec<WorktreeStatus>,
    /// The worktree the app is looking at, for the panel to mark.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<WorktreeId>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorktreeStatus {
    pub id: WorktreeId,
    /// The repository's name, because a worktree's own name says nothing about
    /// which project it is in.
    pub repo: String,
    pub name: String,
    pub branch: String,
    /// The most urgent status among its terminals (`AgentStatus` is ordered);
    /// `None` when none of them runs an agent.
    pub status: Option<AgentStatus>,
    pub stalled: bool,
    pub terminals: Vec<TerminalStatus>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TerminalStatus {
    pub id: TerminalId,
    pub agent: Option<AgentId>,
    /// What that agent is called — `Pi`, `Claude Code` — for a panel that has to
    /// name a terminal before its title says what it is doing.
    pub agent_name: Option<String>,
    pub status: Option<AgentStatus>,
    pub stalled: bool,
    /// The task the agent's title names, when it names one.
    pub task: Option<String>,
    /// The command that re-enters this conversation, for copying into a shell.
    pub resume: Option<String>,
    pub cols: usize,
    pub rows: usize,
}

/// One preset the panel can start in a worktree, as the panel needs it: an id to
/// send back and a name to show. Not the whole `AgentSpec` — its command and env
/// are the app's business, and the page has no use for them.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AgentOption {
    pub id: AgentId,
    pub name: String,
}

impl From<&AgentSpec> for AgentOption {
    fn from(spec: &AgentSpec) -> Self {
        Self {
            id: spec.id.clone(),
            name: spec.name.clone(),
        }
    }
}

/// `claude --resume <id>`: the preset's own command, args and `resume_args`, with
/// the placeholders filled. What a phone pastes into a shell to re-enter this
/// conversation. `None` when the preset cannot resume, or when it needs a
/// session id or transcript path that is not known.
pub fn resume_command(agent: &AgentSpec, id: Option<&str>, path: Option<&Path>) -> Option<String> {
    if agent.resume_args.is_empty() {
        return None;
    }
    let mut parts = vec![agent.command.clone()];
    parts.extend(agent.args.iter().cloned());
    for arg in &agent.resume_args {
        let mut arg = arg.clone();
        if arg.contains("{id}") {
            arg = arg.replace("{id}", id?);
        }
        if arg.contains("{path}") {
            arg = arg.replace("{path}", &path?.to_string_lossy());
        }
        parts.push(arg);
    }
    Some(parts.join(" "))
}

/// The bytes one named key sends. `None` is a name this panel does not know, and
/// the request that named it is refused rather than half-sent.
///
/// A named key rather than a byte offset: the panel is a page, and a page cannot
/// know what an agent's "yes" is. The user presses the name, the app sends the
/// bytes.
pub fn key_bytes(key: &str) -> Option<Vec<u8>> {
    Some(match key {
        "enter" => b"\r".to_vec(),
        "esc" => b"\x1b".to_vec(),
        "tab" => b"\t".to_vec(),
        "backspace" => b"\x7f".to_vec(),
        "space" => b" ".to_vec(),
        "up" => b"\x1b[A".to_vec(),
        "down" => b"\x1b[B".to_vec(),
        "right" => b"\x1b[C".to_vec(),
        "left" => b"\x1b[D".to_vec(),
        "ctrl-c" => b"\x03".to_vec(),
        "ctrl-d" => b"\x04".to_vec(),
        // One printable character is its own key: the panel's `y` / `n` buttons
        // and its digits go through here.
        one if one.len() == 1 && one.as_bytes()[0].is_ascii_graphic() => one.as_bytes().to_vec(),
        _ => return None,
    })
}

/// Every key's bytes, in order. `None` when any name is unknown: a partial
/// answer to "press these keys" is not an answer.
pub fn keys_to_bytes(keys: &[String]) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    for key in keys {
        bytes.extend(key_bytes(key)?);
    }
    Some(bytes)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn agent(id: &str, resume_args: &[&str]) -> AgentSpec {
        AgentSpec {
            id: id.into(),
            name: id.into(),
            command: id.into(),
            args: Vec::new(),
            resume_args: resume_args.iter().map(|arg| arg.to_string()).collect(),
            new_args: Vec::new(),
            env: Default::default(),
            status: Vec::new(),
        }
    }

    #[test]
    fn the_status_json_is_the_pages_contract() {
        let snapshot = Snapshot {
            worktrees: vec![WorktreeStatus {
                id: "/w/fix-build".into(),
                repo: "syro".into(),
                name: "fix-build".into(),
                branch: "fix-build".into(),
                status: Some(AgentStatus::Working),
                stalled: false,
                terminals: vec![TerminalStatus {
                    id: "terminal-7".into(),
                    agent: Some("claude".into()),
                    agent_name: Some("Claude Code".into()),
                    status: Some(AgentStatus::Working),
                    stalled: false,
                    task: Some("Fix cursor offsets".into()),
                    resume: Some("claude --resume abc".into()),
                    cols: 120,
                    rows: 32,
                }],
            }],
            active: Some("/w/fix-build".into()),
        };
        let json = serde_json::to_value(&snapshot).unwrap();
        assert_eq!(json["worktrees"][0]["status"], "working");
        assert_eq!(
            json["worktrees"][0]["terminals"][0]["task"],
            "Fix cursor offsets"
        );
        assert_eq!(json["worktrees"][0]["terminals"][0]["cols"], 120);
        assert_eq!(json["active"], "/w/fix-build");
        // A terminal with no agent is `null`, not absent: the page reads the key.
        assert!(json["worktrees"][0]["terminals"][0]["agent"].is_string());
    }

    #[test]
    fn an_agent_option_is_an_id_and_a_name() {
        let spec = AgentSpec {
            id: "claude".into(),
            name: "Claude Code".into(),
            command: "claude".into(),
            args: vec!["--dangerously-skip-permissions".into()],
            resume_args: Vec::new(),
            new_args: Vec::new(),
            env: [("SECRET".to_string(), "x".to_string())].into(),
            status: Vec::new(),
        };
        let option = AgentOption::from(&spec);
        assert_eq!(option.id, "claude");
        assert_eq!(option.name, "Claude Code");
        // The page is never told the command, its args, or the environment: what
        // it starts is the preset, not a command line.
        let json = serde_json::to_string(&option).unwrap();
        assert_eq!(json, r#"{"id":"claude","name":"Claude Code"}"#);
        assert!(!json.contains("dangerously"));
        assert!(!json.contains("SECRET"));
    }

    #[test]
    fn a_resume_command_is_the_preset_plus_its_placeholders() {
        let claude = agent("claude", &["--resume", "{id}"]);
        assert_eq!(
            resume_command(&claude, Some("abc-123"), None).as_deref(),
            Some("claude --resume abc-123")
        );
        // Pi resumes by transcript path.
        let pi = agent("pi", &["--session", "{path}"]);
        assert_eq!(
            resume_command(&pi, None, Some(Path::new("/tmp/t.jsonl"))).as_deref(),
            Some("pi --session /tmp/t.jsonl")
        );
        // The preset's own args come along: the resume is the same agent.
        let mut with_args = agent("claude", &["--resume", "{id}"]);
        with_args.args = vec!["--dangerously-skip-permissions".into()];
        assert_eq!(
            resume_command(&with_args, Some("abc"), None).as_deref(),
            Some("claude --dangerously-skip-permissions --resume abc")
        );
    }

    #[test]
    fn a_resume_command_without_what_it_needs_is_none() {
        let claude = agent("claude", &["--resume", "{id}"]);
        // The placeholder is there and the id is not: a command naming no
        // session would start a new conversation under a row that promised the
        // old one.
        assert_eq!(resume_command(&claude, None, None), None);
        let pi = agent("pi", &["--session", "{path}"]);
        assert_eq!(resume_command(&pi, Some("abc"), None), None);
        // A preset that cannot resume at all.
        assert_eq!(
            resume_command(&agent("codex", &[]), Some("abc"), None),
            None
        );
    }

    #[test]
    fn keys_map_to_the_bytes_a_child_expects() {
        assert_eq!(key_bytes("enter").as_deref(), Some(&b"\r"[..]));
        assert_eq!(key_bytes("esc").as_deref(), Some(&b"\x1b"[..]));
        assert_eq!(key_bytes("ctrl-c").as_deref(), Some(&b"\x03"[..]));
        assert_eq!(key_bytes("up").as_deref(), Some(&b"\x1b[A"[..]));
        assert_eq!(key_bytes("y").as_deref(), Some(&b"y"[..]));
        assert_eq!(key_bytes("1").as_deref(), Some(&b"1"[..]));
        // Nothing a page might send as a control character sneaks through as a
        // one-character "key".
        assert_eq!(key_bytes("\r"), None);
        assert_eq!(key_bytes(""), None);
        assert_eq!(key_bytes("approve"), None);
    }

    #[test]
    fn keys_are_sent_in_order_or_not_at_all() {
        let keys = |names: &[&str]| names.iter().map(|n| n.to_string()).collect::<Vec<_>>();
        assert_eq!(
            keys_to_bytes(&keys(&["enter", "y"])).as_deref(),
            Some(&b"\ry"[..])
        );
        // One unknown name sends nothing: a half-typed command is worse than a
        // refused one.
        assert_eq!(keys_to_bytes(&keys(&["enter", "nope"])), None);
        assert_eq!(keys_to_bytes(&[]).as_deref(), Some(&b""[..]));
    }

    #[test]
    fn a_terminal_status_round_trips() {
        let status = TerminalStatus {
            id: "terminal-1".into(),
            agent: None,
            agent_name: None,
            status: None,
            stalled: false,
            task: None,
            resume: None,
            cols: 80,
            rows: 24,
        };
        let json = serde_json::to_string(&status).unwrap();
        assert_eq!(
            serde_json::from_str::<TerminalStatus>(&json).unwrap(),
            status
        );
        // `Option` fields are `null`, never missing: the page reads the keys.
        assert!(json.contains("\"task\":null"));
    }

    #[test]
    fn a_worktree_id_is_a_path_and_stays_one() {
        let path = PathBuf::from("/Users/me/syro/workspaces/syro/fix-build");
        let status = WorktreeStatus {
            id: path.display().to_string(),
            repo: "syro".into(),
            name: "fix-build".into(),
            branch: "fix-build".into(),
            status: None,
            stalled: false,
            terminals: Vec::new(),
        };
        let json = serde_json::to_string(&status).unwrap();
        assert!(json.contains("/Users/me/syro/workspaces/syro/fix-build"));
    }
}
