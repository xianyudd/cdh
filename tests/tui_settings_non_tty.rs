use std::fs;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static NEXT_TEST_DIR: AtomicU64 = AtomicU64::new(0);

struct TestDir {
    root: std::path::PathBuf,
}

impl TestDir {
    fn new(name: &str) -> Self {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let sequence = NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "cdh-{name}-{}-{timestamp}-{sequence}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        Self { root }
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[derive(Debug, PartialEq)]
struct FileMetadata {
    len: u64,
    modified: Option<SystemTime>,
    readonly: bool,
}

fn file_metadata(path: &std::path::Path) -> FileMetadata {
    let metadata = fs::metadata(path).unwrap();
    FileMetadata {
        len: metadata.len(),
        modified: metadata.modified().ok(),
        readonly: metadata.permissions().readonly(),
    }
}

#[test]
fn settings_non_tty_skips_settings_and_prints_first_recommendation() {
    let test_dir = TestDir::new("settings-non-tty");
    let home = test_dir.root.join("home");
    let config_home = test_dir.root.join("config");
    let data_home = test_dir.root.join("data");
    let state_home = test_dir.root.join("state");
    let cache_home = test_dir.root.join("cache");
    let current_dir = test_dir.root.join("current");
    let candidate = test_dir.root.join("candidate");
    let history_dir = data_home.join("cdh/history");
    let settings_dir = config_home.join("cdh");
    let settings_path = settings_dir.join("tui.toml");

    for directory in [
        &home,
        &config_home,
        &data_home,
        &state_home,
        &cache_home,
        &current_dir,
        &candidate,
        &history_dir,
        &settings_dir,
    ] {
        fs::create_dir_all(directory).unwrap();
    }

    fs::write(
        history_dir.join("history_raw"),
        format!("1\t{}\n", candidate.display()),
    )
    .unwrap();
    fs::write(
        history_dir.join("history_uniq"),
        format!("{}\n", candidate.display()),
    )
    .unwrap();

    let malformed_settings = b"language = \"en\"\ncolor = [\n";
    fs::write(&settings_path, malformed_settings).unwrap();
    let settings_metadata_before = file_metadata(&settings_path);

    let output = Command::new(env!("CARGO_BIN_EXE_cdh"))
        .current_dir(&current_dir)
        .env_clear()
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_DATA_HOME", &data_home)
        .env("XDG_STATE_HOME", &state_home)
        .env("XDG_CACHE_HOME", &cache_home)
        .env("LANG", "C.UTF-8")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "cdh failed with status {:?}: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, candidate.as_os_str().as_encoded_bytes());

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("TUI settings") && !stderr.contains("tui.toml"),
        "non-TTY execution emitted a settings warning: {stderr}"
    );
    assert_eq!(fs::read(&settings_path).unwrap(), malformed_settings);
    assert_eq!(file_metadata(&settings_path), settings_metadata_before);
}

/// The directory-tree discovery layer must not change the non-interactive
/// contract: with an empty history and no TTY, `cdh` still exits 2 (no
/// candidates) and prints nothing. Discovery only ever runs interactively.
#[test]
fn empty_history_non_tty_still_exits_two() {
    let test_dir = TestDir::new("empty-non-tty");
    let home = test_dir.root.join("home");
    let config_home = test_dir.root.join("config");
    let data_home = test_dir.root.join("data");
    let state_home = test_dir.root.join("state");
    let cache_home = test_dir.root.join("cache");
    let current_dir = test_dir.root.join("current");
    let history_dir = data_home.join("cdh/history");

    for directory in [
        &home,
        &config_home,
        &data_home,
        &state_home,
        &cache_home,
        &current_dir,
        &history_dir,
    ] {
        fs::create_dir_all(directory).unwrap();
    }

    // Empty history files: no recommendations at all.
    fs::write(history_dir.join("history_raw"), b"").unwrap();
    fs::write(history_dir.join("history_uniq"), b"").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_cdh"))
        .current_dir(&current_dir)
        .env_clear()
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_DATA_HOME", &data_home)
        .env("XDG_STATE_HOME", &state_home)
        .env("XDG_CACHE_HOME", &cache_home)
        .env("LANG", "C.UTF-8")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap();

    assert_eq!(
        output.status.code(),
        Some(2),
        "empty history without a TTY must exit 2; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.is_empty());
}
