//! Agent presets (`builtin.toml`, merged with user config by `id`) and the title→status engine.
//!
//! Classification is an ordered regex list per agent: first match wins, no match means "not an
//! agent". A rule may be gated on the agent's name, because `ready` / `running` / `waiting` are
//! ordinary English in shell and cwd titles.

use std::time::Duration;

use serde::Deserialize;

use crate::model::{AgentId, AgentSpec, AgentStatus};

/// Builtin agent presets, compiled into the binary.
pub fn builtin_agents() -> Vec<AgentSpec> {
    #[derive(Deserialize)]
    struct Doc {
        agent: Vec<AgentSpec>,
    }
    toml::from_str::<Doc>(include_str!("builtin.toml"))
        .expect("builtin.toml must parse")
        .agent
}

/// Merge user agents over builtin by `id`: a user entry replaces its builtin in place, user-
/// only entries are appended.
pub fn resolve_agents(user: &[AgentSpec]) -> Vec<AgentSpec> {
    let mut out: Vec<AgentSpec> = builtin_agents()
        .into_iter()
        .map(|builtin| {
            user.iter()
                .find(|u| u.id == builtin.id)
                .cloned()
                .unwrap_or(builtin)
        })
        .collect();

    for u in user {
        if !out.iter().any(|b| b.id == u.id) {
            out.push(u.clone());
        }
    }
    out
}

/// Top-to-bottom first match of `spec.status`; no match = not an agent.
pub fn classify(spec: &AgentSpec, title: &str) -> Option<AgentStatus> {
    if title.is_empty() {
        return None;
    }
    for rule in &spec.status {
        // ponytail: recompiles each rule per call; fine for a handful of rules per title change.
        let Ok(re) = regex::Regex::new(&rule.pattern) else {
            // ponytail: a bad user regex is a config error; skip the rule, not the whole agent.
            continue;
        };
        if !re.is_match(title) {
            continue;
        }
        if let Some(name) = rule.requires.as_deref() {
            if !names_agent(title, name) {
                continue;
            }
        }
        return Some(rule.state);
    }
    None
}

/// True when `title` names `agent` as a whole token: `codex` and `Codex.exe` count, `~/codex-
/// scratch` and `codex.working` do not.
///
/// `regex` has no lookbehind, so the boundary is a consuming group; a word character, `.`, `/`
/// or `-` adjoining the name means it is part of something longer.
fn names_agent(title: &str, agent: &str) -> bool {
    let pattern = format!(
        r"(?i)(?:^|[^\w./\\-]){}(?:\.(?:exe|cmd|bat|ps1))?(?:$|[^\w./\\-])",
        regex::escape(agent)
    );
    match regex::Regex::new(&pattern) {
        Ok(re) => re.is_match(title),
        Err(_) => false,
    }
}

/// How long an agent may say nothing on its terminal before it reads as stalled.
///
/// `shortcut:` not configurable; add a settings key when three minutes turns out wrong for a real agent.
pub const STALL_AFTER: Duration = Duration::from_secs(180);

/// Whether a terminal reads as stalled: the agent said it was working, and then said nothing
/// for [`STALL_AFTER`].
///
/// Only `Working` can stall — `Permission` is the app waiting on the user.
pub fn stalled(status: Option<AgentStatus>, silent_for: Duration) -> bool {
    status == Some(AgentStatus::Working) && silent_for >= STALL_AFTER
}

