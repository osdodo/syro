//! git CLI wrapper + porcelain/diff parsing. Everything shells out to `git` (no git2/gix).

use std::collections::HashSet;
use std::fs;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use anyhow::{Context, Result};

use rust_i18n::t;

/// Clone `url` into `destination`. The parent directory is made first, which `git clone` will not do.
pub fn clone(url: &str, destination: &Path) -> Result<String> {
    let url = url.trim();
    if url.is_empty() {
        anyhow::bail!(t!("git.clone_needs_url").to_string());
    }
    let Some(name) = destination.file_name() else {
        anyhow::bail!(t!("git.not_a_clone_dir", path = destination.display()).to_string());
    };
    let parent = destination.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    // Run in the parent and name the leaf, so git's own refusal reports an existing destination.
    run_git_ok(parent, &["clone", url, &name.to_string_lossy()])
}

/// Make an empty project: the directory, and a repository on `main`.
pub fn init(destination: &Path) -> Result<()> {
    fs::create_dir_all(destination)
        .with_context(|| format!("creating {}", destination.display()))?;
    run_git_ok(destination, &["init", "-q", "-b", "main"])?;
    Ok(())
}

/// Run `git -C <dir> <args>` and capture the raw output (success or not).
fn run_git(dir: &Path, args: &[&str]) -> Result<Output> {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .with_context(|| format!("git -C {} {}", dir.display(), args.join(" ")))
}

/// Run git and require exit status 0; return stdout as a String.
fn run_git_ok(dir: &Path, args: &[&str]) -> Result<String> {
    let out = run_git(dir, args)?;
    if !out.status.success() {
        anyhow::bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// True when `path` is inside a git work tree (the main repo or any worktree).
pub fn is_git_repo(path: &Path) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["rev-parse", "--is-inside-work-tree"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// The root of the repository containing `path`, so any directory inside a
/// worktree (or a subdirectory of one) can be added as a repo.
pub fn repo_root(path: &Path) -> Result<PathBuf> {
    let out = run_git_ok(path, &["rev-parse", "--show-toplevel"])?;
    Ok(PathBuf::from(out.trim()))
}

/// One entry from `git worktree list --porcelain`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitWorktree {
    pub path: PathBuf,
    pub head: Option<String>,
    /// Full ref as printed, e.g. `refs/heads/main`. `None` when detached or bare.
    pub branch: Option<String>,
    pub bare: bool,
    pub detached: bool,
}

/// Parse `git worktree list --porcelain`; a detached HEAD or a bare repo has no branch.
// ponytail: a path containing a space is not representable in this format; none of ours do.
pub(crate) fn parse_worktrees_porcelain(text: &str) -> Vec<GitWorktree> {
    let mut out = Vec::new();
    let mut cur: Option<GitWorktree> = None;

    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("worktree ") {
            if let Some(w) = cur.take() {
                out.push(w);
            }
            cur = Some(GitWorktree {
                path: PathBuf::from(rest),
                head: None,
                branch: None,
                bare: false,
                detached: false,
            });
        } else if let Some(rest) = line.strip_prefix("HEAD ") {
            if let Some(w) = cur.as_mut() {
                w.head = Some(rest.to_string());
            }
        } else if let Some(rest) = line.strip_prefix("branch ") {
            if let Some(w) = cur.as_mut() {
                w.branch = Some(rest.to_string());
            }
        } else if line == "bare" {
            if let Some(w) = cur.as_mut() {
                w.bare = true;
            }
        } else if line == "detached" {
            if let Some(w) = cur.as_mut() {
                w.detached = true;
            }
        }
    }
    if let Some(w) = cur {
        out.push(w);
    }
    out
}

/// All worktrees of `repo_path` (which may be the main worktree or any of its
/// siblings — `git worktree list` resolves to the repo either way).
pub fn list_worktrees(repo_path: &Path) -> Result<Vec<GitWorktree>> {
    let text = run_git_ok(repo_path, &["worktree", "list", "--porcelain"])?;
    Ok(parse_worktrees_porcelain(&text))
}

/// The branch a new worktree forks from: the remote's HEAD, else a conventional trunk name, else `HEAD`.
pub fn default_base_ref(repo: &Path) -> Result<String> {
    if let Ok(out) = run_git(
        repo,
        &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
    ) {
        if out.status.success() {
            let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !name.is_empty() {
                let name = name.strip_prefix("origin/").unwrap_or(name.as_str());
                return Ok(name.to_string());
            }
        }
    }

    for candidate in ["main", "master"] {
        let ref_name = format!("refs/heads/{candidate}");
        let exists = run_git(repo, &["rev-parse", "--verify", "--quiet", &ref_name])
            .map(|out| out.status.success())
            .unwrap_or(false);
        if exists {
            return Ok(candidate.to_string());
        }
    }

    Ok("HEAD".to_string())
}

/// The branch name a worktree name implies: lowercased, runs outside `[a-z0-9]` collapsed to one `-`.
pub fn branch_from_name(name: &str) -> String {
    let mut slug = String::with_capacity(name.len());
    let mut separator = false;
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            if separator && !slug.is_empty() {
                slug.push('-');
            }
            separator = false;
            slug.push(ch.to_ascii_lowercase());
        } else {
            separator = true;
        }
    }

    if slug.is_empty() {
        "worktree".to_string()
    } else {
        slug
    }
}

/// Create a worktree at `path` on a new `branch` from `base`. An existing branch is checked out instead.
pub fn worktree_add(repo: &Path, path: &Path, branch: &str, base: &str) -> Result<()> {
    let path_arg = path.to_string_lossy().into_owned();
    let created = run_git(repo, &["worktree", "add", "-b", branch, &path_arg, base])?;
    if created.status.success() {
        return Ok(());
    }

    let reused = run_git(repo, &["worktree", "add", &path_arg, branch])?;
    if reused.status.success() {
        return Ok(());
    }

    anyhow::bail!(
        t!(
            "git.worktree_add_failed",
            path = path_arg,
            error = String::from_utf8_lossy(&created.stderr).trim()
        )
        .to_string()
    )
}

/// A shared path as a project stores it: relative to the checkout, without a
/// leading `./` or a trailing `/`. `None` for one that is empty or reaches out of
/// the checkout (absolute, or through `..`).
pub fn shared_path(path: &str) -> Option<String> {
    let path = path.trim();
    let path = path
        .strip_prefix("./")
        .unwrap_or(path)
        .trim_end_matches('/');
    let inside = !path.is_empty()
        && Path::new(path)
            .components()
            .all(|part| matches!(part, std::path::Component::Normal(_)));
    inside.then(|| path.to_string())
}

/// Bring each of `paths` from the main checkout into a new worktree: an APFS
/// clone (`cp -c`) where the filesystem can make one, a symlink to the main
/// checkout's copy otherwise. A path the checkout does not have, or one the
/// worktree already has (git checked it out), is left alone. What could not be
/// brought over comes back, one line per path, so a worktree is never refused
/// over a missing `.env`.
pub fn share_paths(main: &Path, worktree: &Path, paths: &[String]) -> Vec<String> {
    let mut failed = Vec::new();
    for path in paths {
        let Some(path) = shared_path(path) else {
            continue;
        };
        let (source, dest) = (main.join(&path), worktree.join(&path));
        if fs::symlink_metadata(&source).is_err() || fs::symlink_metadata(&dest).is_ok() {
            continue;
        }
        if let Err(error) = share_one(&source, &dest) {
            failed.push(format!("{path}: {error:#}"));
        }
    }
    failed
}

fn share_one(source: &Path, dest: &Path) -> Result<()> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    let cloned = Command::new("cp")
        .arg("-cR")
        .arg(source)
        .arg(dest)
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    if cloned {
        return Ok(());
    }
    // A clone that failed part way can leave a partial copy; it was not there
    // before (see the caller), so it is ours to clear.
    let _ = fs::remove_dir_all(dest).or_else(|_| fs::remove_file(dest));
    std::os::unix::fs::symlink(source, dest)
        .with_context(|| t!("git.share_failed", path = dest.display()).to_string())
}

/// Remove the worktree at `path`, and forget the bookkeeping a worktree deleted
/// out of band leaves behind.
pub fn worktree_remove(repo: &Path, path: &Path, force: bool) -> Result<()> {
    let path_arg = path.to_string_lossy().into_owned();
    if force {
        run_git_ok(repo, &["worktree", "remove", "--force", &path_arg])?;
    } else {
        run_git_ok(repo, &["worktree", "remove", &path_arg])?;
    }
    run_git_ok(repo, &["worktree", "prune"])?;
    Ok(())
}

/// Delete `branch`, refusing when it is unmerged unless `force` is set.
pub fn branch_delete(repo: &Path, branch: &str, force: bool) -> Result<()> {
    let flag = if force { "-D" } else { "-d" };
    run_git_ok(repo, &["branch", flag, branch])?;
    Ok(())
}

/// One matching line in a project file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextMatch {
    pub path: String,
    pub line: u32,
    pub column: u32,
    pub text: String,
}

