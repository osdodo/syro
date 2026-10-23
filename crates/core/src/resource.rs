//! What the app and its terminals cost: one host-wide process sweep.
//!
//! Unix reads `ps -eo pid=,ppid=,pcpu=,rss=,tpgid=`; Windows walks the Toolhelp
//! snapshot and asks each process for its times and working set.
//!
//! A terminal costs its whole process subtree: an agent spawns children, and their cost is the
//! terminal's. A pid already claimed by an earlier terminal is skipped, so two panes sharing a
//! supervisor do not count it twice.

#[cfg(unix)]
use std::process::Command;

#[cfg(unix)]
use anyhow::Context;
use anyhow::Result;

use std::collections::{HashMap, HashSet};

/// One row of the host's process table.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ProcessRow {
    pub pid: u32,
    pub ppid: u32,
    /// Percent of one core, as `ps` reports it.
    pub cpu_pct: f32,
    /// Resident set size in bytes.
    pub rss_bytes: u64,
    /// The process group the process's tty is giving the foreground to, `0` when
    /// it has no tty. `ps`'s `tpgid`: a shell at its prompt is its own
    /// foreground group, and a shell running a job is not.
    /// ponytail: always `0` on Windows, so rows never read `Running` there.
    pub tpgid: u32,
}

/// One sweep of the host's process table, indexed for tree walks.
#[derive(Debug, Clone, Default)]
pub struct ProcessTable {
    rows: Vec<ProcessRow>,
    by_pid: HashMap<u32, usize>,
    children_of: HashMap<u32, Vec<u32>>,
}

