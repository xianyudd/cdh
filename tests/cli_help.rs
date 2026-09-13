//! `cdh -h/--help` 的端到端双语契约：跑真实二进制、受控 XDG 环境、
//! 断言 stderr 内容与语言。沿用 `tui_settings_non_tty.rs` 的
//! `CARGO_BIN_EXE_cdh` + `env_clear()` 子进程先例——语言链的每一档
//! （locale / tui.toml / CDH_LANG）都靠注入子进程环境变量驱动，
//! 进程级隔离，不需要全局串行，也不会与并行单测互相踩环境。

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static NEXT_TEST_DIR: AtomicU64 = AtomicU64::new(0);

struct TestDir {
    root: PathBuf,
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

/// 在受控 XDG 环境里跑一次 `cdh -h`，返回 (exit code, stderr)。
///
/// `settings` 是写入 `tui.toml` 的内容（None = 不创建文件）；
/// `cdh_lang` 是注入的 `CDH_LANG`（None = 不设置）。
fn help_output(
    name: &str,
    settings: Option<&str>,
    lang: &str,
    cdh_lang: Option<&str>,
) -> (Option<i32>, String) {
    let test_dir = TestDir::new(name);
    let home = test_dir.root.join("home");
    let config_home = test_dir.root.join("config");
    let data_home = test_dir.root.join("data");
    let state_home = test_dir.root.join("state");
    let cache_home = test_dir.root.join("cache");
    let settings_dir = config_home.join("cdh");

    for directory in [
        &home,
        &config_home,
        &data_home,
        &state_home,
        &cache_home,
        &settings_dir,
    ] {
        fs::create_dir_all(directory).unwrap();
    }
    if let Some(contents) = settings {
        fs::write(settings_dir.join("tui.toml"), contents).unwrap();
    }

    let mut command = Command::new(env!("CARGO_BIN_EXE_cdh"));
    command
        .arg("-h")
        .env_clear()
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_DATA_HOME", &data_home)
        .env("XDG_STATE_HOME", &state_home)
        .env("XDG_CACHE_HOME", &cache_home)
        .env("LANG", lang)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(value) = cdh_lang {
        command.env("CDH_LANG", value);
    }
    let output = command.output().unwrap();
    (
        output.status.code(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
fn help_stderr_localizes_through_the_full_preference_chain() {
    // (a) locale 驱动中文：断言的是中文描述行，不只是两语言相同的旗标名。
    let (code, zh) = help_output("help-locale-zh", None, "zh_CN.UTF-8", None);
    assert_eq!(code, Some(0), "cdh -h must exit 0; stderr: {zh}");
    assert!(zh.contains("用法:"), "expected Chinese help, got: {zh}");
    assert!(zh.contains("显示版本并退出"), "missing zh copy: {zh}");
    assert!(
        zh.contains("记录一次目录访问"),
        "missing zh subcommand copy: {zh}"
    );
    assert!(
        !zh.contains("Usage:"),
        "zh help must not carry English usage: {zh}"
    );

    // (b) locale 驱动英文。
    let (code, en) = help_output("help-locale-en", None, "en_US.UTF-8", None);
    assert_eq!(code, Some(0), "cdh -h must exit 0; stderr: {en}");
    assert!(en.contains("Usage:"), "expected English help, got: {en}");
    assert!(
        en.contains("Print the version and exit"),
        "missing en copy: {en}"
    );
    assert!(
        !en.contains("用法:"),
        "en help must not carry Chinese usage: {en}"
    );

    // (c) tui.toml 保存的偏好覆盖 locale。
    let (code, saved) = help_output(
        "help-saved-en",
        Some("language = \"en\"\n"),
        "zh_CN.UTF-8",
        None,
    );
    assert_eq!(code, Some(0), "cdh -h must exit 0; stderr: {saved}");
    assert!(
        saved.contains("Usage:"),
        "saved 'en' must beat zh locale: {saved}"
    );
    assert!(
        !saved.contains("用法:"),
        "saved 'en' must beat zh locale: {saved}"
    );

    // (d) CDH_LANG 覆盖 tui.toml 与 locale——这正是 picker::cli_help_text 里
    // UiEnvironment::from_process() 胶水的承重断言。
    let (code, pinned) = help_output(
        "help-cdh-lang-zh",
        Some("language = \"en\"\n"),
        "en_US.UTF-8",
        Some("zh-CN"),
    );
    assert_eq!(code, Some(0), "cdh -h must exit 0; stderr: {pinned}");
    assert!(
        pinned.contains("用法:"),
        "CDH_LANG=zh-CN must beat tui.toml 'en' and en locale: {pinned}"
    );
    assert!(
        !pinned.contains("Usage:"),
        "CDH_LANG=zh-CN must win: {pinned}"
    );

    // (e) 损坏的 tui.toml 退回默认偏好（auto → locale），帮助照常打印。
    let (code, broken) = help_output(
        "help-broken-toml",
        Some("language = [\n"),
        "zh_CN.UTF-8",
        None,
    );
    assert_eq!(
        code,
        Some(0),
        "cdh -h must exit 0 even with broken tui.toml; stderr: {broken}"
    );
    assert!(
        broken.contains("用法:"),
        "broken tui.toml must fall back to locale, got: {broken}"
    );
}