/// Search readable, non-ignored files under `root` for a literal, case-insensitive query.
pub fn search_text(root: &Path, query: &str) -> Result<Vec<TextMatch>> {
    if query.trim().is_empty() {
        return Ok(Vec::new());
    }
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "grep",
            "--no-index",
            "--line-number",
            "--column",
            "--null",
            "-I",
            "-F",
            "-i",
            "--exclude-standard",
            "--max-count=3",
            "-e",
            query,
            "--",
            ".",
        ])
        .output()
        .with_context(|| format!("searching {}", root.display()))?;
    if !out.status.success() && out.status.code() != Some(1) {
        anyhow::bail!(
            "git grep failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let mut matches = Vec::new();
    let mut cursor = 0;
    while cursor < out.stdout.len() {
        let Some(path_end) = out.stdout[cursor..].iter().position(|byte| *byte == 0) else {
            break;
        };
        let path = &out.stdout[cursor..cursor + path_end];
        cursor += path_end + 1;
        let Some(line_end) = out.stdout[cursor..].iter().position(|byte| *byte == 0) else {
            break;
        };
        let line = std::str::from_utf8(&out.stdout[cursor..cursor + line_end])
            .unwrap_or_default()
            .parse();
        cursor += line_end + 1;
        let Some(column_end) = out.stdout[cursor..].iter().position(|byte| *byte == 0) else {
            break;
        };
        let column = std::str::from_utf8(&out.stdout[cursor..cursor + column_end])
            .unwrap_or_default()
            .parse();
        cursor += column_end + 1;
        let text_end = out.stdout[cursor..]
            .iter()
            .position(|byte| *byte == b'\n')
            .unwrap_or(out.stdout.len() - cursor);
        let text = &out.stdout[cursor..cursor + text_end];
        cursor += text_end + usize::from(cursor + text_end < out.stdout.len());
        let (Ok(line), Ok(column)) = (line, column) else {
            continue;
        };
        matches.push(TextMatch {
            path: String::from_utf8_lossy(path).into_owned(),
            line,
            column,
            text: String::from_utf8_lossy(text)
                .trim_end_matches('\r')
                .to_string(),
        });
        if matches.len() == 200 {
            break;
        }
    }
    Ok(matches)
}

/// Every tracked file in `root`, relative to it — the quick-open list.
pub fn ls_files(root: &Path) -> Result<Vec<String>> {
    let text = run_git_ok(root, &["ls-files", "--cached"])?;
    Ok(text.lines().map(str::to_string).collect())
}

/// One changed file's unified diff against `base_ref`. An untracked file is diffed against
/// `/dev/null`, where `--no-index` exits 1 and that is the answer rather than a failure.
pub fn diff_text(root: &Path, base_ref: &str, change: &FileChange) -> Result<String> {
    let path = change.path.as_str();
    let args: Vec<&str> = match change.kind {
        ChangeKind::Committed => {
            vec!["diff", "--no-color", base_ref, "HEAD", "--", path]
        }
        ChangeKind::Staged => vec!["diff", "--no-color", "--cached", "--", path],
        ChangeKind::Unstaged => vec!["diff", "--no-color", "--", path],
    };
    let tracked = run_git(root, &args)?;
    let text = String::from_utf8_lossy(&tracked.stdout).into_owned();
    if !text.trim().is_empty() || change.kind != ChangeKind::Unstaged || change.status != '?' {
        return Ok(text);
    }

    let absolute = root.join(path);
    if absolute.is_dir() {
        // An untracked directory that git will not open up even under
        // `--untracked-files=all` — a nested repository — has no file to diff,
        // and `--no-index` on a directory instead looks for a `null` inside it.
        return Ok(String::new());
    }
    let untracked = run_git(
        root,
        &[
            "diff",
            "--no-color",
            "--no-index",
            "--",
            "/dev/null",
            &absolute.to_string_lossy(),
        ],
    )?;
    let text = String::from_utf8_lossy(&untracked.stdout).into_owned();
    if text.trim().is_empty() && !untracked.status.success() {
        anyhow::bail!(
            "git diff {} failed: {}",
            path,
            String::from_utf8_lossy(&untracked.stderr).trim()
        );
    }
    Ok(text)
}

/// What the uncommitted work amounts to, as one diff: the files a `git add -A`
/// would stage, and the files git holds in no commit at all. What the AI writes
/// a message from, so it is one call and not one per file.
pub fn commit_diff(root: &Path) -> Result<String> {
    let mut text = String::new();
    // `HEAD` covers staged and unstaged alike. An unborn `HEAD` — a repository
    // with no commits yet — has no such revision to compare against, so the two
    // halves are asked for separately.
    match run_git(root, &["diff", "--no-color", "HEAD"]) {
        Ok(out) if out.status.success() => text.push_str(&String::from_utf8_lossy(&out.stdout)),
        _ => {
            text.push_str(&run_git_ok(root, &["diff", "--no-color", "--cached"])?);
            text.push_str(&run_git_ok(root, &["diff", "--no-color"])?);
        }
    }
    for change in worktree_changes(root)? {
        if change.status == '?'
            && let Ok(diff) = diff_text(root, "HEAD", &change)
        {
            text.push_str(&diff);
        }
    }
    Ok(text)
}

/// One changed file from `git status --porcelain=v1` or `git diff --name-status`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChange {
    pub path: String,
    /// Porcelain status char: M/A/D/R/C/?
    pub status: char,
    /// One file's change within a particular comparison.
    pub kind: ChangeKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ChangeKind {
    Committed,
    Staged,
    Unstaged,
}

/// Split a porcelain v1 rename field (`old -> new`) into `(destination, old)`.
fn split_rename_path(field: &str) -> (String, Option<String>) {
    if let Some(idx) = field.find(" -> ") {
        (field[idx + 4..].to_string(), Some(field[..idx].to_string()))
    } else {
        (field.to_string(), None)
    }
}

/// Parse one `git diff --name-status` line (`M\tpath`, `R100\told\tnew`).
/// Renames/copies keep the destination path.
fn parse_name_status(line: &str) -> Option<(char, String)> {
    let mut parts = line.split('\t');
    let status_field = parts.next()?;
    let status = status_field.chars().next()?;
    let first = parts.next()?.to_string();
    if status == 'R' || status == 'C' {
        if let Some(dest) = parts.next() {
            return Some((status, dest.to_string()));
        }
    }
    Some((status, first))
}

/// What the source control header shows: the branch, its upstream, and the ahead/behind counts.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BranchStatus {
    /// `None` on a detached HEAD.
    pub branch: Option<String>,
    /// The remote-tracking branch the local one is set to follow, if any.
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
}

impl BranchStatus {
    /// What the branch is called on screen: git's own word for a detached HEAD
    /// when there is no name to show.
    pub fn name(&self) -> &str {
        self.branch.as_deref().unwrap_or("detached HEAD")
    }

    /// The two counts in reading order: not upstream yet, then upstream not here.
    pub fn summary(&self) -> String {
        match (&self.upstream, self.ahead, self.behind) {
            (Some(upstream), 0, 0) => t!("git.no_changes_ahead", upstream = upstream).to_string(),
            (Some(upstream), ahead, 0) => {
                t!("git.ahead", upstream = upstream, ahead = ahead).to_string()
            }
            (Some(upstream), 0, behind) => {
                t!("git.behind", upstream = upstream, behind = behind).to_string()
            }
            (Some(upstream), ahead, behind) => t!(
                "git.ahead_behind",
                upstream = upstream,
                ahead = ahead,
                behind = behind
            )
            .to_string(),
            (None, ..) => t!("git.no_upstream").to_string(),
        }
    }
}

/// The branch on screen and its upstream, from one `git status --porcelain=v2 --branch` call.
pub fn branch_status(root: &Path) -> Result<BranchStatus> {
    let text = run_git_ok(root, &["status", "--porcelain=v2", "--branch"])?;
    Ok(parse_branch_status(&text))
}