/// The task an agent's title names, if it names one: Claude Code's `✳ Fix the build`, Pi's
/// `π - session name - project`, Codex's `Codex: fix cursor offsets`. `None` for a title that
/// is only the agent, a status word, or (Pi) the project directory.
pub fn task_of(spec: &AgentSpec, title: &str) -> Option<String> {
    // The status marks the presets put in front of the text.
    let text = title.trim_start_matches(|c: char| {
        c.is_whitespace()
            || matches!(c, '\u{2800}'..='\u{28FF}' | '\u{25D0}'..='\u{25D3}' | '\u{25B2}' | '\u{2733}' | '\u{270B}' | '\u{2726}' | '\u{23F2}' | '\u{25C7}')
    });
    let text = text
        .strip_prefix("* ")
        .or_else(|| text.strip_prefix(". "))
        .unwrap_or(text);
    if spec.id == "pi" {
        let rest = regex::Regex::new(r"^(?:π|Pi|OMP)\s*[-:!>]?\s*")
            .ok()?
            .replace(text, "");
        // The last part is the project — the directory, which the fallback already says better.
        let (name, _project) = rest.rsplit_once(" - ")?;
        let name = name.trim();
        return (!name.is_empty()).then(|| name.to_string());
    }
    let brand = format!(
        r"(?i)^(?:{}|{})(?:\.exe)?\s*[-:]?\s*",
        regex::escape(&spec.name),
        regex::escape(spec.id.as_str())
    );
    let rest = regex::Regex::new(&brand).ok()?.replace(text.trim(), "");
    // `Ready (syro)`, `Working… (syro)`: a status and the directory, not a task.
    let rest = regex::Regex::new(
        r"(?i)^(?:(?:ready|idle|done|working|thinking|running|action required|waiting)\W*)?(?:\(.*\))?\s*$",
    )
    .ok()?
    .replace(rest.trim(), "");
    let rest = rest.trim();
    (!rest.is_empty()).then(|| rest.to_string())
}

/// How many characters of a task a name keeps.
const NAME_DETAIL_CHARS: usize = 30;

/// What a tab and a sidebar row call each agent terminal of one worktree, given as `(agent,
/// detail)` in pane order: `Pi - fix the build`. Two that would read the same are numbered,
/// `Pi 1 - main` and `Pi 2 - main`, which is the only thing that tells them apart.
pub fn names(terminals: &[(String, String)]) -> Vec<String> {
    terminals
        .iter()
        .zip(numbered(terminals))
        .map(|((agent, _), (number, detail))| {
            if number == 0 {
                format!("{agent} - {detail}")
            } else {
                format!("{agent} {number} - {detail}")
            }
        })
        .collect()
}

/// The same names split so a readout can draw the agent's logo in place of its
/// word: `(0, detail)` when the name is unique, and `(number, detail)` — numbered
/// from 1 — when two would read the same. The detail is the trimmed, shortened
/// task [`names`] builds the name from.
pub fn numbered(terminals: &[(String, String)]) -> Vec<(usize, String)> {
    let short: Vec<(&str, String)> = terminals
        .iter()
        .map(|(agent, detail)| {
            let detail = detail.split_whitespace().collect::<Vec<_>>().join(" ");
            let detail = if detail.chars().count() > NAME_DETAIL_CHARS {
                let cut: String = detail.chars().take(NAME_DETAIL_CHARS - 1).collect();
                format!("{}…", cut.trim_end())
            } else {
                detail
            };
            (agent.as_str(), detail)
        })
        .collect();
    short
        .iter()
        .enumerate()
        .map(|(index, (agent, detail))| {
            let same = |other: &&(&str, String)| other.0 == *agent && other.1 == *detail;
            if short.iter().filter(same).count() == 1 {
                (0, detail.clone())
            } else {
                let number = short[..=index].iter().filter(same).count();
                (number, detail.clone())
            }
        })
        .collect()
}

