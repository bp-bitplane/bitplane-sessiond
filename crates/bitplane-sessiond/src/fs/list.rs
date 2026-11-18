//! A folder's entries with their gitignore status.

use std::path::{Component, Path, PathBuf};
use std::process::Stdio;

use bitplane_protocol::fs::{FsEntry, FsEntryKind};
use tokio::io::AsyncWriteExt;

use crate::git_cli::Git;

pub const GIT_DIR: &str = ".git";

/// The entries of the folder `dir`, sorted by name; `None` when it is not a
/// folder. Without `git`, nothing but `.git` is ignored.
pub async fn list(dir: &Path, git: Option<&Git>) -> Option<Vec<FsEntry>> {
    let canonical = dir.canonicalize().ok().filter(|path| path.is_dir())?;
    let read = std::fs::read_dir(&canonical).ok()?;
    let mut entries: Vec<FsEntry> = read
        .filter_map(Result::ok)
        .map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let kind = match std::fs::metadata(entry.path()) {
                Ok(meta) if meta.is_dir() => FsEntryKind::Directory,
                _ => FsEntryKind::File,
            };
            FsEntry {
                ignored: name == GIT_DIR,
                name,
                kind,
            }
        })
        .collect();
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    let all_ignored = in_git_dir(dir) || in_git_dir(&canonical);
    let Some(git) = git else {
        entries
            .iter_mut()
            .for_each(|entry| entry.ignored |= all_ignored);
        return Some(entries);
    };
    if all_ignored || behind_ignored_link(dir, git).await {
        entries.iter_mut().for_each(|entry| entry.ignored = true);
    } else if let Some(repo) = containing_repo(&canonical) {
        let names: Vec<&str> = entries.iter().map(|entry| entry.name.as_str()).collect();
        let ignored = check_ignore(git, &repo, &canonical, &names).await;
        for entry in &mut entries {
            entry.ignored |= ignored.contains(&entry.name);
        }
    }
    Some(entries)
}

pub fn in_git_dir(path: &Path) -> bool {
    path.components().any(|c| c.as_os_str() == GIT_DIR)
}

/// The nearest folder from `dir` up holding a `.git`.
pub fn containing_repo(dir: &Path) -> Option<PathBuf> {
    dir.ancestors()
        .find(|ancestor| ancestor.join(GIT_DIR).exists())
        .map(Path::to_path_buf)
}

/// Whether any symlink on the way to `dir` is ignored where it lives. Git
/// won't answer for paths beyond a symlink, so whatever it holds inherits that.
async fn behind_ignored_link(dir: &Path, git: &Git) -> bool {
    let mut path = PathBuf::new();
    for component in dir.components() {
        path.push(component);
        if !matches!(component, Component::Normal(_)) {
            continue;
        }
        let is_link = path
            .symlink_metadata()
            .is_ok_and(|meta| meta.file_type().is_symlink());
        if !is_link {
            continue;
        }
        let Some(parent) = path.parent().and_then(|parent| parent.canonicalize().ok()) else {
            continue;
        };
        let Some(repo) = containing_repo(&parent) else {
            continue;
        };
        let name = component.as_os_str().to_string_lossy();
        if check_ignore(git, &repo, &parent, &[&name])
            .await
            .iter()
            .any(|ignored| *ignored == name)
        {
            return true;
        }
    }
    false
}

/// Which of `names` in `dir` `repo`'s ignore rules match. Any failure leaves
/// everything unignored.
async fn check_ignore(git: &Git, repo: &Path, dir: &Path, names: &[&str]) -> Vec<String> {
    let prefix = dir.strip_prefix(repo).unwrap_or(Path::new(""));
    let mut input = Vec::new();
    for name in names.iter().filter(|name| **name != GIT_DIR) {
        input.extend_from_slice(prefix.join(name).to_string_lossy().as_bytes());
        input.push(0);
    }
    if input.is_empty() {
        return Vec::new();
    }
    let Ok(mut child) = git
        .command(
            repo,
            &[
                "-c",
                "core.fsmonitor=false",
                "check-ignore",
                "-z",
                "--stdin",
            ],
        )
        .stdin(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return Vec::new();
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(&input).await;
    }
    let Ok(output) = child.wait_with_output().await else {
        return Vec::new();
    };
    // 1 means nothing matched; 128 means `repo` isn't usable as a repo.
    if !output.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&output.stdout)
        .split('\0')
        .filter(|path| !path.is_empty())
        .filter_map(|path| Path::new(path).file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .collect()
}
