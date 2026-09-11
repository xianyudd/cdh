//! Test-only helpers shared across the crate's unit tests.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// A unique scratch path under the system temp dir, removed when the guard
/// drops.
///
/// Tests used to delete their scratch trees by hand at the end of the happy
/// path; a failed assertion or panic skipped that line and leaked `cdh_*`
/// directories into `/tmp` (issue #30). `TempDir` cleans up from `Drop`
/// instead, so the directory is removed on the happy path, on a failed
/// assertion, and while a panic unwinds the stack.
///
/// The directory itself is created lazily by callers -- a few tests assert it
/// is absent, or build several nested subdirs -- but cleanup always runs.
/// `Deref`/`AsRef<Path>` make it a drop-in stand-in for the `PathBuf` roots
/// these tests passed around before.
pub(crate) struct TempDir {
    path: PathBuf,
}

static SEQ: AtomicU64 = AtomicU64::new(0);

impl TempDir {
    /// Reserve a fresh `cdh_<label>_<pid>_<nanos>_<seq>` path. The pid,
    /// timestamp and process-global counter together keep parallel tests from
    /// colliding even when the clock is coarse (e.g. under WSL).
    pub(crate) fn new(label: &str) -> TempDir {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("cdh_{label}_{}_{nanos}_{seq}", std::process::id()));
        TempDir { path }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

impl std::ops::Deref for TempDir {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.path
    }
}

impl AsRef<Path> for TempDir {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::panic;

    #[test]
    fn temp_dir_is_removed_even_when_a_test_panics_mid_scope() {
        // Regression for issue #30: cleanup must ride on `Drop`, not on a line
        // at the end of the happy path. Create the directory, panic while the
        // guard is live, then assert the directory is gone -- which can only
        // hold if `Drop` ran during unwind. The panic is intentional;
        // `catch_unwind` swallows it (one "thread panicked" line on stderr is
        // expected and not a failure).
        let mut observed = PathBuf::new();
        let result = panic::catch_unwind(panic::AssertUnwindSafe(|| {
            let temp = TempDir::new("panic_safety_probe");
            observed = temp.path().to_path_buf();
            std::fs::create_dir_all(&observed).unwrap();
            assert!(observed.is_dir(), "probe dir should exist before the panic");
            panic!("intentional panic while the TempDir guard is live");
        }));

        assert!(result.is_err(), "the probe closure must have panicked");
        assert!(
            !observed.as_os_str().is_empty(),
            "the probe must have recorded its path before panicking"
        );
        assert!(
            !observed.exists(),
            "TempDir must delete {} while the panic unwinds the stack",
            observed.display()
        );
    }
}
