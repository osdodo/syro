//! Lazy directory tree: read one level at a time, never a recursive scan.
//! A collapsed directory holds nothing; entries are fetched on demand.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;

use rust_i18n::t;

use crate::git;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
    pub ignored: bool,
}

/// One level of `path`: directories first, then files, each sorted by name.
fn read_dir_entries(path: &Path) -> Vec<FileEntry> {
    let mut entries = Vec::new();
    let rd = match fs::read_dir(path) {
        Ok(rd) => rd,
        Err(_) => return entries,
    };
    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == ".git" {
            continue;
        }
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        entries.push(FileEntry {
            path: entry.path(),
            name,
            is_dir,
            ignored: false,
        });
    }
    entries.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.cmp(&b.name)));
    entries
}

/// One level of `path`, no ignore checking.
pub fn read_dir(path: &Path) -> Result<Vec<FileEntry>> {
    Ok(read_dir_entries(path))
}

/// One level of `path`, with git-ignored entries flagged (one batched `git check-ignore`).
pub fn read_dir_ignored(path: &Path) -> Result<Vec<FileEntry>> {
    let mut entries = read_dir_entries(path);

    let rel: Vec<PathBuf> = entries.iter().map(|e| PathBuf::from(&e.name)).collect();
    let refs: Vec<&Path> = rel.iter().map(|p| p.as_path()).collect();
    if let Ok(ignored) = git::check_ignore(path, &refs) {
        for entry in entries.iter_mut() {
            if ignored.contains(Path::new(&entry.name)) {
                entry.ignored = true;
            }
        }
    }
    // ponytail: if check-ignore fails (not a repo) nothing is flagged; the tree still renders.

    Ok(entries)
}

/// Make an empty file. Never replaces an existing one.
pub fn create_file(path: &Path) -> Result<()> {
    if path.exists() {
        anyhow::bail!(t!("fs.already_exists", path = path.display()).to_string());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, "")?;
    Ok(())
}

/// Make a directory, with its parents, refusing an existing path.
pub fn create_dir(path: &Path) -> Result<()> {
    if path.exists() {
        anyhow::bail!(t!("fs.already_exists", path = path.display()).to_string());
    }
    fs::create_dir_all(path)?;
    Ok(())
}

/// Rename within the same parent; refuses to move across directories.
pub fn rename(path: &Path, to: &Path) -> Result<()> {
    if path.parent() != to.parent() {
        anyhow::bail!(t!("fs.not_same_dir", to = to.display(), path = path.display()).to_string());
    }
    if to.exists() {
        anyhow::bail!(t!("fs.already_exists", path = to.display()).to_string());
    }
    fs::rename(path, to)?;
    Ok(())
}

/// Copy `src` (a file, or a directory with everything under it) into `dir`,
/// keeping its name. Never replaces an existing entry.
pub fn copy_into(src: &Path, dir: &Path) -> Result<()> {
    // A directory copied into itself would recurse forever.
    let Some(name) = src.file_name().filter(|_| !dir.starts_with(src)) else {
        anyhow::bail!(t!("git.share_failed", path = src.display()).to_string());
    };
    let to = dir.join(name);
    if to.exists() {
        anyhow::bail!(t!("fs.already_exists", path = to.display()).to_string());
    }
    copy_recursive(src, &to)
}

/// Move `src` into `dir`, keeping its name. A move to where it already is does
/// nothing; an existing entry is never replaced.
pub fn move_into(src: &Path, dir: &Path) -> Result<()> {
    if src.parent() == Some(dir) || src == dir {
        return Ok(());
    }
    // A directory moved into itself would vanish into its own subtree.
    let Some(name) = src.file_name().filter(|_| !dir.starts_with(src)) else {
        anyhow::bail!(t!("fs.not_same_dir", to = dir.display(), path = src.display()).to_string());
    };
    let to = dir.join(name);
    if to.exists() {
        anyhow::bail!(t!("fs.already_exists", path = to.display()).to_string());
    }
    fs::rename(src, to)?;
    Ok(())
}

