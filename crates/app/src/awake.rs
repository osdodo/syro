//! Keeping the computer awake: one child process that holds the system's sleep
//! assertion, started and stopped as the setting and the running agents say.

use std::process::Child;
#[cfg(target_os = "macos")]
use std::process::{Command, Stdio};

/// The child that holds the sleep assertion, if one is running.
///
/// On macOS that child is `/usr/bin/caffeinate -i -s`, which prevents idle
/// *and* system sleep for as long as it runs; killing it gives the machine back
/// its normal sleep.
///
// shortcut: no Linux or Windows assertion — the app is bundled for macOS
// (`Cargo.toml`'s `[package.metadata.bundle]`), and a no-op is honest where
// there is nothing to hold. Add `systemd-inhibit`/`SetThreadExecutionState` when
// those builds exist.
pub struct AwakeAssertion {
    child: Option<Child>,
}

impl AwakeAssertion {
    pub fn new() -> Self {
        Self { child: None }
    }

    /// Whether the app is holding the machine awake now, which is what the
    /// settings form shows.
    pub fn is_active(&self) -> bool {
        self.child.is_some()
    }

    /// Hold the assertion while `active`, release it otherwise. A call that
    /// asks for what is already true does nothing.
    pub fn set(&mut self, active: bool) {
        if active == self.child.is_some() {
            return;
        }
        if active {
            self.start();
        } else {
            self.stop();
        }
    }

    #[cfg(target_os = "macos")]
    fn start(&mut self) {
        match Command::new("/usr/bin/caffeinate")
            .args(["-i", "-s"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(child) => self.child = Some(child),
            // A failure is not fatal and not sticky: the next reason to hold an
            // assertion starts again.
            Err(error) => tracing::warn!(%error, "could not start the sleep assertion"),
        }
    }

    #[cfg(not(target_os = "macos"))]
    fn start(&mut self) {}

    fn stop(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        let _ = child.kill();
        // Reaped, not just killed: a `Child` that is dropped without this leaves
        // a zombie behind.
        let _ = child.wait();
    }
}

/// Turn the display off now, leaving the machine running: with the assertion
/// held (`caffeinate -i -s`, no `-d`), a dark screen does not take the system
/// down with it. Off the UI thread, because `pmset` is a process to wait on.
#[cfg(target_os = "macos")]
pub fn sleep_display() {
    std::thread::spawn(|| {
        if let Err(error) = Command::new("/usr/bin/pmset")
            .arg("displaysleepnow")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
        {
            tracing::warn!(%error, "could not turn the display off");
        }
    });
}

impl Default for AwakeAssertion {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for AwakeAssertion {
    fn drop(&mut self) {
        // The machine sleeps again when the app goes away.
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::AwakeAssertion;

    /// Asking for what is already true is not a second child, and asking for the
    /// opposite starts one — and kills it.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_assertion_holds_one_child_at_a_time() {
        let mut awake = AwakeAssertion::new();
        assert!(!awake.is_active(), "a fresh assertion is not holding");
        awake.set(false);
        assert!(!awake.is_active());

        awake.set(true);
        assert!(awake.is_active(), "caffeinate did not start");
        let pid = awake.child.as_ref().unwrap().id();
        assert!(pid_is_alive(pid), "the caffeinate child is not running");
        awake.set(true);
        assert_eq!(awake.child.as_ref().unwrap().id(), pid, "a second child");

        // And the child is gone once it is released.
        awake.set(false);
        assert!(!awake.is_active());
        assert!(
            !pid_is_alive(pid),
            "the caffeinate child outlived the assertion"
        );
    }

    /// Whether a signal can still reach `pid`: a reaped child takes none. `kill
    /// -0` is the shell tool rather than a `libc` dependency for one assertion.
    #[cfg(target_os = "macos")]
    fn pid_is_alive(pid: u32) -> bool {
        std::process::Command::new("/bin/kill")
            .args(["-0", &pid.to_string()])
            .status()
            .is_ok_and(|status| status.success())
    }
}