/// First spec in `agents` whose rules match `title`.
pub fn classify_any(agents: &[AgentSpec], title: &str) -> Option<(AgentId, AgentStatus)> {
    for spec in agents {
        if let Some(status) = classify(spec, title) {
            return Some((spec.id.clone(), status));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::AgentStatusRule;

    fn find<'a>(agents: &'a [AgentSpec], id: &str) -> &'a AgentSpec {
        agents.iter().find(|a| a.id == id).unwrap()
    }

    /// A spec built in test, so the engine's contracts do not depend on today's presets.
    fn spec(rules: &[(&str, Option<&str>, AgentStatus)]) -> AgentSpec {
        AgentSpec {
            id: "test".into(),
            name: "Test".into(),
            command: "test".into(),
            args: vec![],
            resume_args: vec![],
            new_args: Vec::new(),
            env: Default::default(),
            status: rules
                .iter()
                .map(|(pattern, requires, state)| AgentStatusRule {
                    pattern: pattern.to_string(),
                    requires: requires.map(str::to_string),
                    state: *state,
                })
                .collect(),
        }
    }

    #[test]
    fn builtin_has_five_presets() {
        let agents = builtin_agents();
        let ids: Vec<&str> = agents.iter().map(|a| a.id.as_str()).collect();
        // The order is `builtin.toml`'s, and it is the order every picker lists
        // them in — Pi first, because it is this app's own agent.
        assert_eq!(ids, ["pi", "codex", "claude", "opencode", "gemini"]);
    }

    /// Every builtin keyword rule must carry `requires`, and must spell its keyword with the boundary guards.
    #[test]
    fn builtin_gates_every_keyword_rule() {
        let fragments = [
            "|permission|waiting)",
            r"(?:^|[^\w./\\-])(?:ready|idle|done)",
            r"(?:^|[^\w./\\-])(?:working|thinking|running)",
        ];
        for agent in builtin_agents() {
            for rule in &agent.status {
                if fragments.iter().any(|f| rule.pattern.contains(f)) {
                    assert!(
                        rule.requires.is_some(),
                        "{} rule {:?} is a keyword rule and must say what it requires",
                        agent.id,
                        rule.pattern,
                    );
                }
            }
        }
    }

    #[test]
    fn only_working_can_stall() {
        use super::stalled;
        let long = STALL_AFTER + Duration::from_secs(1);
        assert!(stalled(Some(AgentStatus::Working), long));
        // A waiting agent is not late, and a shell with no agent has no rules to be judged by.
        assert!(!stalled(Some(AgentStatus::Permission), long));
        assert!(!stalled(Some(AgentStatus::Idle), long));
        assert!(!stalled(None, long));
    }

    #[test]
    fn a_working_agent_that_is_talking_is_not_stalled() {
        use super::stalled;
        assert!(!stalled(Some(AgentStatus::Working), Duration::ZERO));
        assert!(!stalled(
            Some(AgentStatus::Working),
            STALL_AFTER - Duration::from_secs(1)
        ));
        // The threshold is inclusive: at exactly three minutes of silence the agent has stalled.
        assert!(stalled(Some(AgentStatus::Working), STALL_AFTER));
    }

    /// A rule without `requires` is ungated, so the gate is reversible in config.
    #[test]
    fn a_rule_without_requires_is_ungated() {
        let ungated = spec(&[("(?i)waiting", None, AgentStatus::Permission)]);
        assert_eq!(
            classify(&ungated, "waiting for input"),
            Some(AgentStatus::Permission)
        );

        let gated = spec(&[("(?i)waiting", Some("claude"), AgentStatus::Permission)]);
        assert_eq!(classify(&gated, "waiting for input"), None);
        assert_eq!(
            classify(&gated, "claude waiting for input"),
            Some(AgentStatus::Permission)
        );
    }

    /// The name must be a token, not a substring: this is what keeps cwd and worktree titles from
    /// naming an agent.
    #[test]
    fn the_gate_matches_a_name_only_as_a_token() {
        let gated = spec(&[("(?s).", Some("codex"), AgentStatus::Idle)]);
        for named in [
            "codex",
            "Codex",
            "  CODEX  ",
            "codex: fix it",
            "run codex",
            "codex.exe",
        ] {
            assert_eq!(
                classify(&gated, named),
                Some(AgentStatus::Idle),
                "{named:?}"
            );
        }
        for glued in [
            "~/codex-scratch",
            "codex.working",
            "~/codex/working",
            "opencode",
        ] {
            assert_eq!(
                classify(&gated, glued),
                None,
                "{glued:?} is not a codex title"
            );
        }
    }

    /// The keyword rules carry boundary guards, so a path or a hyphenated compound is not a status word.
    #[test]
    fn keywords_need_a_word_boundary() {
        let agents = builtin_agents();
        let claude = find(&agents, "claude");

        for refused in [
            "~/codex/working",
            "~/codex/thinking",
            "~/codex/running",
            r"C:\codex\working",
            "codex.working",
            "reworking the diff",
            "is-working-cap",
            "overthinking it",
        ] {
            assert_eq!(
                classify(claude, refused),
                None,
                "{refused:?} is not a working title"
            );
        }

        // A keyword at a whitespace boundary is a status, punctuation and all.
        assert_eq!(
            classify(claude, "claude working on task"),
            Some(AgentStatus::Working)
        );
        assert_eq!(classify(claude, "claude done."), Some(AgentStatus::Idle));
        assert_eq!(classify(claude, "claude ready?"), Some(AgentStatus::Idle));
    }

    /// The task text is free, so a spinner has to outrank a keyword: Claude Code titles itself with the task.
    #[test]
    fn a_spinner_outranks_a_keyword() {
        let agents = builtin_agents();
        let claude = find(&agents, "claude");
        assert_eq!(
            classify(claude, "⠂ claude waiting for the build"),
            Some(AgentStatus::Working)
        );
        // ... and the idle frame does the same over a working word.
        assert_eq!(
            classify(claude, "✳ claude running late"),
            Some(AgentStatus::Idle)
        );
    }

    #[test]
    fn a_permission_rule_outranks_a_working_rule() {
        let agents = builtin_agents();
        let gemini = find(&agents, "gemini");
        assert_eq!(
            classify(gemini, "✋✦ Gemini CLI"),
            Some(AgentStatus::Permission)
        );
        assert_eq!(
            classify(gemini, "gemini working, action required"),
            Some(AgentStatus::Permission)
        );
    }

    /// Claude Code's agent-management screen is not an agent pane.
    #[test]
    fn claude_management_screen_is_not_an_agent() {
        let agents = builtin_agents();
        let claude = find(&agents, "claude");
        for title in [
            "claude agents",
            "  Claude Agents  ",
            "claude.exe agents",
            "Claude.CMD agents",
            r"C:\Users\dev\AppData\Roaming\npm\claude.cmd agents",
            "\"C:\\Users\\dev\\AppData\\Roaming\\npm\\claude.cmd\" agents",
        ] {
            assert_eq!(classify(claude, title), None, "{title:?}");
        }
        // ... but the same words with a status word in them are a live agent.
        assert_eq!(
            classify(claude, "claude agents working"),
            Some(AgentStatus::Working)
        );
    }

    #[test]
    fn pi_reads_its_turn_marker() {
        let agents = builtin_agents();
        let pi = find(&agents, "pi");
        assert_eq!(
            classify(pi, "π ! my-project"),
            Some(AgentStatus::Permission)
        );
        assert_eq!(classify(pi, "π > my-project"), Some(AgentStatus::Idle));
        assert_eq!(classify(pi, "π : my-project"), Some(AgentStatus::Working));
        assert_eq!(classify(pi, "OMP ! release"), Some(AgentStatus::Permission));
        // A frame outranks the marker, which is why `⠋ π - …` is working.
        assert_eq!(classify(pi, "⠋ π - my-project"), Some(AgentStatus::Working));
        // The legacy no-marker form is idle, not missing.
        assert_eq!(classify(pi, "π - my-project"), Some(AgentStatus::Idle));
        assert_eq!(classify(pi, "π: my-project"), Some(AgentStatus::Idle));
        assert_eq!(classify(pi, "π"), None);
    }

    #[test]
    fn plain_prompt_is_not_an_agent() {
        let agents = builtin_agents();
        for spec in &agents {
            for title in [
                "",
                "user@host ~ %",
                "bash",
                "vim myfile.ts",
                "zsh",
                "my-claude-fix",
                "Terminal 1",
                "timestamp ready",
                "example permission needed",
                "android emulator ready",
                "Factory Droid needs input",
                "Cursor Agent",
                // Agents we do not ship must not be named by the words of a
                // title: no preset claims another agent's keyword phrase.
                "aider done",
                "Hermes working",
                "Devin - action required",
            ] {
                assert_eq!(classify(spec, title), None, "{} <- {title:?}", spec.id);
            }
        }
    }

    /// The one assertion this port changed: a bare "waiting" is ordinary title text, and answering
    /// permission for it cries wolf on the one status the product exists to show.
    #[test]
    fn an_ungated_waiting_title_is_no_longer_permission() {
        let agents = builtin_agents();
        let claude = find(&agents, "claude");
        assert_eq!(classify(claude, "Waiting for approval"), None);
        assert_eq!(
            classify(claude, "claude waiting for approval"),
            Some(AgentStatus::Permission)
        );
        // Claude Code's real permission title names itself, so the gate costs nothing on the shapes that exist.
        assert_eq!(
            classify(claude, "Claude Code - action required"),
            Some(AgentStatus::Permission)
        );
        // A user who disagrees can have the old answer back in one config line.
        let ungated = spec(&[(
            "(?i)(?:action required|permission|waiting)",
            None,
            AgentStatus::Permission,
        )]);
        assert_eq!(
            classify(&ungated, "Waiting for approval"),
            Some(AgentStatus::Permission)
        );
    }

    /// A bare frame is activity whichever agent painted it, because a frame proves a TUI is alive,
    /// not whose it is. The presets differ only in whether they watch frames.
    #[test]
    fn a_bare_frame_is_activity_wherever_the_preset_watches_frames() {
        let agents = builtin_agents();
        for (id, expected) in [
            ("claude", Some(AgentStatus::Working)),
            ("codex", Some(AgentStatus::Working)),
            ("opencode", Some(AgentStatus::Working)),
            ("pi", Some(AgentStatus::Working)),
            // Gemini CLI has four glyphs of its own and no braille rule.
            ("gemini", None),
        ] {
            assert_eq!(
                classify(find(&agents, id), "⠋ Devin"),
                expected,
                "{id} <- a braille frame"
            );
        }
    }

    /// Real titles with their expected answers, from the reference status corpus.
    #[test]
    fn corpus_of_real_titles() {
        use AgentStatus::{Idle, Permission, Working};
        let agents = builtin_agents();
        let cases: &[(&str, &str, Option<AgentStatus>)] = &[
            // Claude Code: the title is the task, marked with a frame or ✳.
            ("claude", "✳ Claude Code", Some(Idle)),
            ("claude", "⠂ Claude Code", Some(Working)),
            ("claude", "✳", Some(Idle)),
            (
                "claude",
                "✳ User acknowledgment and confirmation",
                Some(Idle),
            ),
            (
                "claude",
                "⠐ User acknowledgment and confirmation",
                Some(Working),
            ),
            ("claude", ". claude", Some(Working)),
            ("claude", "* claude", Some(Idle)),
            ("claude", ". Compare Opencode Vs Zed", Some(Working)),
            ("claude", "* Review Codex behavior", Some(Idle)),
            ("claude", "claude", Some(Idle)),
            ("claude", "CLAUDE", Some(Idle)),
            ("claude", "✳ Ready", Some(Idle)),
            ("claude", "Claude Code - action required", Some(Permission)),
            ("claude", "claude ready", Some(Idle)),
            ("claude", "claude idle", Some(Idle)),
            ("claude", "claude working on task", Some(Working)),
            ("claude", "claude thinking", Some(Working)),
            ("claude", "claude agents", None),
            ("claude", "claude agents working", Some(Working)),
            ("claude", "claude-scratch", None),
            ("claude", "  Claude Agent  ", None),
            ("claude", "~/.claude/settings.json", None),
            // Codex: a braille frame while working, the bare name when not.
            ("codex", "⠋ Codex is thinking", Some(Working)),
            ("codex", "codex", Some(Idle)),
            ("codex", "Codex", Some(Idle)),
            ("codex", "codex idle", Some(Idle)),
            ("codex", "Codex Working", Some(Working)),
            ("codex", "Codex done", Some(Idle)),
            ("codex", "Codex done.", Some(Idle)),
            ("codex", "codex - permission needed", Some(Permission)),
            ("codex", "Codex: fix cursor offsets", Some(Idle)),
            ("codex", "codex is-working-cap", Some(Idle)),
            ("codex", "~/codex-scratch", None),
            ("codex", "~/codex already built", None),
            ("codex", "~/codex/working", None),
            ("codex", "codex.working", None),
            // OpenCode.
            ("opencode", "opencode", Some(Idle)),
            ("opencode", "OpenCode", Some(Idle)),
            ("opencode", "OpenCode ready", Some(Idle)),
            ("opencode", "opencode running tests", Some(Working)),
            ("opencode", "opencode-blinker", None),
            ("opencode", "OpenCode - action required", Some(Permission)),
            ("opencode", "OC | Screenshot UI issue", Some(Working)),
            ("opencode", "OC |", None),
            // Pi: brand frames, the state marker, and the legacy π form.
            ("pi", "π - my-project", Some(Idle)),
            ("pi", "π - session-name - my-project", Some(Idle)),
            ("pi", "π: my-project", Some(Idle)),
            ("pi", "π ! my-project", Some(Permission)),
            ("pi", "π > my-project", Some(Idle)),
            ("pi", "π : my-project", Some(Working)),
            ("pi", "⠋ π - my-project", Some(Working)),
            ("pi", "pi", Some(Idle)),
            ("pi", "pi ready", Some(Idle)),
            ("pi", "omp ready", Some(Idle)),
            ("pi", "OMP - action required", Some(Permission)),
            ("pi", "pi - refactor the runner", None),
            ("pi", "π - gemini-project", Some(Idle)),
            // Gemini CLI: four glyphs, permission first.
            ("gemini", "✋ Gemini CLI", Some(Permission)),
            ("gemini", "✦ Gemini CLI", Some(Working)),
            ("gemini", "◇ Gemini CLI", Some(Idle)),
            ("gemini", "⏲  Working… (my-project)", Some(Working)),
            ("gemini", "✋✦ Gemini CLI", Some(Permission)),
            ("gemini", "◇  Ready (my-project)", Some(Idle)),
            (
                "gemini",
                "✦  Implementing feature (my-project)",
                Some(Working),
            ),
            (
                "gemini",
                "✋  Action Required (my-project)",
                Some(Permission),
            ),
            ("gemini", "gemini waiting for input", Some(Permission)),
            ("gemini", "gemini thinking", Some(Working)),
            ("gemini", "gemini", Some(Idle)),
            ("gemini", "~/gemini/working", None),
            ("gemini", "gemini-project", None),
            // And the same title read by the wrong preset claims nothing.
            ("claude", "✦ Gemini CLI", None),
            ("gemini", "✳ Claude Code", None),
            ("codex", "π ! my-project", None),
        ];

        let mut checked = 0;
        for (agent, title, expected) in cases {
            assert_eq!(
                classify(find(&agents, agent), title),
                *expected,
                "{agent} <- {title:?}"
            );
            checked += 1;
        }
        assert!(checked >= 20, "the corpus is the point: {checked} cases");
    }

    /// `classify_any` reads the corpus the same way the per-agent call does.
    #[test]
    fn classify_any_finds_the_agent_that_owns_the_title() {
        let agents = builtin_agents();
        assert_eq!(
            classify_any(&agents, "✦ Gemini CLI"),
            Some(("gemini".to_string(), AgentStatus::Working))
        );
        assert_eq!(
            classify_any(&agents, "✳ Claude Code"),
            Some(("claude".to_string(), AgentStatus::Idle))
        );
        assert_eq!(classify_any(&agents, "bash"), None);
    }

    #[test]
    fn a_title_names_the_task_when_it_has_one() {
        let agents = builtin_agents();
        let task = |id: &str, title: &str| task_of(find(&agents, id), title);
        assert_eq!(
            task("claude", "✳ Fix the build").as_deref(),
            Some("Fix the build")
        );
        assert_eq!(
            task("claude", "⠂ Fix the build").as_deref(),
            Some("Fix the build")
        );
        assert_eq!(task("claude", "✳ Claude Code"), None);
        assert_eq!(task("pi", "π - syro"), None);
        assert_eq!(
            task("pi", "π : refactor - syro").as_deref(),
            Some("refactor")
        );
        assert_eq!(
            task("pi", "π - session-name - my-project").as_deref(),
            Some("session-name")
        );
        assert_eq!(
            task("codex", "Codex: fix cursor offsets").as_deref(),
            Some("fix cursor offsets")
        );
        assert_eq!(task("codex", "codex working"), None);
        assert_eq!(task("gemini", "◇  Ready (syro)"), None);
        assert_eq!(task("gemini", "✦  Working… (syro)"), None);
    }

    #[test]
    fn names_number_only_the_ones_that_collide() {
        let pair = |agent: &str, detail: &str| (agent.to_string(), detail.to_string());
        assert_eq!(
            names(&[
                pair("Pi", "main"),
                pair("Pi", "fix the build"),
                pair("Pi", "main"),
                pair("Claude Code", "main"),
            ]),
            [
                "Pi 1 - main",
                "Pi - fix the build",
                "Pi 2 - main",
                "Claude Code - main"
            ]
        );
        let long = names(&[pair(
            "Pi",
            "查看 okx-mode.md，看看还有什么没做，然后把它们都做完再提交",
        )]);
        assert_eq!(
            long,
            ["Pi - 查看 okx-mode.md，看看还有什么没做，然后把它们…"]
        );
    }

    /// The split form numbers the same as the joined one, so a logo and the
    /// words beside it agree on which of two identical agents is which.
    #[test]
    fn numbered_splits_the_name_it_numbers() {
        let pair = |agent: &str, detail: &str| (agent.to_string(), detail.to_string());
        assert_eq!(
            numbered(&[
                pair("Pi", "main"),
                pair("Pi", "fix the build"),
                pair("Pi", "main"),
                pair("Claude Code", "main"),
            ]),
            [
                (1, "main".to_string()),
                (0, "fix the build".to_string()),
                (2, "main".to_string()),
                (0, "main".to_string()),
            ]
        );
    }

    #[test]
    fn resolve_merges_by_id() {
        let user = vec![
            AgentSpec {
                id: "claude".into(),
                name: "My Claude".into(),
                command: "claude2".into(),
                args: vec![],
                resume_args: vec![],
                new_args: Vec::new(),
                env: Default::default(),
                status: vec![],
            },
            AgentSpec {
                id: "custom".into(),
                name: "Custom".into(),
                command: "custom".into(),
                args: vec![],
                resume_args: vec![],
                new_args: Vec::new(),
                env: Default::default(),
                status: vec![],
            },
        ];
        let resolved = resolve_agents(&user);
        assert_eq!(resolved.len(), 6);
        assert_eq!(resolved[0].id, "pi", "the builtin order leads the list");
        // A user entry replaces its builtin *in place*, so the override sits
        // where the builtin it replaced was — not appended, and not first.
        assert_eq!(resolved[2].id, "claude");
        assert_eq!(resolved[2].command, "claude2");
        assert_eq!(resolved.last().unwrap().id, "custom");
    }
}
