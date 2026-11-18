//! A watched repo: its worktree, git dir and (for a linked worktree) common
//! dir, and which changes under them can change its status.

use std::path::{Component, Path, PathBuf};

/// Folders whose churn never changes a status.
pub const IGNORED_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    ".venv",
    "__pycache__",
    ".next",
    ".turbo",
    ".cache",
];

/// Git dir contents that change without changing a status.
const GIT_NOISE: &[&str] = &["objects", "logs", "fsmonitor--daemon"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repo {
    pub worktree: PathBuf,
    pub git_dir: PathBuf,
    pub common_dir: Option<PathBuf>,
}

impl Repo {
    /// The repo whose worktree is `dir`, canonicalized to match event paths;
    /// `None` when `dir` holds no `.git`.
    pub fn at(dir: &Path) -> Option<Repo> {
        let worktree = dir.canonicalize().ok().filter(|dir| dir.is_dir())?;
        let dot_git = worktree.join(".git");
        let git_dir = if dot_git.is_dir() {
            dot_git
        } else {
            let pointer = std::fs::read_to_string(&dot_git).ok()?;
            worktree.join(pointer.strip_prefix("gitdir:")?.trim())
        };
        let git_dir = git_dir.canonicalize().ok()?;
        let common_dir = std::fs::read_to_string(git_dir.join("commondir"))
            .ok()
            .and_then(|path| git_dir.join(path.trim()).canonicalize().ok())
            .filter(|common| *common != git_dir);
        Some(Repo {
            worktree,
            git_dir,
            common_dir,
        })
    }

    /// What the OS watches, recursively: the worktree, and each git dir not
    /// already inside it.
    pub fn watched(&self) -> Vec<PathBuf> {
        let dirs: Vec<&PathBuf> = std::iter::once(&self.git_dir)
            .chain(&self.common_dir)
            .collect();
        let mut paths = vec![self.worktree.clone()];
        for dir in &dirs {
            let covered = dir.starts_with(&self.worktree)
                || dirs
                    .iter()
                    .any(|other| other != dir && dir.starts_with(other));
            if !covered {
                paths.push((*dir).clone());
            }
        }
        paths
    }

    /// Whether a change at `path` can change the status.
    pub fn is_relevant(&self, path: &Path) -> bool {
        if let Ok(relative) = path.strip_prefix(&self.git_dir) {
            return git_relevant(relative);
        }
        if let Some(common) = &self.common_dir
            && let Ok(relative) = path.strip_prefix(common)
        {
            // Other linked worktrees' own state lives in `worktrees/`.
            return !starts_with(relative, "worktrees") && git_relevant(relative);
        }
        path.strip_prefix(&self.worktree).is_ok_and(|relative| {
            !relative
                .components()
                .any(|c| IGNORED_DIRS.iter().any(|ignored| c.as_os_str() == *ignored))
        })
    }

    /// The worktree or its `.git` no longer exists.
    pub fn is_gone(&self) -> bool {
        !self.worktree.is_dir() || !self.worktree.join(".git").exists() || !self.git_dir.is_dir()
    }
}

fn git_relevant(relative: &Path) -> bool {
    !relative.extension().is_some_and(|ext| ext == "lock")
        && !GIT_NOISE.iter().any(|noise| starts_with(relative, noise))
}

fn starts_with(relative: &Path, name: &str) -> bool {
    matches!(relative.components().next(), Some(Component::Normal(first)) if first == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> Repo {
        Repo {
            worktree: "/p/web".into(),
            git_dir: "/p/web/.git".into(),
            common_dir: None,
        }
    }

    fn linked() -> Repo {
        Repo {
            worktree: "/p/feature".into(),
            git_dir: "/p/web/.git/worktrees/feature".into(),
            common_dir: Some("/p/web/.git".into()),
        }
    }

    #[test]
    fn worktree_changes_count_unless_inside_an_ignored_folder() {
        let repo = repo();
        assert!(repo.is_relevant(Path::new("/p/web/src/a.rs")));
        assert!(!repo.is_relevant(Path::new("/p/web/node_modules/x/a.js")));
        assert!(!repo.is_relevant(Path::new("/p/web/app/target/debug/x")));
        assert!(!repo.is_relevant(Path::new("/p/other/a.rs")));
    }

    #[test]
    fn git_dir_changes_count_but_locks_objects_and_logs_do_not() {
        let repo = repo();
        assert!(repo.is_relevant(Path::new("/p/web/.git/index")));
        assert!(repo.is_relevant(Path::new("/p/web/.git/refs/heads/main")));
        assert!(!repo.is_relevant(Path::new("/p/web/.git/index.lock")));
        assert!(!repo.is_relevant(Path::new("/p/web/.git/objects/ab/cdef")));
        assert!(!repo.is_relevant(Path::new("/p/web/.git/logs/HEAD")));
    }

    #[test]
    fn a_linked_worktree_ignores_its_siblings_state() {
        let repo = linked();
        assert!(repo.is_relevant(Path::new("/p/web/.git/worktrees/feature/index")));
        assert!(repo.is_relevant(Path::new("/p/web/.git/refs/heads/feature")));
        assert!(!repo.is_relevant(Path::new("/p/web/.git/worktrees/other/index")));
        assert_eq!(
            repo.watched(),
            [PathBuf::from("/p/feature"), PathBuf::from("/p/web/.git")]
        );
    }

    #[test]
    fn a_git_dir_inside_the_worktree_is_not_watched_twice() {
        assert_eq!(repo().watched(), [PathBuf::from("/p/web")]);
    }
}
