//! Git probing for the preview panel.
//!
//! The picker never shells out just to find a repository: locating the repo and
//! reading the branch are plain filesystem reads, and only the dirty check pays
//! for a `git` subprocess (with a timeout, because the picker cannot afford to
//! stall on one).

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

const GIT_DIRTY_TIMEOUT: Duration = Duration::from_millis(300);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct GitInfo {
    pub(super) branch: String,
    pub(super) dirty: Option<bool>,
}

pub(super) fn read_git_info(path: &Path) -> Option<GitInfo> {
    let (repo_root, git_dir) = find_git_repo(path)?;
    let head = fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let branch = parse_git_head_branch(&head)?;
    Some(GitInfo {
        branch,
        dirty: read_git_dirty(&repo_root, GIT_DIRTY_TIMEOUT),
    })
}

/// Locate a repository without starting a `git` process. A `.git` directory is
/// the normal case; a `.git` file covers worktrees and submodules.
fn find_git_repo(start: &Path) -> Option<(PathBuf, PathBuf)> {
    for ancestor in start.ancestors() {
        let marker = ancestor.join(".git");
        if marker.is_dir() {
            return Some((ancestor.to_path_buf(), marker));
        }
        if marker.is_file() {
            let content = fs::read_to_string(&marker).ok()?;
            let target = content.trim().strip_prefix("gitdir: ")?;
            let target = Path::new(target);
            let git_dir = if target.is_absolute() {
                target.to_path_buf()
            } else {
                marker.parent()?.join(target)
            };
            if git_dir.is_dir() {
                return Some((ancestor.to_path_buf(), git_dir));
            }
        }
    }
    None
}

fn parse_git_head_branch(head: &str) -> Option<String> {
    let head = head.trim();
    if let Some(branch) = head.strip_prefix("ref: refs/heads/") {
        return Some(branch.to_string());
    }
    (!head.is_empty()).then(|| "detached".to_string())
}

fn read_git_dirty(repo_root: &Path, timeout: Duration) -> Option<bool> {
    // Spawn `git status` with a piped stdout so a slow filesystem (WSL2, network
    // mounts) can't stall the picker. On timeout we kill the child and return,
    // and the reader thread drains the pipe so neither the process nor the
    // thread lingers after `git` finally exits.
    let mut child = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(repo_root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    let mut stdout = child.stdout.take()?;
    let (tx, rx) = mpsc::channel();
    let reader = thread::spawn(move || {
        let mut buffer = Vec::new();
        let read = stdout.read_to_end(&mut buffer);
        let _ = tx.send(read.map(|_| !buffer.is_empty()));
    });

    let dirty = match rx.recv_timeout(timeout) {
        Ok(read_result) => child
            .wait()
            .ok()
            .and_then(|status| status.success().then_some(()).and(read_result.ok())),
        Err(_) => {
            // Timed out: kill the child so `read_to_end` returns and the reader
            // thread unblocks; joining keeps the pipe alive until then.
            let _ = child.kill();
            let _ = child.wait();
            None
        }
    };

    let _ = reader.join();
    dirty
}
