//! Reading `git status --porcelain=v2 --branch -z`.

use std::path::Path;

use bitplane_protocol::git::{GitChange, GitChangeKind, GitHead, GitStatus, GitTracking};

use crate::git_cli::{self, Git};

/// The repo's status; `Err` carries git's own words.
pub async fn read(git: &Git, dir: &Path) -> Result<GitStatus, String> {
    let output = git
        .command(
            dir,
            &[
                "--no-optional-locks",
                "status",
                "--porcelain=v2",
                "--branch",
                "-z",
            ],
        )
        .output()
        .await
        .map_err(|error| format!("couldn't run git: {error}"))?;
    if !output.status.success() {
        return Err(git_cli::failure(&output));
    }
    parse(&output.stdout)
}

pub fn parse(output: &[u8]) -> Result<GitStatus, String> {
    let text = String::from_utf8_lossy(output);
    let mut fields = text.split('\0');
    let (mut oid, mut branch, mut tracking) = (None, None, None);
    let mut status = GitStatus {
        head: GitHead::Unknown,
        tracking: None,
        staged: Vec::new(),
        unstaged: Vec::new(),
        untracked: Vec::new(),
    };
    while let Some(entry) = fields.next() {
        let (tag, rest) = entry.split_once(' ').unwrap_or((entry, ""));
        match tag {
            "#" => match rest.split_once(' ') {
                Some(("branch.oid", value)) => oid = Some(value),
                Some(("branch.head", value)) => branch = Some(value),
                Some(("branch.ab", value)) => tracking = parse_ab(value),
                _ => {}
            },
            "1" => {
                let parts: Vec<&str> = rest.splitn(8, ' ').collect();
                let [xy, .., path] = parts[..] else {
                    return Err(unexpected(entry));
                };
                push_change(&mut status, xy, path, None);
            }
            "2" => {
                let parts: Vec<&str> = rest.splitn(9, ' ').collect();
                let [xy, .., path] = parts[..] else {
                    return Err(unexpected(entry));
                };
                let from = fields.next().ok_or_else(|| unexpected(entry))?;
                push_change(&mut status, xy, path, Some(from));
            }
            "u" => {
                let parts: Vec<&str> = rest.splitn(10, ' ').collect();
                let [.., path] = parts[..] else {
                    return Err(unexpected(entry));
                };
                status.unstaged.push(GitChange {
                    path: path.into(),
                    kind: GitChangeKind::Conflicted,
                    from: None,
                });
            }
            "?" => status.untracked.push(rest.into()),
            _ => {}
        }
    }
    let (Some(oid), Some(branch)) = (oid, branch) else {
        return Err("git status didn't report the branch".into());
    };
    status.head = match branch {
        "(detached)" => GitHead::Detached {
            sha: oid.chars().take(7).collect(),
        },
        name => GitHead::Branch { name: name.into() },
    };
    status.tracking = tracking;
    Ok(status)
}

fn push_change(status: &mut GitStatus, xy: &str, path: &str, from: Option<&str>) {
    let mut codes = xy.chars();
    let (index, worktree) = (codes.next(), codes.next());
    for (code, list) in [
        (index, &mut status.staged),
        (worktree, &mut status.unstaged),
    ] {
        let Some(kind) = code.and_then(change_kind) else {
            continue;
        };
        let from = matches!(kind, GitChangeKind::Renamed | GitChangeKind::Copied)
            .then(|| from.map(String::from))
            .flatten();
        list.push(GitChange {
            path: path.into(),
            kind,
            from,
        });
    }
}

fn change_kind(code: char) -> Option<GitChangeKind> {
    Some(match code {
        '.' => return None,
        'T' => GitChangeKind::TypeChanged,
        'A' => GitChangeKind::Added,
        'D' => GitChangeKind::Deleted,
        'R' => GitChangeKind::Renamed,
        'C' => GitChangeKind::Copied,
        'U' => GitChangeKind::Conflicted,
        _ => GitChangeKind::Modified,
    })
}

fn parse_ab(value: &str) -> Option<GitTracking> {
    let (ahead, behind) = value.split_once(' ')?;
    Some(GitTracking {
        ahead: ahead.strip_prefix('+')?.parse().ok()?,
        behind: behind.strip_prefix('-')?.parse().ok()?,
    })
}

fn unexpected(entry: &str) -> String {
    format!("unexpected git status line: {entry}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn change(path: &str, kind: GitChangeKind, from: Option<&str>) -> GitChange {
        GitChange {
            path: path.into(),
            kind,
            from: from.map(String::from),
        }
    }

    #[test]
    fn reads_the_branch_and_tracking() {
        let output = b"# branch.oid 1234567890abcdef\0# branch.head main\0# branch.upstream origin/main\0# branch.ab +2 -1\0";
        let status = parse(output).unwrap();
        assert_eq!(
            status.head,
            GitHead::Branch {
                name: "main".into()
            }
        );
        assert_eq!(
            status.tracking,
            Some(GitTracking {
                ahead: 2,
                behind: 1
            })
        );
    }

    #[test]
    fn a_detached_head_is_its_short_sha() {
        let status = parse(b"# branch.oid 1234567890abcdef\0# branch.head (detached)\0").unwrap();
        assert_eq!(
            status.head,
            GitHead::Detached {
                sha: "1234567".into()
            }
        );
        assert_eq!(status.tracking, None);
    }

    #[test]
    fn splits_changes_into_staged_and_unstaged() {
        let output = [
            "# branch.oid abc",
            "# branch.head main",
            "1 M. N... 100644 100644 100644 aaa bbb staged.txt",
            "1 .M N... 100644 100644 100644 aaa bbb unstaged.txt",
            "1 MM N... 100644 100644 100644 aaa bbb both.txt",
            "1 A. N... 000000 100644 100644 000 bbb new file.txt",
            "1 .D N... 100644 100644 000000 aaa aaa gone.txt",
            "1 .T N... 100644 100644 120000 aaa aaa link",
            "2 R. N... 100644 100644 100644 aaa aaa R100 renamed.txt",
            "original.txt",
            "u UU N... 100644 100644 100644 100644 aaa bbb ccc conflict.txt",
            "? untracked.txt",
            "",
        ]
        .join("\0");
        let status = parse(output.as_bytes()).unwrap();
        assert_eq!(
            status.staged,
            [
                change("staged.txt", GitChangeKind::Modified, None),
                change("both.txt", GitChangeKind::Modified, None),
                change("new file.txt", GitChangeKind::Added, None),
                change("renamed.txt", GitChangeKind::Renamed, Some("original.txt")),
            ]
        );
        assert_eq!(
            status.unstaged,
            [
                change("unstaged.txt", GitChangeKind::Modified, None),
                change("both.txt", GitChangeKind::Modified, None),
                change("gone.txt", GitChangeKind::Deleted, None),
                change("link", GitChangeKind::TypeChanged, None),
                change("conflict.txt", GitChangeKind::Conflicted, None),
            ]
        );
        assert_eq!(status.untracked, ["untracked.txt"]);
    }

    #[test]
    fn output_without_a_branch_is_an_error() {
        assert!(parse(b"? a.txt\0").is_err());
    }

    #[test]
    fn a_truncated_rename_is_an_error() {
        let output = "# branch.oid abc\0# branch.head main\x002 R. N... 100644 100644 100644 aaa aaa R100 renamed.txt";
        assert!(parse(output.as_bytes()).is_err());
    }
}