impl ProcessTable {
    /// Read the host's process table. One call, and the only I/O in this module.
    pub fn sample() -> Result<Self> {
        #[cfg(unix)]
        {
            let output = Command::new("ps")
                .args(["-eo", "pid=,ppid=,pcpu=,rss=,tpgid="])
                .output()
                .context("running ps for the resource readout")?;
            if !output.status.success() {
                anyhow::bail!(
                    "ps failed: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                );
            }
            Ok(Self::parse(&String::from_utf8_lossy(&output.stdout)))
        }

        #[cfg(windows)]
        {
            windows_sample()
        }
    }

    fn from_rows(rows: Vec<ProcessRow>) -> Self {
        let mut table = Self::default();
        for row in rows {
            let index = table.rows.len();
            table.by_pid.insert(row.pid, index);
            table.children_of.entry(row.ppid).or_default().push(row.pid);
            table.rows.push(row);
        }
        table
    }

    /// Parse `ps` output. A line that is not four numbers is skipped, not a failed sweep: a process
    /// can exit between the table being written and `ps` printing it.
    pub fn parse(text: &str) -> Self {
        let mut rows = Vec::new();
        for line in text.lines() {
            let mut fields = line.split_whitespace();
            let (Some(pid), Some(ppid), Some(cpu), Some(rss)) =
                (fields.next(), fields.next(), fields.next(), fields.next())
            else {
                continue;
            };
            let (Ok(pid), Ok(ppid)) = (pid.parse::<u32>(), ppid.parse::<u32>()) else {
                continue;
            };
            let cpu_pct = cpu.parse::<f32>().unwrap_or(0.0);
            // `ps` reports kilobytes on macOS and Linux alike.
            let rss_bytes = rss.parse::<u64>().unwrap_or(0) * 1024;
            // Absent from a table that does not carry it, and `-1` where the
            // platform has a tty it will not name: both are no tty.
            let tpgid = fields
                .next()
                .and_then(|tpgid| tpgid.parse::<u32>().ok())
                .unwrap_or(0);

            rows.push(ProcessRow {
                pid,
                ppid,
                tpgid,
                cpu_pct: if cpu_pct.is_finite() && cpu_pct > 0.0 {
                    cpu_pct
                } else {
                    0.0
                },
                rss_bytes,
            });
        }
        Self::from_rows(rows)
    }

    pub fn rows(&self) -> &[ProcessRow] {
        &self.rows
    }

    pub fn row(&self, pid: u32) -> Option<&ProcessRow> {
        self.by_pid.get(&pid).map(|index| &self.rows[*index])
    }

    /// Every descendant of `root` inclusive, minus `excluded`. Breadth-first, so a cycle would
    /// terminate rather than hang the refresh.
    pub fn subtree(&self, root: u32, excluded: &HashSet<u32>) -> Vec<u32> {
        let mut found = Vec::new();
        let mut seen = HashSet::new();
        let mut queue = vec![root];
        while let Some(pid) = queue.pop() {
            if !seen.insert(pid) || excluded.contains(&pid) {
                continue;
            }
            if self.by_pid.contains_key(&pid) {
                found.push(pid);
            }
            if let Some(children) = self.children_of.get(&pid) {
                queue.extend(children.iter().copied());
            }
        }
        found
    }

    /// Whether `pid` has handed its tty's foreground to something else, so a job
    /// is running in front of it. A shell at its prompt is its own foreground
    /// group, and a process with no tty is nobody's.
    pub fn has_a_foreground_job(&self, pid: u32) -> bool {
        self.row(pid)
            .is_some_and(|row| row.tpgid != 0 && row.tpgid != pid)
    }

    /// CPU percent and resident bytes summed over `pids`.
    pub fn usage_of(&self, pids: &[u32]) -> Usage {
        let mut usage = Usage::default();
        for pid in pids {
            if let Some(row) = self.row(*pid) {
                usage.cpu_pct += row.cpu_pct;
                usage.rss_bytes += row.rss_bytes;
                usage.processes += 1;
            }
        }
        usage
    }
}

/// One sweep from the Toolhelp process snapshot, with each process's lifetime CPU
/// average and working set. A process that cannot be opened (a protected one) is
/// still in the table — its pid and parent matter to the tree — just at zero cost.
#[cfg(windows)]
fn windows_sample() -> Result<ProcessTable> {
    use std::mem::{size_of, zeroed};
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };

    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error().into());
    }

    let mut rows = Vec::new();
    let mut entry: PROCESSENTRY32W = unsafe { zeroed() };
    entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;
    let mut more = unsafe { Process32FirstW(snapshot, &mut entry) } != 0;
    while more {
        rows.push(windows_row(entry.th32ProcessID, entry.th32ParentProcessID));
        more = unsafe { Process32NextW(snapshot, &mut entry) } != 0;
    }
    unsafe { CloseHandle(snapshot) };

    Ok(ProcessTable::from_rows(rows))
}

#[cfg(windows)]
fn windows_row(pid: u32, ppid: u32) -> ProcessRow {
    use std::mem::size_of;
    use std::time::{SystemTime, UNIX_EPOCH};
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME};
    use windows_sys::Win32::System::ProcessStatus::{
        K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    let mut row = ProcessRow {
        pid,
        ppid,
        cpu_pct: 0.0,
        rss_bytes: 0,
        tpgid: 0,
    };
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        return row;
    }

    let mut counters = PROCESS_MEMORY_COUNTERS::default();
    counters.cb = size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
    if unsafe { K32GetProcessMemoryInfo(process, &mut counters, counters.cb) } != 0 {
        row.rss_bytes = counters.WorkingSetSize as u64;
    }

    let mut created = FILETIME::default();
    let mut exited = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    if unsafe { GetProcessTimes(process, &mut created, &mut exited, &mut kernel, &mut user) } != 0 {
        // Lifetime average, which is what `ps` reports: CPU time over wall time.
        const FILETIME_UNIX_EPOCH_SECS: u64 = 11_644_473_600;
        let created_secs = filetime_ticks(created) / 10_000_000;
        let started = created_secs.saturating_sub(FILETIME_UNIX_EPOCH_SECS);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_secs())
            .unwrap_or(started);
        let wall = now.saturating_sub(started);
        let cpu = (filetime_ticks(kernel) + filetime_ticks(user)) as f64 / 10_000_000.0;
        if wall > 0 {
            row.cpu_pct = (cpu / wall as f64 * 100.0) as f32;
        }
    }
    unsafe { CloseHandle(process) };
    row
}