/// Parse the `# branch.*` headers of `git status --porcelain=v2 --branch`.
pub fn parse_branch_status(text: &str) -> BranchStatus {
    let mut status = BranchStatus::default();
    for line in text.lines() {
        let Some(header) = line.strip_prefix("# ") else {
            continue;
        };
        let mut parts = header.split_whitespace();
        match parts.next() {
            Some("branch.head") => {
                let name = parts.next().unwrap_or_default();
                status.branch =
                    (name != "(detached)" && !name.is_empty()).then(|| name.to_string());
            }
            Some("branch.upstream") => {
                status.upstream = parts.next().map(str::to_string);
            }
            Some("branch.ab") => {
                // `+2 -1`, either sign possible, either count absent when zero.
                for field in parts {
                    let (sign, count) = field.split_at(1);
                    let count: u32 = count.parse().unwrap_or(0);
                    match sign {
                        "+" => status.ahead = count,
                        "-" => status.behind = count,
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    status
}

/// One commit, as the source control panel's history lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commit {
    pub id: String,
    pub short: String,
    pub summary: String,
    pub author: String,
    /// When the author wrote it, in epoch seconds. The timestamp rather than
    /// git's own relative date, which is English ("2 hours ago") whatever the
    /// app is speaking: the panel says the age in the reader's language.
    pub at: i64,
    /// The names pointing at this commit, cleaned up for a badge.
    pub refs: Vec<String>,
    /// The ids of its parents: one for a commit, more than one for a merge, and
    /// none for the first commit in the repository. What a graph draws its lines
    /// from.
    pub parents: Vec<String>,
}

/// What [`commits`] and [`graph_commits`] ask git for: the id, its abbreviation,
/// the subject, the author, the time, the parents every graph needs, and `%D`'s
/// names — separated by `\u{1f}`, which no commit subject contains.
const LOG_FORMAT: &str = "--pretty=format:%H%x1f%h%x1f%s%x1f%an%x1f%at%x1f%P%x1f%D";

/// The newest `limit` commits on the current branch.
pub fn commits(root: &Path, limit: usize) -> Result<Vec<Commit>> {
    let out = run_git_ok(root, &["log", &format!("--max-count={limit}"), LOG_FORMAT])?;
    Ok(parse_log(&out))
}

/// Every commit any ref reaches, newest first, in the one order a graph can be
/// drawn from: `--topo-order`, where a commit's parents all come after it.
/// `--all` is what puts the other branches, and the remotes, in the picture.
pub fn graph_commits(root: &Path, limit: usize) -> Result<Vec<Commit>> {
    let out = run_git_ok(
        root,
        &[
            "log",
            "--all",
            "--topo-order",
            &format!("--max-count={limit}"),
            LOG_FORMAT,
        ],
    )?;
    Ok(parse_log(&out))
}

/// The files one commit changed, against the parent it was made on. A merge
/// reports one line per file it settled, which is the change a reader looking at
/// a merge is asking about.
pub fn commit_files(root: &Path, id: &str) -> Result<Vec<FileChange>> {
    // `--format=` keeps the message out of the answer: the status lines are all
    // of it, and a message that began with "M\t" would be read as a file.
    let text = run_git_ok(root, &["show", "--format=", "--name-status", id])?;
    Ok(text
        .lines()
        .filter_map(parse_name_status)
        .map(|(status, path)| FileChange {
            path,
            status,
            kind: ChangeKind::Committed,
        })
        .collect())
}

/// Where a remote-tracking branch lives: which remote, and what the branch is
/// called *on* it. A remote branch's own name is the last segment of a
/// `refs/remotes` ref, and it may have slashes of its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteRef {
    /// The remote's name: `origin`.
    pub remote: String,
    /// The branch's name on it, without the remote's prefix: `feat/solana` for
    /// `origin/feat/solana`.
    pub branch: String,
}

/// One branch in the repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Branch {
    /// The short name: `main`, or `origin/main` for a remote-tracking branch.
    pub name: String,
    /// The commit it points at.
    pub commit: String,
    pub short: String,
    /// True for the branch `HEAD` is on, which is the one that cannot be deleted.
    pub current: bool,
    /// Where this branch lives, when it lives on a remote rather than in this
    /// checkout. What a delete of it has to name: a remote branch is deleted by
    /// pushing the deletion, and this is what says to which remote and which
    /// branch there.
    pub remote: Option<RemoteRef>,
    /// The remote-tracking branch a local one follows, if any.
    pub upstream: Option<String>,
}

/// Every branch in the repository: the local ones first, then the remote-tracking
/// ones, each group by name. `origin/HEAD` is a symbol pointing at another
/// branch, not a branch, so it is not one of them.
pub fn branches(root: &Path) -> Result<Vec<Branch>> {
    // `%09` and not `%1f`: for-each-ref's own escaping only knows `%xx` hex, and a
    // tab is the one byte neither a ref name nor an upstream can hold.
    let format = "--format=%(refname)%09%(refname:short)%09%(objectname)%09%(objectname:short)%09%(HEAD)%09%(upstream:short)%09%(symref)";
    let text = run_git_ok(
        root,
        &[
            "for-each-ref",
            format,
            "--sort=refname",
            "refs/heads",
            "refs/remotes",
        ],
    )?;
    let mut out: Vec<Branch> = Vec::new();
    for line in text.lines() {
        let fields: Vec<&str> = line.split('\t').collect();
        if fields.len() < 7 {
            continue;
        }
        let (full, name, id, short, head, upstream, symref) = (
            fields[0], fields[1], fields[2], fields[3], fields[4], fields[5], fields[6],
        );
        if !symref.is_empty() || name.is_empty() {
            continue;
        }
        out.push(Branch {
            name: name.to_string(),
            commit: id.to_string(),
            short: short.to_string(),
            current: head == "*",
            remote: full.strip_prefix("refs/remotes/").and_then(|rest| {
                // The remote's name is the first segment; everything after it is
                // the branch's name on that remote.
                let (remote, branch) = rest.split_once('/')?;
                (!branch.is_empty()).then(|| RemoteRef {
                    remote: remote.to_string(),
                    branch: branch.to_string(),
                })
            }),
            upstream: (!upstream.is_empty()).then(|| upstream.to_string()),
        });
    }
    out.sort_by_key(|branch| (branch.remote.is_some(), branch.name.clone()));
    Ok(out)
}

/// One file's unified diff, as one commit changed it. The worktree's own diff is
/// [`diff_text`]; this is the other comparison, and it is what a click on a file
/// under a commit row asks for.
pub fn commit_file_diff(root: &Path, id: &str, path: &str) -> Result<String> {
    // `--format=` keeps the commit's message and its own header out of the answer.
    run_git_ok(root, &["show", "--no-color", "--format=", id, "--", path])
}

/// Parse `git log --pretty=format:…`; `%D` is git's ref decoration, minus its `HEAD -> ` / `tag: ` syntax.
pub fn parse_log(text: &str) -> Vec<Commit> {
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| {
            let fields: Vec<&str> = line.split('\u{1f}').collect();
            // The five fields every line has, then the parents — empty only for
            // the first commit in a repository — then the refs.
            if fields.len() < 6 {
                return None;
            }
            let (id, short, summary, author, at) =
                (fields[0], fields[1], fields[2], fields[3], fields[4]);
            let parents: Vec<String> = fields[5].split_whitespace().map(str::to_string).collect();
            let refs = fields
                .get(6)
                .copied()
                .unwrap_or_default()
                .split(',')
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(|name| {
                    name.strip_prefix("HEAD -> ")
                        .or_else(|| name.strip_prefix("tag: "))
                        .unwrap_or(name)
                        .to_string()
                })
                .collect();
            Some(Commit {
                id: id.to_string(),
                short: short.to_string(),
                summary: summary.to_string(),
                author: author.to_string(),
                // A date git could not print is a date no panel can render;
                // zero is the epoch, which reads as "a long time ago".
                at: at.trim().parse().unwrap_or(0),
                refs,
                parents,
            })
        })
        .collect()
}

/// The uncommitted files of `worktree_path`, which is also the answer to "is there anything to commit".
pub fn worktree_changes(worktree_path: &Path) -> Result<Vec<FileChange>> {
    let mut out: Vec<FileChange> = Vec::new();
    // Every untracked file by name, so a new directory is rows a reader can open
    // and diff rather than one row git would refuse to diff.
    let status = run_git_ok(
        worktree_path,
        &["status", "--porcelain=v1", "--untracked-files=all"],
    )?;
    for line in status.lines() {
        if line.len() < 4 {
            continue;
        }
        let b = line.as_bytes();
        let x = b[0] as char;
        let y = b[1] as char;
        let (path, _old) = split_rename_path(&line[3..]);
        if x != ' ' && x != '?' {
            out.push(FileChange {
                path: path.clone(),
                status: x,
                kind: ChangeKind::Staged,
            });
        }
        if y != ' ' || x == '?' {
            out.push(FileChange {
                path,
                status: if x == '?' { '?' } else { y },
                kind: ChangeKind::Unstaged,
            });
        }
    }
    Ok(out)
}

/// Remove an unstaged change without touching the index. Untracked paths are removed directly;
/// tracked paths are restored from the index, preserving any staged content.
pub fn discard_unstaged(worktree_path: &Path, change: &FileChange) -> Result<()> {
    if change.kind != ChangeKind::Unstaged {
        anyhow::bail!("only unstaged changes can be discarded");
    }
    if change.status == '?' {
        let path = worktree_path.join(&change.path);
        let metadata =
            fs::symlink_metadata(&path).with_context(|| format!("reading {}", path.display()))?;
        if metadata.file_type().is_dir() {
            fs::remove_dir_all(&path).with_context(|| format!("removing {}", path.display()))?;
        } else {
            fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
        }
        return Ok(());
    }
    run_git_ok(
        worktree_path,
        &["restore", "--worktree", "--", &change.path],
    )?;
    Ok(())
}

/// Move a staged path back to the worktree without changing its contents.
pub fn unstage(worktree_path: &Path, change: &FileChange) -> Result<()> {
    if change.kind != ChangeKind::Staged {
        anyhow::bail!("only staged changes can be unstaged");
    }
    run_git_ok(worktree_path, &["restore", "--staged", "--", &change.path])?;
    Ok(())
}

/// `(added, deleted)` lines per comparison and path; binary files count as zeros.
/// Untracked files are counted separately because git diff does not list them.
pub fn diff_stat(
    worktree_path: &Path,
    base_ref: &str,
    changes: &[FileChange],
) -> Result<std::collections::BTreeMap<(ChangeKind, String), (u32, u32)>> {
    let mut stats = std::collections::BTreeMap::new();
    for (kind, args) in [
        (
            ChangeKind::Committed,
            vec!["diff", "--numstat", base_ref, "HEAD"],
        ),
        (ChangeKind::Staged, vec!["diff", "--numstat", "--cached"]),
        (ChangeKind::Unstaged, vec!["diff", "--numstat"]),
    ] {
        let text = run_git_ok(worktree_path, &args)?;
        for line in text.lines() {
            let mut fields = line.split('\t');
            let (Some(added), Some(deleted), Some(path)) =
                (fields.next(), fields.next(), fields.next())
            else {
                continue;
            };
            stats.insert(
                (kind, path.to_string()),
                (added.parse().unwrap_or(0), deleted.parse().unwrap_or(0)),
            );
        }
    }
    for change in changes
        .iter()
        .filter(|change| change.kind == ChangeKind::Unstaged && change.status == '?')
    {
        if let Some(counts) = untracked_lines(&worktree_path.join(&change.path)) {
            stats.insert((change.kind, change.path.clone()), counts);
        }
    }
    Ok(stats)
}

/// `(added, deleted)` for a file no commit holds: its own lines, which is all a commit would
/// add. `None` when there is nothing to count — a binary file, or a directory
/// `--untracked-files=all` will not open (a nested repository) — where git's own `--numstat`
/// says `-` too.
fn untracked_lines(path: &Path) -> Option<(u32, u32)> {
    let mut reader = std::io::BufReader::new(fs::File::open(path).ok()?);
    let mut lines = 0u32;
    // An empty file is the one case with no byte to look at, and it has no last
    // line to count.
    let mut ends_with_newline = true;
    loop {
        // A chunk at a time: the file is whatever the reader left lying in a
        // repository, and reading all of it in to count line breaks is a spike
        // nobody asked for.
        let buf = reader.fill_buf().ok()?;
        if buf.is_empty() {
            break;
        }
        if buf.contains(&0) {
            return None;
        }
        lines += buf.iter().filter(|byte| **byte == b'\n').count() as u32;
        ends_with_newline = buf.last() == Some(&b'\n');
        let filled = buf.len();
        reader.consume(filled);
    }
    Some((lines + u32::from(!ends_with_newline), 0))
}

/// `git add -A` then commit, which is the contract of a panel with one button. A blank message
/// commits nothing.
pub fn commit_all(worktree_path: &Path, message: &str) -> Result<String> {
    if message.trim().is_empty() {
        anyhow::bail!("a commit needs a message");
    }
    run_git_ok(worktree_path, &["add", "-A"])?;
    run_git_ok(worktree_path, &["commit", "-m", message])?;
    run_git_ok(worktree_path, &["log", "-1", "--pretty=%s"])
}

/// Push the current branch. `publish` adds `-u origin HEAD` for a branch with no upstream yet.
pub fn push(worktree_path: &Path, publish: bool) -> Result<String> {
    if publish {
        run_git_reporting(worktree_path, &["push", "-u", "origin", "HEAD"])
    } else {
        run_git_reporting(worktree_path, &["push"])
    }
}

/// Switch this checkout to `branch`. git decides whether the working tree is
/// clean enough — a change the switch would overwrite is its own refusal, and
/// the caller shows it as git wrote it.
pub fn checkout(root: &Path, branch: &str) -> Result<String> {
    run_git_reporting(root, &["checkout", branch])
}

/// Delete a branch on a remote: `git push <remote> --delete <branch>`. The one
/// delete that changes something on somebody else's machine, which is why the
/// question asked before it says so.
pub fn remote_branch_delete(root: &Path, remote: &str, branch: &str) -> Result<String> {
    let refname = format!("refs/remotes/{remote}/{branch}");
    // A ref under `refs/remotes` whose first segment is not a remote at all — the
    // shape `git fetch origin pull/12/head:pr/12` leaves behind — is local. There
    // is no remote to push a deletion to, and no remote's refspec owns the ref,
    // so no fetch would ever prune it: dropping it is the whole delete.
    if !remotes(root)?.iter().any(|name| name == remote) {
        run_git_ok(root, &["update-ref", "-d", &refname])?;
        return Ok(format!(
            "{remote}/{branch} was a local ref, not a branch on a remote — dropped it"
        ));
    }
    let args = ["push", remote, "--delete", branch];
    let out = run_git(root, &args)?;
    let said = said(&out);
    if out.status.success() {
        return Ok(said);
    }
    // Nothing left on the remote to delete, and a remote-tracking ref here that
    // says otherwise: the branch went — a merged PR, another machine — and this
    // checkout never pruned. git cannot delete what is not there, so the stale
    // ref is the thing to drop; left alone it is a branch the graph keeps
    // drawing, and a delete that fails the same way every time it is asked.
    if said.contains("remote ref does not exist") {
        run_git_ok(root, &["update-ref", "-d", &refname])?;
        return Ok(format!("{branch} was already gone from {remote}"));
    }
    anyhow::bail!("git {} failed: {said}", args.join(" "));
}

/// The configured remotes' names.
fn remotes(root: &Path) -> Result<Vec<String>> {
    Ok(run_git_ok(root, &["remote"])?
        .lines()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .collect())
}

/// The page the repository can be opened at in a browser: the `origin` remote,
/// with the ssh forms turned into the `https://` a browser takes. `None` when
/// there is no remote, or when what it holds is not a URL at all (a local path
/// is a remote, but it has no page).
pub fn remote_url(root: &Path) -> Option<String> {
    let raw = run_git_ok(root, &["remote", "get-url", "origin"]).ok()?;
    let raw = raw.trim();
    // `git@github.com:owner/repo.git` and `ssh://git@github.com/owner/repo.git`
    // name the same page as the https URL; only the punctuation differs.
    let http = if let Some(rest) = raw.strip_prefix("git@") {
        format!("https://{}", rest.replacen(':', "/", 1))
    } else if let Some(rest) = raw.strip_prefix("ssh://git@") {
        format!("https://{rest}")
    } else {
        raw.to_string()
    };
    let url = http.trim_end_matches('/');
    let url = url.strip_suffix(".git").unwrap_or(url);
    url.starts_with("https://").then(|| url.to_string())
}

/// Fetch every remote, and drop the remote-tracking refs whose branch is gone
/// from it. `--prune` is the point of fetching in a window like this one: a
/// branch somebody deleted upstream is a branch the graph should stop drawing.
pub fn fetch(root: &Path) -> Result<String> {
    run_git_reporting(root, &["fetch", "--all", "--prune"])
}

/// Pull the current branch — and only when it fast-forwards. A plain `git pull`
/// would merge on a branch that has moved on both ends, and that is a merge
/// commit (or a conflicted worktree) nobody in this window asked for: git's own
/// refusal says which it was, and the terminal is where a merge is decided.
pub fn pull(root: &Path) -> Result<String> {
    run_git_reporting(root, &["pull", "--ff-only"])
}

/// Run git and hand back what it said on either stream, one line at a time.
///
/// The network commands — fetch, pull, push — write their summary to *stderr*,
/// where `run_git_ok` never looks: a notice read from stdout alone would say
/// nothing at all about the one thing the reader just did.
fn run_git_reporting(dir: &Path, args: &[&str]) -> Result<String> {
    let out = run_git(dir, args)?;
    let said = said(&out);
    if !out.status.success() {
        anyhow::bail!("git {} failed: {said}", args.join(" "));
    }
    Ok(said)
}

/// What git said, both streams as one line.
fn said(out: &Output) -> String {
    // Progress lines arrive on one rewriting line and end in `\r`, so both
    // separators are line breaks here, and the blank lines between git's blocks
    // are not worth keeping.
    format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
    .lines()
    .map(str::trim)
    .filter(|line| !line.is_empty())
    .collect::<Vec<_>>()
    .join(" · ")
}

/// Changed files grouped by provenance: branch commits, staged, and unstaged work.
pub fn changed_files(worktree_path: &Path, base_ref: &str) -> Result<Vec<FileChange>> {
    let mut out = worktree_changes(worktree_path)?;
    let diff = run_git_ok(worktree_path, &["diff", "--name-status", base_ref, "HEAD"])?;
    for line in diff.lines() {
        if let Some((status_char, path)) = parse_name_status(line) {
            out.push(FileChange {
                path,
                status: status_char,
                kind: ChangeKind::Committed,
            });
        }
    }
    Ok(out)
}

/// Whether a worktree has `branch` checked out, and the directory that holds it.
///
/// `git branch -d` refuses such a branch and `-D` refuses it too — git will not
/// leave a checkout on a branch that does not exist. So the only way such a
/// branch goes is with the worktree holding it, and this is what says which one
/// that is.
pub fn branch_worktree(root: &Path, branch: &str) -> Result<Option<PathBuf>> {
    let wanted = format!("refs/heads/{branch}");
    Ok(list_worktrees(root)?
        .into_iter()
        .find(|worktree| worktree.branch.as_deref() == Some(wanted.as_str()))
        .map(|worktree| worktree.path))
}

/// Which of `paths` are git-ignored, through one batched `git check-ignore --stdin`.
pub fn check_ignore(root: &Path, paths: &[&Path]) -> Result<HashSet<PathBuf>> {
    if paths.is_empty() {
        return Ok(HashSet::new());
    }
    let input = paths
        .iter()
        .map(|p| p.to_string_lossy())
        .collect::<Vec<_>>()
        .join("\n");

    let mut child = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["check-ignore", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawn git check-ignore")?;
    child
        .stdin
        .take()
        .context("pipe git check-ignore stdin")?
        .write_all(input.as_bytes())
        .context("write to git check-ignore")?;
    let out = child.wait_with_output().context("wait git check-ignore")?;

    // exit 0 = some ignored, 1 = none ignored, anything else = fatal (not a repo).
    let code = out.status.code();
    if code != Some(0) && code != Some(1) {
        anyhow::bail!(
            "git check-ignore failed (exit {code:?}): {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }

    let mut ignored = HashSet::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let line = line.trim();
        if !line.is_empty() {
            ignored.insert(PathBuf::from(line));
        }
    }
    Ok(ignored)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("syro-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn git_in(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {} failed", args.join(" "));
    }

    #[test]
    fn a_shared_path_stays_inside_the_checkout() {
        assert_eq!(shared_path(" .env "), Some(".env".into()));
        assert_eq!(shared_path("./data/"), Some("data".into()));
        assert_eq!(shared_path("a/b"), Some("a/b".into()));
        for bad in ["", "  ", "/etc", "../x", "a/../../x", "./"] {
            assert_eq!(shared_path(bad), None, "{bad:?}");
        }
    }

    /// Each path lands in the worktree, by clone or by link, and what is missing
    /// or already there is left alone.
    #[test]
    fn shared_paths_are_brought_into_a_new_worktree() {
        let root = temp_dir("share-paths");
        let (main, tree) = (root.join("main"), root.join("tree"));
        std::fs::create_dir_all(main.join("data/deep")).unwrap();
        std::fs::create_dir_all(&tree).unwrap();
        std::fs::write(main.join(".env"), "KEY=1\n").unwrap();
        std::fs::write(main.join("data/deep/x"), "x").unwrap();
        std::fs::write(main.join("kept"), "main").unwrap();
        std::fs::write(tree.join("kept"), "tree").unwrap();

        let paths = [".env", "data", "kept", "missing", "../escape"].map(String::from);
        assert!(share_paths(&main, &tree, &paths).is_empty());

        assert_eq!(
            std::fs::read_to_string(tree.join(".env")).unwrap(),
            "KEY=1\n"
        );
        assert_eq!(
            std::fs::read_to_string(tree.join("data/deep/x")).unwrap(),
            "x"
        );
        assert_eq!(std::fs::read_to_string(tree.join("kept")).unwrap(), "tree");
        assert!(!tree.join("missing").exists());
        assert!(!root.join("escape").exists());
    }

    /// A repository on `branch` with one commit, in its own temp dir.
    fn scratch_repo(tag: &str, branch: &str) -> PathBuf {
        let dir = temp_dir(tag);
        git_in(&dir, &["init", "-q", "-b", branch]);
        git_in(&dir, &["config", "user.email", "test@syro.invalid"]);
        git_in(&dir, &["config", "user.name", "Syro Test"]);
        std::fs::write(dir.join("README.md"), "hello\n").unwrap();
        git_in(&dir, &["add", "README.md"]);
        git_in(&dir, &["commit", "-q", "-m", "first"]);
        dir
    }

    /// git prints paths resolved through symlinks (`/var` → `/private/var`), so compare them resolved.
    fn canonical(path: &Path) -> PathBuf {
        std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
    }

    fn branch_names(repo: &Path) -> Vec<String> {
        run_git_ok(repo, &["branch", "--format=%(refname:short)"])
            .unwrap()
            .lines()
            .map(|line| line.trim().to_string())
            .filter(|line| !line.is_empty())
            .collect()
    }

    /// The page a repository is opened at: the https form of its `origin`, and
    /// nothing at all for a remote that is not a URL.
    #[test]
    fn the_remote_url_is_the_https_form_of_origin() {
        let dir = scratch_repo("remote-url", "main");
        assert_eq!(remote_url(&dir), None, "no remote, no page");

        for (remote, expected) in [
            (
                "git@github.com:owner/repo.git",
                "https://github.com/owner/repo",
            ),
            (
                "https://github.com/owner/repo.git",
                "https://github.com/owner/repo",
            ),
            (
                "ssh://git@github.com/owner/repo",
                "https://github.com/owner/repo",
            ),
        ] {
            if remotes(&dir).unwrap().is_empty() {
                git_in(&dir, &["remote", "add", "origin", remote]);
            } else {
                git_in(&dir, &["remote", "set-url", "origin", remote]);
            }
            assert_eq!(remote_url(&dir).as_deref(), Some(expected), "{remote}");
        }

        // A local path is a remote, but it is not a page.
        git_in(&dir, &["remote", "set-url", "origin", "/tmp/elsewhere.git"]);
        assert_eq!(remote_url(&dir), None);
    }

    #[test]
    fn branch_headers_read_the_way_git_writes_them() {
        let text = "# branch.oid 2b3c\n# branch.head main\n# branch.upstream origin/main\n# branch.ab +2 -1\n1 .M N... 100644 100644 100644 a b src/main.rs\n";
        let status = parse_branch_status(text);
        assert_eq!(status.branch.as_deref(), Some("main"));
        assert_eq!(status.upstream.as_deref(), Some("origin/main"));
        assert_eq!((status.ahead, status.behind), (2, 1));
        assert_eq!(status.summary(), "2 ahead, 1 behind origin/main");

        let status = parse_branch_status("# branch.head feature/x\n");
        assert_eq!(status.branch.as_deref(), Some("feature/x"));
        assert_eq!(status.upstream, None);
        assert_eq!(status.summary(), "no upstream branch");

        let status = parse_branch_status("# branch.head (detached)\n");
        assert_eq!(status.branch, None);
        assert_eq!(status.name(), "detached HEAD");
    }

    /// The network three: a push sends the branch to the remote, a fetch brings
    /// the remote's refs back, and a pull moves the branch — but only where it
    /// fast-forwards.
    #[test]
    fn a_push_a_fetch_and_a_pull_go_through_the_remote() {
        let dir = temp_dir("sync");
        let origin = dir.join("origin.git");
        git_in(
            &dir,
            &[
                "init",
                "-q",
                "--bare",
                "-b",
                "main",
                &origin.to_string_lossy(),
            ],
        );
        let clone = |name: &str| {
            let at = dir.join(name);
            git_in(&dir, &["clone", "-q", &origin.to_string_lossy(), name]);
            git_in(&at, &["config", "user.email", "test@syro.invalid"]);
            git_in(&at, &["config", "user.name", "Syro Test"]);
            at
        };
        let mine = clone("mine");
        let theirs = clone("theirs");

        // A push says what it sent, on the stream `run_git_ok` never reads.
        std::fs::write(mine.join("one.txt"), "one\n").unwrap();
        git_in(&mine, &["add", "."]);
        git_in(&mine, &["commit", "-q", "-m", "the first"]);
        let said = push(&mine, true).unwrap();
        assert!(said.contains("main"), "the push said nothing: {said:?}");

        // So does a pull, and it is the commit that arrives.
        let said = pull(&theirs).unwrap();
        assert!(theirs.join("one.txt").is_file(), "the pull brought nothing");
        assert!(!said.is_empty(), "the pull said nothing at all");

        // A fetch is the refs, not the working tree: the file on disk is the
        // commit's after a `git log`, and the branch is where the fetch put it.
        std::fs::write(mine.join("two.txt"), "two\n").unwrap();
        git_in(&mine, &["add", "."]);
        git_in(&mine, &["commit", "-q", "-m", "the second"]);
        push(&mine, false).unwrap();
        assert!(fetch(&theirs).is_ok());
        assert!(
            !theirs.join("two.txt").is_file(),
            "fetch moved the worktree"
        );
        assert_eq!(
            run_git_ok(&theirs, &["rev-parse", "origin/main"])
                .unwrap()
                .trim(),
            run_git_ok(&mine, &["rev-parse", "HEAD"]).unwrap().trim()
        );

        // And a pull that would have to merge is refused rather than merged:
        // git's own words, not a merge commit.
        std::fs::write(theirs.join("mine-only.txt"), "x\n").unwrap();
        git_in(&theirs, &["add", "."]);
        git_in(&theirs, &["commit", "-q", "-m", "only here"]);
        let refused = pull(&theirs).unwrap_err().to_string();
        assert!(refused.contains("fast-forward"), "{refused:?}");
        // And it left no merge behind: one branch, no conflict.
        assert!(worktree_changes(&theirs).unwrap().is_empty());

        // A remote branch is deleted by pushing the deletion — and the local
        // remote-tracking ref goes with it, so the graph stops drawing it.
        git_in(&mine, &["push", "-q", "origin", "main:spike"]);
        assert!(fetch(&mine).is_ok());
        assert!(
            branches(&mine)
                .unwrap()
                .iter()
                .any(|branch| branch.name == "origin/spike")
        );
        let said = remote_branch_delete(&mine, "origin", "spike").unwrap();
        assert!(said.contains("spike"), "the delete said nothing: {said:?}");
        assert!(
            !branches(&mine)
                .unwrap()
                .iter()
                .any(|branch| branch.name == "origin/spike"),
            "the remote-tracking ref outlived the branch on the remote"
        );
        assert!(
            run_git(&origin, &["rev-parse", "--verify", "refs/heads/spike"])
                .unwrap()
                .status
                .success()
                == false,
            "the remote still has the branch"
        );
        // And git's own refusal — the remote will not lose its current branch —
        // comes back as an error rather than as silence.
        assert!(remote_branch_delete(&mine, "origin", "main").is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `git fetch origin pull/12/head:pr/12` leaves a ref under `refs/remotes`
    /// whose first segment is not a remote, and no remote's refspec owns it —
    /// no fetch would ever prune it. Deleting it is a local delete, not a push.
    #[test]
    fn deleting_a_ref_that_is_not_a_remote_branch_drops_it_locally() {
        let repo = scratch_repo("pr-ref", "main");
        git_in(&repo, &["update-ref", "refs/remotes/pr/1", "HEAD"]);
        assert!(
            branches(&repo)
                .unwrap()
                .iter()
                .any(|branch| branch.name == "pr/1")
        );

        let said = remote_branch_delete(&repo, "pr", "1").unwrap();
        assert!(said.contains("pr/1"), "the delete said nothing: {said:?}");
        assert!(
            !branches(&repo)
                .unwrap()
                .iter()
                .any(|branch| branch.name == "pr/1"),
            "the ref outlived the delete"
        );

        let _ = std::fs::remove_dir_all(&repo);
    }

    /// A branch somebody deleted on the remote — a merged PR, another machine —
    /// leaves a remote-tracking ref here that is the only thing still saying the
    /// branch exists. There is nothing left on the remote to delete, so git
    /// refuses; the stale ref is what goes, and it goes quietly.
    #[test]
    fn deleting_a_remote_branch_already_gone_drops_the_stale_ref() {
        let dir = temp_dir("stale-remote-branch");
        let origin = dir.join("origin.git");
        git_in(
            &dir,
            &[
                "init",
                "-q",
                "--bare",
                "-b",
                "main",
                &origin.to_string_lossy(),
            ],
        );
        git_in(&dir, &["clone", "-q", &origin.to_string_lossy(), "mine"]);
        let mine = dir.join("mine");
        git_in(&mine, &["config", "user.email", "test@syro.invalid"]);
        git_in(&mine, &["config", "user.name", "Syro Test"]);
        std::fs::write(mine.join("one.txt"), "one\n").unwrap();
        git_in(&mine, &["add", "."]);
        git_in(&mine, &["commit", "-q", "-m", "the first"]);
        git_in(&mine, &["push", "-q", "origin", "main:spike"]);
        git_in(&mine, &["fetch", "-q", "origin"]);
        assert!(
            branches(&mine)
                .unwrap()
                .iter()
                .any(|branch| branch.name == "origin/spike")
        );

        // The branch goes at the remote end, without this checkout pruning.
        git_in(&origin, &["update-ref", "-d", "refs/heads/spike"]);
        let said = remote_branch_delete(&mine, "origin", "spike").unwrap();
        assert!(said.contains("spike"), "the delete said nothing: {said:?}");
        assert!(
            !branches(&mine)
                .unwrap()
                .iter()
                .any(|branch| branch.name == "origin/spike"),
            "the stale remote-tracking ref outlived the delete"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_clean_branch_says_so_and_an_ahead_one_counts() {
        let mut status = BranchStatus {
            branch: Some("main".into()),
            upstream: Some("origin/main".into()),
            ..Default::default()
        };
        assert_eq!(status.summary(), "no changes ahead of origin/main");
        status.ahead = 3;
        assert_eq!(status.summary(), "3 ahead of origin/main");
        status.ahead = 0;
        status.behind = 5;
        assert_eq!(status.summary(), "5 behind origin/main");
    }

    #[test]
    fn log_lines_become_commits_with_clean_ref_names() {
        let text = "abc123\u{1f}abc1\u{1f}fix the flaky test\u{1f}Ada\u{1f}1700000000\u{1f}def456 abc789\u{1f}HEAD -> main, origin/main, tag: v0.2.35\n";
        let commits = parse_log(text);
        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].short, "abc1");
        assert_eq!(commits[0].summary, "fix the flaky test");
        assert_eq!(commits[0].author, "Ada");
        assert_eq!(commits[0].at, 1_700_000_000);
        assert_eq!(commits[0].refs, ["main", "origin/main", "v0.2.35"]);
        assert_eq!(commits[0].parents, ["def456", "abc789"]);

        // The first commit in a repository has no parents, which is an empty
        // field rather than a missing one.
        let root = parse_log("abc123\u{1f}abc1\u{1f}first\u{1f}Ada\u{1f}1700000000\u{1f}\u{1f}\n");
        assert_eq!(root.len(), 1);
        assert!(root[0].parents.is_empty());
        assert!(root[0].refs.is_empty());

        let commits = parse_log(
            "abc123\u{1f}abc1\u{1f}plain\u{1f}Ada\u{1f}1 day ago\u{1f}\u{1f}\nnonsense\n",
        );
        assert_eq!(commits.len(), 1);
        assert!(commits[0].refs.is_empty());
        // A date that is not one is the epoch rather than an error: the log read
        // as far as it could.
        assert_eq!(commits[0].at, 0);
    }

    /// The branches, local and remote, with the one `HEAD` is on marked — and
    /// the remote's `HEAD` symbol left out, because it is a name for another
    /// branch rather than one of its own.
    #[test]
    fn branches_come_back_local_first_and_remote_head_is_not_a_branch() {
        let repo = scratch_repo("branches", "main");
        git_in(&repo, &["branch", "feature"]);
        git_in(&repo, &["branch", "feat/nested"]);
        git_in(&repo, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
        git_in(
            &repo,
            &["update-ref", "refs/remotes/origin/feat/nested", "HEAD"],
        );
        git_in(
            &repo,
            &[
                "symbolic-ref",
                "refs/remotes/origin/HEAD",
                "refs/remotes/origin/main",
            ],
        );

        let found = branches(&repo).unwrap();
        let names: Vec<&str> = found.iter().map(|branch| branch.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "feat/nested",
                "feature",
                "main",
                "origin/feat/nested",
                "origin/main"
            ],
            "{names:?}"
        );
        assert!(found.iter().all(|branch| branch.name != "origin"));
        let main = found.iter().find(|branch| branch.name == "main").unwrap();
        assert!(main.current, "the branch HEAD is on is not marked");
        assert!(main.remote.is_none(), "a local branch is not a remote one");
        assert_eq!(main.commit.len(), 40);
        assert_eq!(main.commit[..main.short.len()], main.short);
        assert!(!found.iter().find(|b| b.name == "feature").unwrap().current);
        let remote = found.iter().find(|b| b.name == "origin/main").unwrap();
        assert_eq!(
            remote.remote,
            Some(RemoteRef {
                remote: "origin".into(),
                branch: "main".into(),
            })
        );
        assert_eq!(remote.commit, main.commit);
        // A branch with slashes of its own keeps all of them: the remote's name
        // is the first segment and the rest is the branch.
        assert_eq!(
            found
                .iter()
                .find(|b| b.name == "origin/feat/nested")
                .unwrap()
                .remote,
            Some(RemoteRef {
                remote: "origin".into(),
                branch: "feat/nested".into(),
            })
        );

        let _ = std::fs::remove_dir_all(&repo);
    }

    /// The graph's log is every branch, not just this one, and each commit knows
    /// the parents the lanes are drawn from.
    #[test]
    fn the_graph_reads_every_branch_with_the_parents_on_each_commit() {
        let repo = scratch_repo("graph-log", "main");
        git_in(&repo, &["branch", "feature"]);
        std::fs::write(repo.join("only-main.txt"), "x\n").unwrap();
        git_in(&repo, &["add", "."]);
        git_in(&repo, &["commit", "-q", "-m", "only on main"]);

        let only_branch = commits(&repo, 10).unwrap();
        assert_eq!(only_branch.len(), 2);
        assert_eq!(only_branch[0].parents.len(), 1);
        assert!(only_branch[1].parents.is_empty(), "the root has no parent");

        let all = graph_commits(&repo, 10).unwrap();
        // The feature branch points at the first commit, so `--all` is still the
        // same two commits — and the merge `feature` never had cannot appear.
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].summary, "only on main");
        assert_eq!(all[1].parents, Vec::<String>::new());

        // A merge names both parents, which is the one thing the panel's own log
        // never had to carry.
        std::fs::write(repo.join("on-feature.txt"), "y\n").unwrap();
        git_in(&repo, &["checkout", "-q", "feature"]);
        git_in(&repo, &["add", "."]);
        git_in(&repo, &["commit", "-q", "-m", "only on feature"]);
        git_in(&repo, &["checkout", "-q", "main"]);
        git_in(
            &repo,
            &["merge", "-q", "--no-ff", "-m", "merge feature", "feature"],
        );
        let merged = graph_commits(&repo, 10).unwrap();
        let merge = merged
            .iter()
            .find(|commit| commit.summary == "merge feature")
            .expect("the merge is not in the graph");
        assert_eq!(merge.parents.len(), 2, "a merge has two parents");
        assert_eq!(merged.len(), 4);

        let _ = std::fs::remove_dir_all(&repo);
    }

    /// A commit's file diff is the comparison the worktree's own diff is not:
    /// what that commit did to that file, whatever the worktree says since.
    #[test]
    fn a_commit_file_diff_shows_what_that_commit_did() {
        let repo = scratch_repo("commit-file-diff", "main");
        std::fs::write(repo.join("README.md"), "second\n").unwrap();
        git_in(&repo, &["add", "."]);
        git_in(&repo, &["commit", "-q", "-m", "the change"]);
        let id = run_git_ok(&repo, &["rev-parse", "HEAD"])
            .unwrap()
            .trim()
            .to_string();

        let text = commit_file_diff(&repo, &id, "README.md").unwrap();
        assert!(text.contains("+second"), "{text:?}");
        assert!(text.contains("-hello"), "{text:?}");

        // A file the commit did not touch has nothing to show, which is an empty
        // diff rather than an error.
        assert!(
            commit_file_diff(&repo, &id, "nothing.md")
                .unwrap()
                .is_empty()
        );
        // And a commit git cannot resolve is git's own complaint.
        assert!(commit_file_diff(&repo, "no-such-commit", "README.md").is_err());

        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn the_branch_and_the_history_come_from_a_real_repository() {
        let repo = scratch_repo("branch-log", "main");
        std::fs::write(repo.join("second.txt"), "two\n").unwrap();
        git_in(&repo, &["add", "second.txt"]);
        git_in(&repo, &["commit", "-q", "-m", "second commit"]);

        let status = branch_status(&repo).unwrap();
        assert_eq!(status.branch.as_deref(), Some("main"));
        assert_eq!(status.upstream, None);
        assert_eq!(status.summary(), "no upstream branch");

        let log = commits(&repo, 10).unwrap();
        assert_eq!(log.len(), 2);
        assert_eq!(log[0].summary, "second commit");
        assert_eq!(log[1].summary, "first");
        assert_eq!(log[0].author, "Syro Test");
        assert!(log[0].refs.iter().any(|name| name == "main"));

        // And the cap is what the caller asked for.
        assert_eq!(commits(&repo, 1).unwrap().len(), 1);

        let _ = std::fs::remove_dir_all(&repo);
    }

    /// What a history row opens onto: the files *that* commit changed, which is a
    /// different question from the branch's changes — the second commit below
    /// changed nothing the first one did not.
    #[test]
    fn a_commit_reports_the_files_it_changed() {
        let repo = scratch_repo("commit-files", "main");
        std::fs::write(repo.join("README.md"), "hello again\n").unwrap();
        std::fs::write(repo.join("second.txt"), "two\n").unwrap();
        git_in(&repo, &["add", "-A"]);
        git_in(&repo, &["commit", "-q", "-m", "second"]);
        git_in(&repo, &["rm", "-q", "second.txt"]);
        git_in(&repo, &["commit", "-q", "-m", "third"]);
        git_in(&repo, &["commit", "-q", "--allow-empty", "-m", "nothing"]);

        let log = commits(&repo, 10).unwrap();
        assert!(
            commit_files(&repo, &log[0].id).unwrap().is_empty(),
            "a commit that changed nothing has no files, and is not an error"
        );
        assert_eq!(
            commit_files(&repo, &log[1].id).unwrap(),
            vec![FileChange {
                path: "second.txt".into(),
                status: 'D',
                kind: ChangeKind::Committed,
            }],
            "the removal is the third commit's, and only its"
        );
        // git's own order: by path, the one the commit already had first.
        assert_eq!(
            commit_files(&repo, &log[2].id).unwrap(),
            vec![
                FileChange {
                    path: "README.md".into(),
                    status: 'M',
                    kind: ChangeKind::Committed,
                },
                FileChange {
                    path: "second.txt".into(),
                    status: 'A',
                    kind: ChangeKind::Committed,
                },
            ]
        );
        // The root commit is every file in it, all of them new.
        assert_eq!(
            commit_files(&repo, &log[3].id).unwrap(),
            vec![FileChange {
                path: "README.md".into(),
                status: 'A',
                kind: ChangeKind::Committed,
            }]
        );

        // And a name git cannot resolve is git's own complaint, not an empty list.
        assert!(commit_files(&repo, "no-such-commit").is_err());

        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn changed_files_separate_committed_staged_and_unstaged_diffs() {
        let repo = scratch_repo("change-kinds", "main");
        std::fs::write(repo.join("shared.txt"), "base\n").unwrap();
        git_in(&repo, &["add", "shared.txt"]);
        git_in(&repo, &["commit", "-q", "-m", "shared base"]);
        git_in(&repo, &["checkout", "-q", "-b", "feature"]);

        std::fs::write(repo.join("committed.txt"), "committed\n").unwrap();
        git_in(&repo, &["add", "committed.txt"]);
        git_in(&repo, &["commit", "-q", "-m", "branch change"]);

        std::fs::write(repo.join("shared.txt"), "base\nstaged\n").unwrap();
        git_in(&repo, &["add", "shared.txt"]);
        std::fs::write(repo.join("shared.txt"), "base\nstaged\nworktree\n").unwrap();
        std::fs::write(repo.join("loose.txt"), "untracked\n").unwrap();

        let changes = changed_files(&repo, "main").unwrap();
        let find = |path: &str, kind| {
            changes
                .iter()
                .find(|change| change.path == path && change.kind == kind)
                .unwrap()
        };
        let committed = find("committed.txt", ChangeKind::Committed);
        let staged = find("shared.txt", ChangeKind::Staged);
        let unstaged = find("shared.txt", ChangeKind::Unstaged);
        let untracked = find("loose.txt", ChangeKind::Unstaged);

        assert!(
            diff_text(&repo, "main", committed)
                .unwrap()
                .contains("+committed")
        );
        assert!(
            diff_text(&repo, "main", staged)
                .unwrap()
                .contains("+staged")
        );
        assert!(
            diff_text(&repo, "main", unstaged)
                .unwrap()
                .contains("+worktree")
        );
        assert_eq!(untracked.status, '?');
        assert!(
            diff_text(&repo, "main", untracked)
                .unwrap()
                .contains("+untracked")
        );

        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn discard_and_unstage_only_touch_the_requested_git_layer() {
        let repo = scratch_repo("undo-git-layer", "main");
        let file = repo.join("tracked.txt");
        std::fs::write(&file, "base\n").unwrap();
        git_in(&repo, &["add", "tracked.txt"]);
        git_in(&repo, &["commit", "-q", "-m", "tracked file"]);
        std::fs::write(&file, "base\nstaged\n").unwrap();
        git_in(&repo, &["add", "tracked.txt"]);
        std::fs::write(&file, "base\nstaged\nunstaged\n").unwrap();

        let changes = worktree_changes(&repo).unwrap();
        let staged = changes
            .iter()
            .find(|change| change.kind == ChangeKind::Staged)
            .unwrap();
        let unstaged = changes
            .iter()
            .find(|change| change.kind == ChangeKind::Unstaged)
            .unwrap();
        discard_unstaged(&repo, unstaged).unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "base\nstaged\n");
        assert!(
            run_git_ok(&repo, &["diff", "--cached"])
                .unwrap()
                .contains("+staged")
        );

        unstage(&repo, staged).unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "base\nstaged\n");
        assert!(run_git_ok(&repo, &["diff", "--cached"]).unwrap().is_empty());
        assert!(run_git_ok(&repo, &["diff"]).unwrap().contains("+staged"));

        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn discard_untracked_file_removes_only_that_file() {
        let repo = scratch_repo("discard-untracked", "main");
        let file = repo.join("loose.txt");
        std::fs::write(&file, "untracked\n").unwrap();
        let change = worktree_changes(&repo).unwrap().remove(0);
        discard_unstaged(&repo, &change).unwrap();
        assert!(!file.exists());
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn numstat_lines_become_a_count_per_path() {
        let repo = scratch_repo("numstat", "main");
        std::fs::write(repo.join("README.md"), "hello\nagain\n").unwrap();
        std::fs::write(repo.join("new.txt"), "one\ntwo\n").unwrap();
        git_in(&repo, &["add", "new.txt"]);

        let stats = diff_stat(&repo, "main", &worktree_changes(&repo).unwrap()).unwrap();
        // Staged and unstaged counts are independent.
        assert_eq!(
            stats.get(&(ChangeKind::Unstaged, "README.md".into())),
            Some(&(1, 0))
        );
        assert_eq!(
            stats.get(&(ChangeKind::Staged, "new.txt".into())),
            Some(&(2, 0))
        );
        assert_eq!(
            stats.get(&(ChangeKind::Committed, "src/main.rs".into())),
            None
        );

        // An untracked file is in no commit for `diff` to compare against, so the
        // whole file is what a commit would add and its own lines are the count.
        std::fs::write(repo.join("loose.txt"), "x\ny\n").unwrap();
        let stats = diff_stat(&repo, "main", &worktree_changes(&repo).unwrap()).unwrap();
        assert_eq!(
            stats.get(&(ChangeKind::Unstaged, "loose.txt".into())),
            Some(&(2, 0))
        );

        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn an_untracked_files_own_lines_are_its_count() {
        let repo = scratch_repo("untracked-lines", "main");
        // The last line is a line whether or not it ends in a newline.
        std::fs::write(repo.join("three.txt"), "one\ntwo\nthree\n").unwrap();
        std::fs::write(repo.join("two.txt"), "one\ntwo").unwrap();
        std::fs::write(repo.join("empty.txt"), "").unwrap();
        assert_eq!(untracked_lines(&repo.join("three.txt")), Some((3, 0)));
        assert_eq!(untracked_lines(&repo.join("two.txt")), Some((2, 0)));
        assert_eq!(untracked_lines(&repo.join("empty.txt")), Some((0, 0)));

        // Nothing to count: a binary file, a directory, a path that is not there.
        std::fs::write(repo.join("bin"), [0u8, 1, 2, b'\n']).unwrap();
        assert_eq!(untracked_lines(&repo.join("bin")), None);
        assert_eq!(untracked_lines(&repo), None);
        assert_eq!(untracked_lines(&repo.join("gone.txt")), None);

        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn a_clone_makes_the_directory_and_an_init_makes_a_repository() {
        let source = scratch_repo("clone-source", "main");
        let dir = temp_dir("clone-dest");
        let destination = dir.join("projects").join("copied");

        clone(&source.to_string_lossy(), &destination).unwrap();
        assert!(
            destination.join(".git").is_dir(),
            "the clone left no repository"
        );
        assert!(
            destination.join("README.md").is_file(),
            "the clone left no files"
        );
        assert_eq!(
            run_git_ok(&destination, &["log", "-1", "--pretty=%s"])
                .unwrap()
                .trim(),
            "first"
        );
        assert!(clone(&source.to_string_lossy(), &destination).is_err());
        assert!(clone("  ", &dir.join("nowhere")).is_err());

        let made = dir.join("projects").join("from-scratch");
        init(&made).unwrap();
        assert!(is_git_repo(&made));
        assert_eq!(
            run_git_ok(&made, &["symbolic-ref", "--short", "HEAD"])
                .unwrap()
                .trim(),
            "main"
        );

        let _ = fs::remove_dir_all(&source);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn committing_takes_the_worktree_and_needs_a_message() {
        let repo = scratch_repo("commit", "main");
        std::fs::write(repo.join("README.md"), "hello\nworld\n").unwrap();
        std::fs::write(repo.join("untracked.txt"), "new\n").unwrap();

        assert!(commit_all(&repo, "   ").is_err());
        assert_eq!(commits(&repo, 10).unwrap().len(), 1);

        let subject = commit_all(&repo, "add the world").unwrap();
        assert_eq!(subject.trim(), "add the world");
        let log = commits(&repo, 10).unwrap();
        assert_eq!(log.len(), 2);
        assert_eq!(log[0].summary, "add the world");
        // `add -A` is what makes the panel's one button honest: the untracked file it listed is in the commit.
        assert!(worktree_changes(&repo).unwrap().is_empty());
        let tracked = run_git_ok(&repo, &["ls-files"]).unwrap();
        assert!(tracked.contains("untracked.txt"), "{tracked}");

        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn the_commit_diff_holds_tracked_and_untracked_work() {
        let repo = scratch_repo("commit-diff", "main");
        std::fs::write(repo.join("README.md"), "hello\nworld\n").unwrap();
        std::fs::write(repo.join("fresh.txt"), "new\n").unwrap();

        let diff = commit_diff(&repo).unwrap();
        assert!(
            diff.contains("+world"),
            "the tracked change is missing: {diff}"
        );
        assert!(
            diff.contains("fresh.txt") && diff.contains("+new"),
            "the untracked file is missing: {diff}"
        );

        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn repo_root_is_found_from_a_subdirectory() {
        let repo = scratch_repo("repo-root", "main");
        let nested = repo.join("src").join("deep");
        std::fs::create_dir_all(&nested).unwrap();

        let root = repo_root(&nested).unwrap();

        assert_eq!(canonical(&root), canonical(&repo));

        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn branch_from_name_slugs_a_worktree_name() {
        assert_eq!(branch_from_name("Fix the Bug"), "fix-the-bug");
        assert_eq!(branch_from_name("feature/foo"), "feature-foo");
        assert_eq!(branch_from_name("  spaced  out  "), "spaced-out");
        assert_eq!(branch_from_name("a--b"), "a-b");
        assert_eq!(branch_from_name("v1.2.3"), "v1-2-3");
        assert_eq!(branch_from_name(""), "worktree");
        assert_eq!(branch_from_name("___"), "worktree");
        assert_eq!(branch_from_name("Ünïcode"), "n-code");
    }

    #[test]
    fn default_base_ref_follows_origin_head_then_trunk_names() {
        let repo = scratch_repo("base-ref-main", "main");
        assert_eq!(default_base_ref(&repo).unwrap(), "main");

        // A clone records the branch its remote's HEAD points at; that wins.
        git_in(
            &repo,
            &["update-ref", "refs/remotes/origin/release", "HEAD"],
        );
        git_in(
            &repo,
            &[
                "symbolic-ref",
                "refs/remotes/origin/HEAD",
                "refs/remotes/origin/release",
            ],
        );
        assert_eq!(default_base_ref(&repo).unwrap(), "release");

        let master = scratch_repo("base-ref-master", "master");
        assert_eq!(default_base_ref(&master).unwrap(), "master");

        // Nothing conventional to name, so the answer is whatever HEAD is.
        let trunk = scratch_repo("base-ref-trunk", "trunk");
        assert_eq!(default_base_ref(&trunk).unwrap(), "HEAD");

        let _ = std::fs::remove_dir_all(&repo);
        let _ = std::fs::remove_dir_all(&master);
        let _ = std::fs::remove_dir_all(&trunk);
    }

    #[test]
    fn worktree_add_remove_and_branch_delete_round_trip() {
        let repo = scratch_repo("worktree-ops", "main");
        let scratch = temp_dir("worktree-ops-paths");
        let first = scratch.join("one");

        worktree_add(&repo, &first, "feature-x", "main").unwrap();
        assert!(first.join("README.md").is_file());
        let listed = list_worktrees(&repo).unwrap();
        assert_eq!(listed.len(), 2);
        assert!(
            listed
                .iter()
                .any(|w| canonical(&w.path) == canonical(&first)
                    && w.branch.as_deref() == Some("refs/heads/feature-x")),
            "the new worktree is missing from {listed:?}"
        );
        // The branch a worktree holds: git will not delete it, and this is what
        // says which directory would have to go first.
        assert_eq!(
            branch_worktree(&repo, "feature-x")
                .unwrap()
                .map(|path| canonical(&path)),
            Some(canonical(&first))
        );
        assert!(branch_delete(&repo, "feature-x", false).is_err());
        assert!(
            branch_delete(&repo, "feature-x", true).is_err(),
            "force is not a way out of a worktree"
        );
        assert_eq!(
            branch_worktree(&repo, "main")
                .unwrap()
                .map(|path| canonical(&path)),
            Some(canonical(&repo))
        );
        assert_eq!(branch_worktree(&repo, "no-such-branch").unwrap(), None);

        worktree_remove(&repo, &first, false).unwrap();
        assert!(!first.exists());
        assert_eq!(list_worktrees(&repo).unwrap().len(), 1);

        let second = scratch.join("two");
        worktree_add(&repo, &second, "feature-x", "main").unwrap();
        assert!(second.join("README.md").is_file());
        worktree_remove(&repo, &second, false).unwrap();
        assert!(!second.exists());

        assert!(branch_names(&repo).contains(&"feature-x".to_string()));
        branch_delete(&repo, "feature-x", false).unwrap();
        assert_eq!(branch_names(&repo), vec!["main".to_string()]);

        let _ = std::fs::remove_dir_all(&scratch);
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn worktree_remove_needs_force_for_a_dirty_worktree() {
        let repo = scratch_repo("worktree-force", "main");
        let scratch = temp_dir("worktree-force-paths");
        let wt = scratch.join("dirty");
        worktree_add(&repo, &wt, "dirty-work", "main").unwrap();
        std::fs::write(wt.join("README.md"), "changed\n").unwrap();

        assert!(worktree_remove(&repo, &wt, false).is_err());
        assert!(wt.exists());

        worktree_remove(&repo, &wt, true).unwrap();
        assert!(!wt.exists());

        let _ = std::fs::remove_dir_all(&scratch);
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn parse_worktrees_porcelain_handles_detached_and_bare() {
        let text = "\
worktree /repo/main
HEAD 1111111111111111111111111111111111111111
branch refs/heads/main

worktree /repo/feature
HEAD 2222222222222222222222222222222222222222
branch refs/heads/feature

worktree /repo/detached
HEAD 3333333333333333333333333333333333333333
detached

worktree /repo/bare
bare
";
        let ws = parse_worktrees_porcelain(text);
        assert_eq!(ws.len(), 4);

        assert_eq!(ws[0].path, PathBuf::from("/repo/main"));
        assert_eq!(ws[0].branch.as_deref(), Some("refs/heads/main"));
        assert!(!ws[0].detached && !ws[0].bare);

        assert_eq!(ws[1].branch.as_deref(), Some("refs/heads/feature"));

        assert!(ws[2].detached);
        assert!(ws[2].branch.is_none());

        assert!(ws[3].bare);
        assert!(ws[3].head.is_none());
        assert!(ws[3].branch.is_none());
    }

    #[test]
    fn parse_name_status_keeps_rename_destination() {
        assert_eq!(
            parse_name_status("M\tsrc/a.rs"),
            Some(('M', "src/a.rs".to_string()))
        );
        assert_eq!(
            parse_name_status("R100\told.rs\tnew.rs"),
            Some(('R', "new.rs".to_string()))
        );
    }

    #[test]
    fn diff_text_covers_a_tracked_change_and_an_untracked_file() {
        let dir = scratch_repo("diff-text", "main");
        std::fs::write(dir.join("tracked.txt"), "one\n").unwrap();
        git_in(&dir, &["add", "tracked.txt"]);
        git_in(&dir, &["commit", "-m", "init"]);
        std::fs::write(dir.join("tracked.txt"), "one\ntwo\n").unwrap();
        std::fs::write(dir.join("fresh.txt"), "hello\n").unwrap();

        let changes = changed_files(&dir, "HEAD").unwrap();
        let tracked = changes.iter().find(|c| c.path == "tracked.txt").unwrap();
        let text = diff_text(&dir, "HEAD", tracked).unwrap();
        assert!(text.contains("+two"), "{text}");
        assert!(
            text.contains("-"),
            "the diff should show the hunk header too: {text}"
        );

        // Untracked: diffed against /dev/null, and `--no-index` exits 1 here, which is not a failure.
        let fresh = changes.iter().find(|c| c.path == "fresh.txt").unwrap();
        let text = diff_text(&dir, "HEAD", fresh).unwrap();
        assert!(text.contains("+hello"), "{text}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn text_search_finds_literal_lines_and_skips_ignored_files() {
        let dir = scratch_repo("search-text", "main");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/notes.txt"), "A needle in this line\n").unwrap();
        std::fs::write(dir.join("src/other.txt"), "another needle here\n").unwrap();
        std::fs::write(dir.join(".gitignore"), "ignored.txt\n").unwrap();
        std::fs::write(dir.join("ignored.txt"), "needle hidden\n").unwrap();
        let found = search_text(&dir, "NEEDLE").unwrap();
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].path, "src/notes.txt");
        assert_eq!(found[0].line, 1);
        assert_eq!(found[0].column, 3);
        assert_eq!(found[0].text, "A needle in this line");
        assert_eq!(found[1].path, "src/other.txt");
        assert_eq!(found[1].line, 1);
        assert_eq!(found[1].column, 9);
        assert_eq!(found[1].text, "another needle here");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_search_term_has_no_results() {
        let dir = scratch_repo("search-empty", "main");
        assert!(search_text(&dir, " ").unwrap().is_empty());
        assert!(search_text(&dir, "no-match").unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ls_files_lists_tracked_files_relative_to_the_root() {
        let dir = scratch_repo("ls-files", "main");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(dir.join("README.md"), "# hi\n").unwrap();
        git_in(&dir, &["add", "-A"]);
        git_in(&dir, &["commit", "-m", "init"]);
        std::fs::write(dir.join("untracked.txt"), "x\n").unwrap();

        let files = ls_files(&dir).unwrap();
        assert!(files.contains(&"src/main.rs".to_string()), "{files:?}");
        assert!(files.contains(&"README.md".to_string()), "{files:?}");
        assert!(!files.contains(&"untracked.txt".to_string()), "{files:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_untracked_directory_is_listed_as_the_files_inside_it() {
        let dir = scratch_repo("untracked-dir", "main");
        std::fs::create_dir_all(dir.join("web")).unwrap();
        std::fs::write(dir.join("web/page.html"), "hi\n").unwrap();

        // Not one `web/` row, which git will not diff; the files themselves.
        let changes = changed_files(&dir, "HEAD").unwrap();
        assert!(
            !changes.iter().any(|c| c.path.ends_with('/')),
            "{changes:?}"
        );
        let page = changes.iter().find(|c| c.path == "web/page.html").unwrap();
        assert!(diff_text(&dir, "HEAD", page).unwrap().contains("+hi"));

        // The one directory that survives: a nested repository. It is a row with
        // nothing to show, not an error git prints into the diff pane.
        git_in(&dir.join("web"), &["init", "-q"]);
        let changes = changed_files(&dir, "HEAD").unwrap();
        let nested = changes.iter().find(|c| c.path == "web/").unwrap();
        assert_eq!(diff_text(&dir, "HEAD", nested).unwrap(), "");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn check_ignore_reports_only_ignored_paths() {
        let dir = temp_dir("check-ignore");
        let status = Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(["init", "-q"])
            .status()
            .unwrap();
        assert!(status.success());

        std::fs::write(dir.join(".gitignore"), "ignored.txt\nnode_modules/\n").unwrap();
        std::fs::write(dir.join("ignored.txt"), "x").unwrap();
        std::fs::write(dir.join("kept.txt"), "x").unwrap();
        std::fs::create_dir(dir.join("node_modules")).unwrap();

        let ignored = check_ignore(
            &dir,
            &[
                Path::new("ignored.txt"),
                Path::new("kept.txt"),
                Path::new("node_modules"),
            ],
        )
        .unwrap();

        assert!(ignored.contains(Path::new("ignored.txt")));
        assert!(ignored.contains(Path::new("node_modules")));
        assert!(!ignored.contains(Path::new("kept.txt")));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
