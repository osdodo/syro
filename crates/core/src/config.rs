//! `settings.toml`: user configuration with per-section defaults.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::model::{AgentSpec, HookSpec};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TerminalSettings {
    #[serde(default = "default_scrollback")]
    pub scrollback: u32,
    #[serde(default = "default_font_size")]
    pub font_size: f32,
}

fn default_scrollback() -> u32 {
    5000
}

fn default_background_opacity() -> f32 {
    1.0
}

/// The blur radius the switch it replaced meant by `true`.
pub const DEFAULT_BLUR_RADIUS: u32 = 20;

/// A blur radius, or the on/off switch an older `settings.toml` spells it as.
fn blur_radius<'de, D: serde::Deserializer<'de>>(d: D) -> Result<u32, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Blur {
        On(bool),
        Radius(u32),
    }
    Ok(match Blur::deserialize(d)? {
        Blur::On(on) => {
            if on {
                DEFAULT_BLUR_RADIUS
            } else {
                0
            }
        }
        Blur::Radius(radius) => radius,
    })
}

fn default_font_size() -> f32 {
    13.0
}

impl Default for TerminalSettings {
    fn default() -> Self {
        Self {
            scrollback: 5000,
            font_size: 13.0,
        }
    }
}

/// What this app asks the system about sleeping while it runs. `settings.toml`
/// spells it `computer_awake = "on" | "auto" | "off"`.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ComputerAwakeMode {
    /// Never ask: the system sleeps as it normally would.
    #[default]
    Off,
    /// Ask only while an agent is working.
    Auto,
    /// Always ask.
    On,
}

/// Which palette the app draws in. `settings.toml` spells it
/// `appearance = "light" | "dark"`, or leaves it out to follow the system.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Appearance {
    Light,
    Dark,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Settings {
    #[serde(default)]
    pub terminal: TerminalSettings,
    /// Whether the app keeps the computer awake, and when. The app holds the
    /// assertion with a child process (see `awake::AwakeAssertion`); core only
    /// remembers the answer.
    #[serde(default)]
    pub computer_awake: ComputerAwakeMode,
    /// The palette, or absent for "follow the system".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub appearance: Option<Appearance>,
    /// How opaque the window is: 1.0 is solid, and lower lets what is behind it
    /// through the whole UI. The app clamps it (see
    /// `settings_view::clamp_opacity`), because a window at 0.05 is a window
    /// nobody can find the settings in.
    #[serde(default = "default_background_opacity")]
    pub background_opacity: f32,
    /// How much a see-through window blurs what is behind it, as a radius in
    /// points: 0 is no blur. An older file's `true` / `false` still reads.
    #[serde(default, deserialize_with = "blur_radius")]
    pub background_blur: u32,
    // The TOML spells these `[[agent]]` / `[[hook]]` (singular).
    #[serde(default, rename = "agent")]
    pub agents: Vec<AgentSpec>,
    #[serde(default, rename = "hook")]
    pub hooks: Vec<HookSpec>,
    #[serde(default)]
    pub keymap: BTreeMap<String, String>,
    /// The command the AI button runs to write a commit message: the prompt is
    /// on its stdin and its stdout is the message. Absent or blank is
    /// [`crate::commit_message::DEFAULT_COMMAND`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit_message_command: Option<String>,
    /// The UI language: `"en"`, `"zh-CN"`, or absent for "follow the system".
    /// A string rather than an enum because `syro-core` has no opinion about
    /// which languages the app ships; the app maps it (see `app::i18n`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            terminal: TerminalSettings::default(),
            computer_awake: ComputerAwakeMode::Off,
            appearance: None,
            background_opacity: 1.0,
            background_blur: 0,
            agents: Vec::new(),
            hooks: Vec::new(),
            keymap: BTreeMap::new(),
            commit_message_command: None,
            language: None,
        }
    }
}

impl Settings {
    /// Write the settings back to `path`.
    pub fn save_to(&self, path: &Path) -> anyhow::Result<()> {
        let text = toml::to_string_pretty(self)?;
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
        }
        let tmp = temp_sibling(path);
        std::fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
        // ponytail: no fsync before rename; a crash mid-write loses settings.toml
        // (which falls back to the defaults) but never corrupts it. Add fsync if
        // durability matters.
        std::fs::rename(&tmp, path)
            .with_context(|| format!("renaming {} -> {}", tmp.display(), path.display()))?;
        Ok(())
    }

    /// Load `settings.toml` from [`crate::settings_path`]; a missing file is the
    /// default, an unreadable one is an error, a malformed one is an error.
    pub fn load() -> anyhow::Result<Settings> {
        let text = match std::fs::read_to_string(crate::settings_path()) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Settings::default()),
            Err(e) => return Err(e.into()),
        };
        Ok(toml::from_str(&text)?)
    }
}