#[cfg(windows)]
fn filetime_ticks(time: windows_sys::Win32::Foundation::FILETIME) -> u64 {
    ((time.dwHighDateTime as u64) << 32) | time.dwLowDateTime as u64
}

/// What one thing costs: a terminal and everything it started, or the app.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Usage {
    pub cpu_pct: f32,
    pub rss_bytes: u64,
    /// How many processes the figure covers.
    pub processes: usize,
}

impl Usage {
    pub fn is_empty(&self) -> bool {
        self.processes == 0
    }
}

/// One terminal's cost, with the pid its tree hangs off.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TerminalUsage {
    pub pid: u32,
    pub usage: Usage,
    /// Something is running in front of the child this app started, in the same
    /// tty — a job in a shell. What a row reads as `Running` when the child
    /// itself reports nothing.
    pub busy: bool,
}

/// The whole readout: the app, each terminal, and their total.
#[derive(Debug, Clone, PartialEq)]
pub struct ResourceSnapshot {
    /// The app process alone, not its subtree — counting the subtree would count every terminal twice.
    pub app: Usage,
    /// One entry per live terminal, in the order the caller asked for them.
    pub terminals: Vec<TerminalUsage>,
    pub total: Usage,
}

impl ResourceSnapshot {
    /// Build the readout. The total is the app plus every terminal tree and nothing else: a pid
    /// that belongs to neither was not started by this app.
    pub fn of(table: &ProcessTable, app_pid: u32, terminal_pids: &[u32]) -> Self {
        let app = table
            .row(app_pid)
            .map(|row| Usage {
                cpu_pct: row.cpu_pct,
                rss_bytes: row.rss_bytes,
                processes: 1,
            })
            .unwrap_or_default();

        // A pid claimed by an earlier terminal is not counted again by a later
        // one, which is what keeps two panes sharing a supervisor honest.
        let mut claimed: HashSet<u32> = HashSet::new();
        claimed.insert(app_pid);

        let mut terminals = Vec::with_capacity(terminal_pids.len());
        for pid in terminal_pids {
            let pids = table.subtree(*pid, &claimed);
            claimed.extend(pids.iter().copied());
            terminals.push(TerminalUsage {
                pid: *pid,
                usage: table.usage_of(&pids),
                busy: table.has_a_foreground_job(*pid),
            });
        }

        let mut total = app;
        for terminal in &terminals {
            total.cpu_pct += terminal.usage.cpu_pct;
            total.rss_bytes += terminal.usage.rss_bytes;
            total.processes += terminal.usage.processes;
        }

        Self {
            app,
            terminals,
            total,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two terminals, one of which started children, and an unrelated process.
    const TABLE: &str = "\
    1     0  0.0  10240
    100   1  2.5  51200
    101 100  1.0  20480
    102 101  0.5  10240
    200   1  7.0  40960
    201 200  0.0  10240
    900   1  9.9 999999
";

    fn table() -> ProcessTable {
        ProcessTable::parse(TABLE)
    }

    #[test]
    fn a_row_is_pid_ppid_cpu_and_kilobytes_of_rss() {
        let table = table();
        let row = table.row(101).expect("pid 101");
        assert_eq!(row.ppid, 100);
        assert_eq!(row.cpu_pct, 1.0);
        assert_eq!(row.rss_bytes, 20 * 1024 * 1024);
    }

    #[test]
    fn a_line_that_is_not_a_row_is_skipped() {
        let table = ProcessTable::parse("garbage\n\n   \n12 1 0.0 1024\n1 x 0.0 1\n");
        assert_eq!(table.rows().len(), 1, "{:?}", table.rows());
        assert!(table.row(12).is_some());
    }

    #[test]
    fn a_terminal_costs_its_whole_tree() {
        let table = table();
        let snapshot = ResourceSnapshot::of(&table, 1, &[100, 200]);

        assert_eq!(snapshot.terminals[0].usage.processes, 3);
        assert_eq!(
            snapshot.terminals[0].usage.rss_bytes,
            (51200 + 20480 + 10240) * 1024
        );
        assert_eq!(snapshot.terminals[0].usage.cpu_pct, 4.0);
        assert_eq!(snapshot.terminals[1].usage.processes, 2);

        // Not its subtree: that would count every terminal twice.
        assert_eq!(snapshot.app.processes, 1);
        assert_eq!(snapshot.app.rss_bytes, 10240 * 1024);

        // pid 900 belongs to no terminal and not to the app, so it is not in the total.
        assert_eq!(snapshot.total.processes, 6);
        assert_eq!(
            snapshot.total.rss_bytes,
            (10240 + 51200 + 20480 + 10240 + 40960 + 10240) * 1024
        );
    }

    #[test]
    fn two_terminals_sharing_a_supervisor_do_not_count_it_twice() {
        let table = table();
        // 100 and 101 both named: the second one's tree is what is left of it.
        let snapshot = ResourceSnapshot::of(&table, 1, &[100, 101]);
        assert_eq!(snapshot.terminals[0].usage.processes, 3);
        assert_eq!(snapshot.terminals[1].usage.processes, 0);
        assert_eq!(snapshot.total.processes, 4, "app + 100, 101, 102");
    }

    #[test]
    fn a_terminal_that_has_already_exited_is_empty_not_a_panic() {
        let table = table();
        let snapshot = ResourceSnapshot::of(&table, 1, &[777]);
        assert!(snapshot.terminals[0].usage.is_empty());
        assert_eq!(snapshot.total.processes, 1);
    }

    /// A shell whose tty belongs to something else has a job in the foreground;
    /// one at its own prompt, and one with no tty at all, does not.
    #[test]
    fn a_foreground_job_is_read_from_the_tty() {
        let table = ProcessTable::parse(
            "\
    1 0 0.0 1024 0
    10 1 0.0 51200 11
    11 10 1.0 20480 11
    20 1 0.0 10240 20
    30 1 0.0 10240 0
",
        );
        assert!(table.has_a_foreground_job(10), "11 is in front of 10");
        assert!(!table.has_a_foreground_job(11), "11 is the foreground");
        assert!(!table.has_a_foreground_job(20), "20 is at its own prompt");
        assert!(!table.has_a_foreground_job(30), "30 has no tty");
        assert!(!table.has_a_foreground_job(999), "999 is not in the table");

        // And it is what a terminal is handed, beside what it costs.
        let snapshot = ResourceSnapshot::of(&table, 1, &[10, 20]);
        assert!(snapshot.terminals[0].busy);
        assert!(!snapshot.terminals[1].busy);
        assert_eq!(
            snapshot.terminals[0].usage.rss_bytes,
            (51200 + 20480) * 1024
        );
    }

    /// A table that does not carry the column reads as no tty, so nobody busy.
    #[test]
    fn a_sweep_without_the_column_is_read_as_no_tty() {
        assert_eq!(table().row(100).expect("pid 100").tpgid, 0);
        assert!(!table().has_a_foreground_job(100));
    }

    /// The one test that runs `ps`: what can still be wrong is the invocation itself.
    #[test]
    fn a_real_sweep_sees_this_test_process() {
        let table = ProcessTable::sample().expect("ps");
        let me = std::process::id();
        let row = table.row(me).expect("this test process is in the table");
        assert!(row.rss_bytes > 0, "a running process has resident bytes");
        assert!(row.ppid > 0, "something started this test");
        // And the walk finds it under its own parent.
        assert!(table.subtree(row.ppid, &HashSet::new()).contains(&me));
    }

    #[test]
    fn a_tree_with_a_cycle_terminates() {
        let table = ProcessTable::parse("1 2 0.0 1\n2 1 0.0 1\n");
        let subtree = table.subtree(1, &HashSet::new());
        assert_eq!(subtree.len(), 2);
    }
}