fn copy_recursive(src: &Path, to: &Path) -> Result<()> {
    if fs::symlink_metadata(src)?.is_dir() {
        fs::create_dir(to)?;
        for entry in fs::read_dir(src)? {
            let entry = entry?;
            copy_recursive(&entry.path(), &to.join(entry.file_name()))?;
        }
    } else {
        fs::copy(src, to)?;
    }
    Ok(())
}

/// Delete a file, or a directory and everything under it.
pub fn remove(path: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(path)?;
    if meta.is_dir() {
        fs::remove_dir_all(path)?;
    } else {
        fs::remove_file(path)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("syro-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn mutations_refuse_to_clobber_what_is_there() {
        let dir = temp_dir("fs-mutations");

        create_file(&dir.join("a.txt")).unwrap();
        assert!(dir.join("a.txt").is_file());
        assert!(
            create_file(&dir.join("a.txt")).is_err(),
            "an existing file must not be replaced"
        );
        assert!(create_dir(&dir.join("a.txt")).is_err());

        create_dir(&dir.join("sub/deep")).unwrap();
        assert!(dir.join("sub/deep").is_dir());

        rename(&dir.join("a.txt"), &dir.join("b.txt")).unwrap();
        assert!(!dir.join("a.txt").exists() && dir.join("b.txt").is_file());
        assert!(
            rename(&dir.join("b.txt"), &dir.join("sub/b.txt")).is_err(),
            "a rename is within one directory"
        );
        create_file(&dir.join("c.txt")).unwrap();
        assert!(rename(&dir.join("b.txt"), &dir.join("c.txt")).is_err());

        create_file(&dir.join("sub/deep/x.txt")).unwrap();
        remove(&dir.join("sub")).unwrap();
        assert!(!dir.join("sub").exists());
        remove(&dir.join("c.txt")).unwrap();

        let outside = temp_dir("fs-copy-src");
        std::fs::create_dir_all(outside.join("pkg/inner")).unwrap();
        std::fs::write(outside.join("pkg/inner/f.txt"), "hi").unwrap();
        copy_into(&outside.join("pkg"), &dir).unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("pkg/inner/f.txt")).unwrap(), "hi");
        assert!(copy_into(&outside.join("pkg"), &dir).is_err(), "no clobbering");
        assert!(copy_into(&dir.join("pkg"), &dir.join("pkg/inner")).is_err());

        create_dir(&dir.join("dest")).unwrap();
        move_into(&dir.join("pkg"), &dir.join("dest")).unwrap();
        assert!(!dir.join("pkg").exists() && dir.join("dest/pkg/inner/f.txt").is_file());
        move_into(&dir.join("dest/pkg"), &dir.join("dest")).unwrap();
        assert!(move_into(&dir.join("dest"), &dir.join("dest/pkg")).is_err());
        create_file(&dir.join("pkg")).unwrap();
        assert!(move_into(&dir.join("pkg"), &dir.join("dest")).is_err(), "no clobbering");
        let _ = std::fs::remove_dir_all(&outside);
        assert!(!dir.join("c.txt").exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_dir_orders_dirs_first_and_skips_dot_git() {
        let dir = temp_dir("fs-tree");
        std::fs::create_dir(dir.join(".git")).unwrap();
        std::fs::create_dir(dir.join("zdir")).unwrap();
        std::fs::create_dir(dir.join("adir")).unwrap();
        std::fs::write(dir.join("bfile"), "x").unwrap();
        std::fs::write(dir.join("afile"), "x").unwrap();

        let entries = read_dir(&dir).unwrap();
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();

        assert_eq!(names, vec!["adir", "zdir", "afile", "bfile"]);
        assert!(entries[0].is_dir && entries[1].is_dir);
        assert!(!entries[2].is_dir && !entries[3].is_dir);
        assert!(!names.contains(&".git"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