/// A unique sibling temp path: `settings.toml.tmp-<pid>`. The same convention
/// `store.rs` uses for `state.json`, spelled here rather than shared, because the
/// two never meet: each names the file it is about to replace.
fn temp_sibling(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "settings.toml".to_string());
    path.with_file_name(format!("{file_name}.tmp-{}", std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AgentSpec, AgentStatusRule, HookSpec};

    /// A scratch directory for the one test that writes a file.
    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "syro-settings-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn defaults() {
        let s = Settings::default();
        assert_eq!(s.terminal.scrollback, 5000);
        assert_eq!(s.terminal.font_size, 13.0);
        // A window with no transparency in the file is a solid window.
        assert_eq!(s.background_opacity, 1.0);
        // And a window nobody asked to frost is not frosted.
        assert_eq!(s.background_blur, 0);
        // And one that names no appearance follows the system's.
        assert_eq!(s.appearance, None);
        // A file that says nothing about sleeping leaves the machine alone.
        assert_eq!(s.computer_awake, ComputerAwakeMode::Off);
        assert!(s.agents.is_empty());
        assert!(s.hooks.is_empty());
        assert!(s.keymap.is_empty());
        // No command in the file means the AI button runs the default one.
        assert!(s.commit_message_command.is_none());
        // No `language` in the file means "follow the system".
        assert!(s.language.is_none());
    }

    /// The blur is a radius now, and a file from when it was a switch still reads.
    #[test]
    fn the_blur_reads_as_a_radius_or_the_old_switch() {
        let blur = |text: &str| toml::from_str::<Settings>(text).unwrap().background_blur;
        assert_eq!(blur("background_blur = 12"), 12);
        assert_eq!(blur("background_blur = true"), DEFAULT_BLUR_RADIUS);
        assert_eq!(blur("background_blur = false"), 0);
        assert_eq!(blur(""), 0);
    }

    #[test]
    fn the_appearance_is_light_dark_or_absent() {
        let read = |text: &str| toml::from_str::<Settings>(text).unwrap().appearance;
        assert_eq!(read(r#"appearance = "light""#), Some(Appearance::Light));
        assert_eq!(read(r#"appearance = "dark""#), Some(Appearance::Dark));
        assert_eq!(read(""), None);
    }

    #[test]
    fn parses_sample_toml() {
        let text = r#"
[terminal]
scrollback = 2000

[[agent]]
id = "claude"
name = "Claude Code"
command = "claude"
args = ["--dangerously-skip-permissions"]
status = [
  { match = '^✳( |$)', state = "idle" },
]

[[hook]]
on = "worktree_created"
run = "pnpm install"

[keymap]
"cmd-t" = "terminal::New"
"#;
        let s: Settings = toml::from_str(text).unwrap();
        assert_eq!(s.terminal.scrollback, 2000);
        assert_eq!(s.computer_awake, ComputerAwakeMode::Off);
        // font_size falls back to the per-field default.
        assert_eq!(s.terminal.font_size, 13.0);
        assert_eq!(s.agents.len(), 1);
        assert_eq!(s.agents[0].id, "claude");
        assert_eq!(s.agents[0].status.len(), 1);
        assert_eq!(s.agents[0].status[0].pattern, "^✳( |$)");
        assert_eq!(s.hooks[0].on, "worktree_created");
        assert_eq!(
            s.keymap.get("cmd-t").map(String::as_str),
            Some("terminal::New")
        );
    }

    /// Every section survives the round trip, which is the whole risk of writing
    /// a user's config back: their `[[agent]]` block is the thing that must not
    /// be dropped.
    #[test]
    fn save_round_trips_every_section_and_leaves_no_temp_file() {
        let dir = scratch("roundtrip");
        let path = dir.join("settings.toml");

        let mut settings = Settings::default();
        settings.terminal.scrollback = 2_000;
        settings.terminal.font_size = 15.0;
        settings.agents = vec![AgentSpec {
            id: "claude".into(),
            name: "My Claude".into(),
            command: "claude2".into(),
            args: vec!["--yolo".into()],
            resume_args: vec!["--resume".into(), "{id}".into()],
            new_args: Vec::new(),
            env: [("A".to_string(), "1".to_string())].into_iter().collect(),
            status: vec![AgentStatusRule {
                pattern: "^\u{2733}".into(),
                requires: None,
                state: crate::model::AgentStatus::Idle,
            }],
        }];
        settings.hooks = vec![HookSpec {
            on: "worktree_created".into(),
            run: "pnpm install".into(),
            terminal: true,
        }];
        settings
            .keymap
            .insert("cmd-t".into(), "terminal::New".into());
        settings.language = Some("zh-CN".into());
        settings.computer_awake = ComputerAwakeMode::Auto;

        settings.save_to(&path).unwrap();
        // Over an existing file as well: the second write is the one that has
        // something to replace.
        settings.save_to(&path).unwrap();

        let written = std::fs::read_to_string(&path).unwrap();
        let loaded: Settings = toml::from_str(&written).unwrap();
        assert_eq!(loaded, settings, "the file did not round-trip:\n{written}");
        // The sections are spelled the way the plan spells them, so a user's own
        // file keeps the shape they wrote.
        assert!(written.contains("[[agent]]"), "{written}");
        assert!(written.contains("[[hook]]"), "{written}");
        assert!(written.contains("[keymap]"), "{written}");

        let leftovers: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains(".tmp-"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "temp files left behind: {leftovers:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_toml_is_defaults() {
        let s: Settings = toml::from_str("").unwrap();
        assert_eq!(s, Settings::default());
    }

    /// Each mode is spelled the way a user would write it, and `on` is not read
    /// as `off` — the one mistake that would leave the machine sleeping.
    #[test]
    fn the_awake_mode_is_spelled_out() {
        for (text, expected) in [
            ("on", ComputerAwakeMode::On),
            ("auto", ComputerAwakeMode::Auto),
            ("off", ComputerAwakeMode::Off),
        ] {
            let s: Settings = toml::from_str(&format!("computer_awake = \"{text}\"")).unwrap();
            assert_eq!(s.computer_awake, expected);
        }
        assert!(toml::from_str::<Settings>("computer_awake = \"awake\"").is_err());
    }
}
