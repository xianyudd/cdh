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
#[cfg(test)]
use std::sync::Mutex;
use std::thread;
use std::time::Duration;

/// Every `git` subprocess in this module goes through `git_command`, so the
/// spawned child never inherits a caller's GIT_* configuration. Incident that
/// motivates this: a git hook exported `GIT_DIR`, `cargo test` inherited it,
/// and a test's `git init` re-initialized the host repository and corrupted
/// `core.bare` instead of initializing its own temporary directory.
fn git_command(args: &[&str]) -> Command {
    let mut command = Command::new("git");
    command.args(args);
    for variable in [
        "GIT_DIR",
        "GIT_INDEX_FILE",
        "GIT_WORK_TREE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_CONFIG",
        "GIT_CONFIG_GLOBAL",
        "GIT_CONFIG_SYSTEM",
        "GIT_CONFIG_PARAMETERS",
    ] {
        command.env_remove(variable);
    }
    command
}

/// Serializes tests that spawn `git` while the process environment carries an
/// injected `GIT_DIR`: the variable is process-wide, so an unguarded concurrent
/// spawn in another test would reproduce the leak `git_command` prevents.
#[cfg(test)]
pub(super) static GIT_ENV_LOCK: Mutex<()> = Mutex::new(());

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
    let mut child = git_command(&["status", "--porcelain"])
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_root(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("cdh_picker_git_test_{name}_{unique}"))
    }

    /// The dirty check must judge the directory it was given, not whatever
    /// `GIT_DIR` points at. Unhardened, `git status` against a bare decoy repo
    /// fails with "this operation must be run in a work tree" and the dirty
    /// state silently degrades to `None`; hardened, it sees the real worktree.
    #[test]
    fn read_git_dirty_ignores_injected_git_dir() {
        if Command::new("git").arg("--version").output().is_err() {
            return;
        }
        let _guard = GIT_ENV_LOCK.lock().unwrap();
        let root = temp_root("git_dir_injection");
        let repo = root.join("repo");
        let decoy = root.join("decoy.git");
        fs::create_dir_all(&repo).unwrap();
        assert!(Command::new("git")
            .args(["init", "-q"])
            .current_dir(&repo)
            .status()
            .unwrap()
            .success());
        assert!(Command::new("git")
            .args(["init", "-q", "--bare"])
            .arg(&decoy)
            .status()
            .unwrap()
            .success());
        fs::write(repo.join("note.txt"), "dirty").unwrap();
        let decoy_config_before = fs::read(decoy.join("config")).unwrap();

        // Same situation as a git hook exporting GIT_DIR into cdh's
        // environment: without the hardening the spawned `git status` would
        // target the decoy instead of `repo`.
        env::set_var("GIT_DIR", &decoy);
        let dirty = read_git_dirty(&repo, Duration::from_secs(5));
        env::remove_var("GIT_DIR");

        assert_eq!(dirty, Some(true));
        assert_eq!(fs::read(decoy.join("config")).unwrap(), decoy_config_before);
        let _ = fs::remove_dir_all(&root);
    }

    /// Pins the exact set of variables `git_command` strips: dropping one
    /// `env_remove` here would reopen the corresponding injection path.
    #[test]
    fn git_command_strips_injected_git_environment() {
        let command = git_command(&["status", "--porcelain"]);
        let environment: std::collections::BTreeMap<String, Option<&std::ffi::OsStr>> = command
            .get_envs()
            .map(|(key, value)| (key.to_string_lossy().into_owned(), value))
            .collect();
        for variable in [
            "GIT_DIR",
            "GIT_INDEX_FILE",
            "GIT_WORK_TREE",
            "GIT_OBJECT_DIRECTORY",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            "GIT_CONFIG",
            "GIT_CONFIG_GLOBAL",
            "GIT_CONFIG_SYSTEM",
            "GIT_CONFIG_PARAMETERS",
        ] {
            assert_eq!(environment.get(variable), Some(&None), "{variable}");
        }
        assert!(!environment.contains_key("GIT_AUTHOR_NAME"));
    }
}
