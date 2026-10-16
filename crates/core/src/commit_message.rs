//! Writing a commit message with the user's AI command: the prompt goes in on
//! the command's stdin, and what it prints is the message. No model lives here.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result};

/// The command the AI button runs when `settings.toml` names none. `pi` prints
/// instead of opening a TUI, saves no session, and needs no tools to read a diff.
pub const DEFAULT_COMMAND: &str = "pi -p --no-session --no-tools";

/// A message is written from the shape of a change, and past this the tail is
/// the same shape again. In chars, so a cut never lands mid-UTF-8.
const DIFF_LIMIT: usize = 60_000;

/// What the command reads: the instruction, then the diff.
pub fn prompt(diff: &str) -> String {
    let diff = if diff.chars().count() > DIFF_LIMIT {
        diff.chars().take(DIFF_LIMIT).collect::<String>()
    } else {
        diff.to_string()
    };
    format!(
        "Write a git commit message for the changes below. Start with a summary line \
         in the imperative mood, at most 72 characters, naming what changed rather \
         than how; add a blank line and a short body only when it carries something \
         the summary does not. Reply with the message and nothing else.\n\n{diff}\n"
    )
}

/// Run `command` in `root` with `input` on its stdin, and return what it printed.
///
/// Through the user's shell, and a *login, interactive* one: a version manager
/// puts its bin directory on the `PATH` in `.zshrc`, which a GUI app's
/// environment has never read — the same road the app takes to find an agent.
pub fn write(root: &Path, command: &str, input: &str) -> Result<String> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
    let mut child = Command::new(&shell)
        .args(["-ilc", command])
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("running {command}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        // A command that exits before reading its stdin is a broken pipe, not a
        // failure of its own: the exit status and what it printed are the answer.
        // Dropping `stdin` here is also what tells the command there is no more.
        let _ = stdin.write_all(input.as_bytes());
    }
    let output = child.wait_with_output()?;
    if !output.status.success() {
        anyhow::bail!(
            "{command} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_prompt_carries_the_instruction_and_the_diff() {
        let text = prompt("diff --git a/x b/x\n+hi\n");
        assert!(text.contains("commit message"), "{text}");
        assert!(text.contains("+hi"), "{text}");
    }

    #[test]
    fn a_diff_past_the_limit_is_cut() {
        let long = "x".repeat(DIFF_LIMIT + 100);
        assert_eq!(prompt(&long).matches('x').count(), DIFF_LIMIT);
    }

    /// The command's stdout is the message: `cat` hands the prompt straight back,
    /// which is the whole contract, without a model in the loop.
    #[test]
    fn what_the_command_prints_is_the_message() {
        let message = write(&std::env::temp_dir(), "cat", "hello\n").unwrap();
        assert_eq!(message, "hello");
    }

    #[test]
    fn a_command_that_fails_says_so() {
        let error = write(&std::env::temp_dir(), "echo boom >&2; exit 3", "").unwrap_err();
        assert!(format!("{error:#}").contains("boom"), "{error:#}");
    }
}
