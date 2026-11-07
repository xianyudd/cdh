//! PTY 集成测试：把 cdh 挂到真实 tty 上，驱动 crossterm 的输入路径（issue #34）。
//!
//! 与已有两层覆盖的分工：
//! - `src/picker.rs` 的 dispatch 单测喂的是**已构造的** `crossterm::Event`，
//!   不经过字节解析；
//! - golden 渲染单测钉的是输出帧字节（CrosstermBackend<Vec<u8>>）。
//!
//! 这里的测试补中间一层：PTY 字节 → `event::poll`/`event::read` 解码 →
//! cdh 键位/鼠标映射 → 退出语义。每次 crossterm 升级，「解码层语义没变」
//! 只能由这条路径证明。
//!
//! 进程拓扑对齐 shell 集成的真实用法：stdin/stderr = pty slave（字节流经
//! 真实 tty，cdh 以 `stderr+stdin 均为终端` 判定交互模式），stdout = 普通
//! 文件（真实用法里 `$(cdh)` 的 stdout 也是管道捕获）。portable-pty 的
//! `spawn_command` 会把三个标准流全接到 slave，所以借 `/bin/sh -c` 的
//! `exec` 重定向把 stdout 引到文件。
//!
//! 历史夹具格式核实自 `src/history.rs`：history_raw 每行 `<ts_secs>\t<abs_path>`，
//! history_uniq 每行一个 path、从旧到新。时间戳取远古值让 frecency/recency/
//! 转移全部衰减到 0，排序只剩 uniq 名次信号（alpha 最新 → 恒为第 1 行，
//! beta 第 2 行），与运行时刻无关。

use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// `TermGuard::enter` 的收尾字节（EnableMouseCapture 的 SGR 模式段）。
/// 输出流里出现它 = 原始模式已就绪、事件循环即将开始，可以安全注入输入。
const MOUSE_CAPTURE_ENABLE: &[u8] = b"\x1b[?1006h";
/// 退出时的 DisableMouseCapture（SGR 段）与 LeaveAlternateScreen。
const MOUSE_CAPTURE_DISABLE: &[u8] = b"\x1b[?1006l";
const ENTER_ALTERNATE_SCREEN: &[u8] = b"\x1b[?1049h";
const LEAVE_ALTERNATE_SCREEN: &[u8] = b"\x1b[?1049l";

const COLS: u16 = 80;
const ROWS: u16 = 24;

/// 列表首行在屏幕上的 0 基行号。来自 `screen_layout` 的固定分段：
/// header=0、输入框=1、上分隔线=2、列表从 3 开始（与终端高度无关）。
/// 精确的「行 → 结果下标」映射已由 dispatch 单测覆盖，这里只需要
/// 一个几何上稳定的行来证明字节 → 事件 → 分发的连通性。
const LIST_FIRST_ROW: u16 = 3;
/// 鼠标点击的 0 基列号：列表区内、远离右侧环境光动效留白的左段。
const CLICK_COLUMN: u16 = 4;
/// 点击的列表行下标：第 2 行（beta），与默认选中的第 1 行（alpha）区分，
/// 证明点击真的移动了选择。
const CLICK_LIST_INDEX: u16 = 1;

/// 单步等待上限：CI 慢机器余量。所有等待都是轮询条件而非固定 sleep。
const STEP_TIMEOUT: Duration = Duration::from_secs(10);
const EXIT_TIMEOUT: Duration = Duration::from_secs(20);
const POLL_INTERVAL: Duration = Duration::from_millis(25);

/// 测试用临时根目录，`Drop` 时整棵删掉（模式与 tests/tui_settings_non_tty.rs
/// 的 TestDir 一致：src/test_support.rs 是 #[cfg(test)] 的 lib 内设施，
/// 集成测试用不了，就近自带一份）。
struct TestDir {
    root: PathBuf,
}

static NEXT_TEST_DIR: AtomicU64 = AtomicU64::new(0);

impl TestDir {
    fn new(name: &str) -> Self {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let sequence = NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed);
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("scratch")
            .join(format!(
                "cdh-pty-{name}-{}-{timestamp}-{sequence}",
                process::id()
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

/// 夹具：两个已知目录。列表恒为 [alpha, beta]（见文件头注释）。
struct Fixture {
    alpha: PathBuf,
    beta: PathBuf,
}

impl Fixture {
    fn alpha_bytes(&self) -> Vec<u8> {
        path_bytes(&self.alpha)
    }

    fn beta_bytes(&self) -> Vec<u8> {
        path_bytes(&self.beta)
    }
}

fn path_bytes(path: &Path) -> Vec<u8> {
    path.as_os_str().as_encoded_bytes().to_vec()
}

/// master 侧输出流的共享快照：后台线程持续汇入字节，子进程退出、
/// slave 全部关闭后置 `closed`。
#[derive(Default)]
struct StreamState {
    bytes: Vec<u8>,
    closed: bool,
}

/// 一个挂在 PTY 上的 cdh 会话。`Drop` 兜底 kill 残余子进程，保证
/// 测试失败路径也不留 cdh 进程。
struct PtySession {
    child: Option<Box<dyn Child + Send + Sync>>,
    writer: Box<dyn Write + Send>,
    _master: Box<dyn MasterPty + Send>,
    state: Arc<Mutex<StreamState>>,
    stdout_path: PathBuf,
}

fn drain_pty(mut reader: Box<dyn Read + Send>, state: Arc<Mutex<StreamState>>) {
    let mut buf = [0u8; 4096];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => state.lock().unwrap().bytes.extend_from_slice(&buf[..n]),
            // master 读到 EIO = slave 端已全部关闭，按 EOF 处理。
            Err(_) => break,
        }
    }
    state.lock().unwrap().closed = true;
}

fn stream_contains(state: &Mutex<StreamState>, needle: &[u8]) -> bool {
    let state = state.lock().unwrap();
    state
        .bytes
        .windows(needle.len())
        .any(|window| window == needle)
}

// Ratatui emits cursor-addressed diffs, not complete frames: stripping ANSI would
// miss retained cells and whitespace. Replay its absolute-address/style subset.
fn stream_plain_contains(state: &Mutex<StreamState>, needle: &[u8]) -> bool {
    use unicode_width::UnicodeWidthChar;
    let state = state.lock().unwrap();
    let text = String::from_utf8_lossy(&state.bytes);
    let mut chars = text.chars().peekable();
    let mut cells = vec![vec![" ".to_string(); COLS as usize]; ROWS as usize];
    let (mut row, mut col) = (0usize, 0usize);
    while let Some(ch) = chars.next() {
        if ch == '\x1b' && chars.peek() == Some(&'[') {
            chars.next();
            let mut args = String::new();
            let mut final_char = None;
            for ch in chars.by_ref() {
                if ('@'..='~').contains(&ch) {
                    final_char = Some(ch);
                    break;
                }
                args.push(ch);
            }
            let numbers: Vec<usize> = args.split(';').map(|n| n.parse().unwrap_or(0)).collect();
            match final_char {
                Some('H' | 'f') => {
                    row = numbers.first().copied().unwrap_or(1).max(1) - 1;
                    col = numbers.get(1).copied().unwrap_or(1).max(1) - 1;
                }
                Some('J') if numbers.first() == Some(&2) => {
                    for line in &mut cells {
                        line.fill(" ".into());
                    }
                }
                _ => {} // styles and mode changes do not occupy cells
            }
        } else if ch == '\r' {
            col = 0;
        } else if ch == '\n' {
            row += 1;
        } else if !ch.is_control() {
            let width = ch.width().unwrap_or(0);
            if row < cells.len() && col < COLS as usize {
                if width == 0 && col > 0 {
                    cells[row][col - 1].push(ch);
                } else {
                    cells[row][col] = ch.to_string();
                    if width == 2 && col + 1 < COLS as usize {
                        cells[row][col + 1].clear();
                    }
                }
            }
            col += width;
        }
    }
    let screen = cells
        .into_iter()
        .map(|line| line.concat())
        .collect::<Vec<_>>()
        .join("\n");
    screen
        .as_bytes()
        .windows(needle.len())
        .any(|part| part == needle)
}

#[test]
fn terminal_snapshot_preserves_spaces_unicode_and_incremental_overwrites() {
    let state = Mutex::new(StreamState {
        bytes: b"\x1b[1;1HPage 0/0\x1b[1;6H1\x1b[1;8H1\x1b[2;1Halpha\x1b[2;7Hbeta".to_vec(),
        closed: false,
    });
    assert!(stream_plain_contains(&state, b"Page 1/1"));
    assert!(stream_plain_contains(&state, b"alpha beta"));
    assert!(!stream_plain_contains(&state, b"Page 0/0"));
    state
        .lock()
        .unwrap()
        .bytes
        .extend_from_slice("\x1b[3;1H目录 space".as_bytes());
    assert!(stream_plain_contains(&state, "目录 space".as_bytes()));
}

fn stream_position(state: &Mutex<StreamState>, needle: &[u8]) -> Option<usize> {
    let state = state.lock().unwrap();
    state
        .bytes
        .windows(needle.len())
        .position(|window| window == needle)
}

fn debug_tail(state: &Mutex<StreamState>) -> String {
    let state = state.lock().unwrap();
    let len = state.bytes.len();
    let start = len.saturating_sub(256);
    format!("{:?}", &state.bytes[start..])
}

impl PtySession {
    /// 等到 TermGuard::enter 完成（鼠标捕获字节出现）：此后 tty 已在原始
    /// 模式，注入的按键不会再被行缓冲或回显。
    fn wait_ready(&self) {
        let deadline = Instant::now() + STEP_TIMEOUT;
        while !stream_contains(&self.state, MOUSE_CAPTURE_ENABLE) {
            assert!(
                Instant::now() < deadline,
                "cdh 未在 {:?} 内完成终端初始化；输出尾部: {}",
                STEP_TIMEOUT,
                debug_tail(&self.state)
            );
            thread::sleep(POLL_INTERVAL);
        }
    }

    /// 等输出流关闭（子进程退出、slave fd 全部关闭）。之后快照才是完整的。
    fn wait_stream_closed(&self) {
        let deadline = Instant::now() + STEP_TIMEOUT;
        while !self.state.lock().unwrap().closed {
            assert!(
                Instant::now() < deadline,
                "PTY 输出流未在 {:?} 内关闭；输出尾部: {}",
                STEP_TIMEOUT,
                debug_tail(&self.state)
            );
            thread::sleep(POLL_INTERVAL);
        }
    }

    fn send(&mut self, bytes: &[u8]) {
        self.writer.write_all(bytes).expect("写入 pty master");
        self.writer.flush().expect("flush pty master");
    }

    fn wait_exit(&mut self) -> portable_pty::ExitStatus {
        let deadline = Instant::now() + EXIT_TIMEOUT;
        loop {
            if let Some(status) = self
                .child
                .as_mut()
                .expect("child 已被取走")
                .try_wait()
                .expect("try_wait cdh")
            {
                return status;
            }
            if Instant::now() >= deadline {
                let _ = self.child.as_mut().expect("child 已被取走").kill();
                panic!(
                    "cdh 未在 {:?} 内退出；输出尾部: {}",
                    EXIT_TIMEOUT,
                    debug_tail(&self.state)
                );
            }
            thread::sleep(POLL_INTERVAL);
        }
    }

    /// cdh 的 stdout（选中的路径，不带换行；未选中为空）。
    fn selection(&self) -> Vec<u8> {
        fs::read(&self.stdout_path).unwrap_or_default()
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            // 已退出的进程 kill 会报错，忽略；这里兜的是断言失败后的残余进程。
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn spawn_cdh_on_pty(label: &str) -> (TestDir, Fixture, PtySession) {
    spawn_search_on_pty(label, &[], false)
}

fn spawn_search_on_pty(
    label: &str,
    args: &[&str],
    discovery: bool,
) -> (TestDir, Fixture, PtySession) {
    spawn_search_options(label, args, discovery, false)
}

fn spawn_search_options(
    label: &str,
    args: &[&str],
    discovery: bool,
    protect: bool,
) -> (TestDir, Fixture, PtySession) {
    spawn_search_fixture(label, args, discovery, protect, |_| {})
}

fn spawn_search_fixture(
    label: &str,
    args: &[&str],
    discovery: bool,
    protect: bool,
    prepare: impl FnOnce(&Path),
) -> (TestDir, Fixture, PtySession) {
    let test_dir = TestDir::new(label);
    let home = test_dir.root.join("home");
    let config_home = test_dir.root.join("config");
    let data_home = test_dir.root.join("data");
    let state_home = test_dir.root.join("state");
    let cache_home = test_dir.root.join("cache");
    let current_dir = test_dir.root.join("cwd");
    let dirs = test_dir.root.join("dirs");
    let alpha = dirs.join("alpha");
    let beta = dirs.join("beta");
    let history_dir = data_home.join("cdh").join("history");

    for directory in [
        &home,
        &config_home,
        &data_home,
        &state_home,
        &cache_home,
        &current_dir,
        &alpha,
        &beta,
        &history_dir,
    ] {
        fs::create_dir_all(directory).unwrap();
    }

    // history_raw：`<ts_secs>\t<abs_path>`；history_uniq：从旧到新。
    // 远古时间戳 + uniq 尾行 = alpha 最新 → 列表恒为 [alpha, beta]。
    fs::write(
        history_dir.join("history_raw"),
        format!("1000\t{}\n1000\t{}\n", alpha.display(), beta.display()),
    )
    .unwrap();
    fs::write(
        history_dir.join("history_uniq"),
        format!("{}\n{}\n", beta.display(), alpha.display()),
    )
    .unwrap();

    if discovery {
        fs::create_dir_all(dirs.join("needle")).unwrap();
        fs::create_dir_all(home.join("needle-ancestor/child")).unwrap();
        // HOME hit alone cannot reveal the never-visited external sibling.
        fs::write(
            history_dir.join("history_uniq"),
            format!(
                "{}\n{}\n",
                alpha.display(),
                home.join("needle-ancestor/child").display()
            ),
        )
        .unwrap();
    }
    fs::create_dir_all(dirs.join("alpha beta")).unwrap();
    if args.contains(&"alpha beta") {
        fs::write(
            history_dir.join("history_uniq"),
            format!(
                "{}\n{}\n{}\n",
                alpha.display(),
                beta.display(),
                dirs.join("alpha beta").display()
            ),
        )
        .unwrap();
    }
    if protect || args.contains(&"/needle$") {
        fs::create_dir_all(dirs.join("needle-allowed")).unwrap();
    }
    if protect {
        fs::write(
            data_home.join("cdh/excludes"),
            format!("{}\n", dirs.join("needle").display()),
        )
        .unwrap();
    }

    prepare(&test_dir.root);
    let stdout_path = test_dir.root.join("stdout");

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: ROWS,
            cols: COLS,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();

    let mut cmd = CommandBuilder::new("/bin/sh");
    cmd.arg("-c");
    cmd.arg("out=$1; shift; exec \"$0\" \"$@\" >\"$out\"");
    cmd.arg(env!("CARGO_BIN_EXE_cdh"));
    cmd.arg(&stdout_path);
    for arg in args {
        if *arg == "{absolute-target}" {
            fs::create_dir_all(home.join("目录 space")).unwrap();
            cmd.arg(home.join("目录 space"));
        } else {
            cmd.arg(arg);
        }
    }
    if args.contains(&"~/目录 space") || args.contains(&"../home/目录 space") {
        fs::create_dir_all(home.join("目录 space")).unwrap();
    }
    cmd.cwd(&current_dir);
    // 全新环境：路径全指临时目录，可能泄进来的 CDH_* 一并清掉。
    cmd.env_clear();
    cmd.env("HOME", &home);
    cmd.env("XDG_CONFIG_HOME", &config_home);
    cmd.env("XDG_DATA_HOME", &data_home);
    cmd.env("XDG_STATE_HOME", &state_home);
    cmd.env("XDG_CACHE_HOME", &cache_home);
    cmd.env("LANG", "C.UTF-8");
    cmd.env("LC_ALL", "C.UTF-8");
    cmd.env("TERM", "xterm-256color");
    // 候选池钉死为夹具两行：发现层会异步并入扫描结果、搅动列表排序，
    // 而这里要钉的是输入路径。发现层有自己的契约测试。
    cmd.env("CDH_DISCOVER", if discovery { "1" } else { "0" });
    cmd.env("CDH_CORNER_3D", "0");
    cmd.env("CDH_COLOR", "0");

    let child = pair.slave.spawn_command(cmd).unwrap();
    // 父进程侧的 slave 句柄关掉：子进程持有自己的副本，父侧不关的话
    // 子进程退出后 master 读不到 EOF，汇流线程收不了尾。
    drop(pair.slave);

    let reader = pair.master.try_clone_reader().unwrap();
    let writer = pair.master.take_writer().unwrap();
    let state = Arc::new(Mutex::new(StreamState::default()));
    let drain_state = Arc::clone(&state);
    thread::spawn(move || drain_pty(reader, drain_state));

    (
        test_dir,
        Fixture { alpha, beta },
        PtySession {
            child: Some(child),
            writer,
            _master: pair.master,
            state,
            stdout_path,
        },
    )
}

#[test]
fn absolute_tilde_relative_unicode_space_paths_select_literal_target_on_pty() {
    for query in ["{absolute-target}", "~/目录 space", "../home/目录 space"] {
        let (dir, _fixture, mut session) = spawn_search_on_pty("path-intent", &[query], true);
        session.wait_ready();
        let deadline = Instant::now() + STEP_TIMEOUT;
        while !stream_plain_contains(&session.state, b"1/1") {
            assert!(
                Instant::now() < deadline,
                "literal target never reached first page: {:?}",
                String::from_utf8_lossy(&session.state.lock().unwrap().bytes)
            );
            thread::sleep(POLL_INTERVAL);
        }
        session.send(b"\x1b[H\r");
        assert_eq!(session.wait_exit().exit_code(), 0);
        assert_eq!(
            session.selection(),
            path_bytes(&dir.root.join("home/目录 space"))
        );
    }
}

#[test]
fn cli_multi_tokens_stay_or_and_quoted_spaces_stay_one_token_on_pty() {
    let (_dir, fixture, mut session) = spawn_search_on_pty("tokens", &["alpha", "beta"], false);
    session.wait_ready();
    let deadline = Instant::now() + STEP_TIMEOUT;
    while !stream_plain_contains(&session.state, b"/beta") {
        assert!(
            Instant::now() < deadline,
            "OR did not retain beta-only history"
        );
        thread::sleep(POLL_INTERVAL);
    }
    session.send(b"\x1b[B\r");
    assert_eq!(session.wait_exit().exit_code(), 0);
    assert_eq!(session.selection(), fixture.beta_bytes());
    let (_dir, fixture, mut session) = spawn_search_on_pty("quoted", &["alpha beta"], false);
    session.wait_ready();
    let deadline = Instant::now() + STEP_TIMEOUT;
    while !stream_plain_contains(&session.state, b"1/1")
        || !stream_plain_contains(&session.state, b"\"alpha beta\"")
    {
        assert!(Instant::now() < deadline, "quoted argv boundary missing");
        thread::sleep(POLL_INTERVAL);
    }
    session.send(b"\r");
    assert_eq!(session.wait_exit().exit_code(), 0);
    assert_eq!(
        session.selection(),
        path_bytes(&fixture.alpha.parent().unwrap().join("alpha beta"))
    );
}

#[test]
fn cli_ignore_override_and_excludes_protect_discovery_on_pty() {
    for protect in [false, true] {
        let args = if protect {
            vec!["needle"]
        } else {
            vec!["--ignore-re", "/needle$", "needle"]
        };
        let (dir, fixture, mut session) = spawn_search_options("protected", &args, true, protect);
        let history = dir.root.join("data/cdh/history/history_uniq");
        let before = fs::read(&history).unwrap();
        session.wait_ready();
        let deadline = Instant::now() + STEP_TIMEOUT;
        while !stream_plain_contains(&session.state, b"dirs/needle-allowed") {
            assert!(
                Instant::now() < deadline,
                "allowed discovery never reached picker"
            );
            thread::sleep(POLL_INTERVAL);
        }
        session.send(b"\x1b[H\r");
        assert_eq!(session.wait_exit().exit_code(), 0);
        assert_eq!(
            session.selection(),
            path_bytes(&fixture.alpha.parent().unwrap().join("needle-allowed"))
        );
        assert_eq!(fs::read(history).unwrap(), before);
        if protect {
            assert_eq!(
                fs::read_to_string(dir.root.join("data/cdh/excludes")).unwrap(),
                format!(
                    "{}\n",
                    fixture.alpha.parent().unwrap().join("needle").display()
                )
            );
        }
    }
}

#[test]
fn non_tty_query_remains_history_only_with_no_newline_or_state_changes() {
    let dir = TestDir::new("non-tty-search");
    let home = dir.root.join("home");
    let history = dir.root.join("data/cdh/history");
    let alpha = home.join("alpha");
    for path in [
        &alpha,
        &home.join("needle"),
        &history,
        &dir.root.join("cwd"),
    ] {
        fs::create_dir_all(path).unwrap();
    }
    fs::write(
        history.join("history_raw"),
        format!("1000\t{}\n", alpha.display()),
    )
    .unwrap();
    fs::write(
        history.join("history_uniq"),
        format!("{}\n", alpha.display()),
    )
    .unwrap();
    let before_raw = fs::read(history.join("history_raw")).unwrap();
    let before_uniq = fs::read(history.join("history_uniq")).unwrap();
    for (query, code, expected) in [("needle", 2, Vec::new()), ("alpha", 0, path_bytes(&alpha))] {
        let output = process::Command::new(env!("CARGO_BIN_EXE_cdh"))
            .arg(query)
            .env_clear()
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", dir.root.join("config"))
            .env("XDG_DATA_HOME", dir.root.join("data"))
            .env("XDG_STATE_HOME", dir.root.join("state"))
            .env("XDG_CACHE_HOME", dir.root.join("cache"))
            .env("CDH_DISCOVER", "1")
            .current_dir(dir.root.join("cwd"))
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(code));
        assert_eq!(output.stdout, expected);
    }
    assert_eq!(fs::read(history.join("history_raw")).unwrap(), before_raw);
    assert_eq!(fs::read(history.join("history_uniq")).unwrap(), before_uniq);
    assert!(!dir.root.join("config/cdh/tui.toml").exists());
    assert!(!dir.root.join("data/cdh/excludes").exists());
}

#[test]
fn search_regression_cli_query_is_visible_before_typing() {
    let (_dir, fixture, mut session) = spawn_search_on_pty("handoff", &["beta"], false);
    session.wait_ready();
    let deadline = Instant::now() + STEP_TIMEOUT;
    while !stream_contains(&session.state, b"OR") {
        assert!(Instant::now() < deadline, "CLI OR query marker missing");
        thread::sleep(POLL_INTERVAL);
    }
    session.send(b"\r");
    assert_eq!(session.wait_exit().exit_code(), 0);
    assert_eq!(session.selection(), fixture.beta_bytes());
}

#[test]
fn search_regression_discovery_survives_query_limit_and_threshold() {
    for args in [
        vec!["needle"],
        vec!["--limit", "1", "needle"],
        vec!["--threshold", "2", "needle"],
    ] {
        let (_dir, fixture, mut session) = spawn_search_on_pty("roots", &args, true);
        session.wait_ready();
        let deadline = Instant::now() + STEP_TIMEOUT;
        // Rendered basename is contiguous even with ANSI styles; full raw paths need not be.
        while !stream_contains(&session.state, b"needle") || !stream_contains(&session.state, b"OR")
        {
            assert!(Instant::now() < deadline, "initial CLI query missing");
            thread::sleep(POLL_INTERVAL);
        }
        // Wait for the visible discovered basename+parent to prove a batch reached the UI.
        while !stream_plain_contains(&session.state, b"dirs/needle") {
            assert!(
                Instant::now() < deadline,
                "external sibling was not discovered"
            );
            thread::sleep(POLL_INTERVAL);
        }
        // Discovery intentionally preserves the selected path, so explicitly move to the new top.
        session.send(b"\x1b[H\r");
        assert_eq!(session.wait_exit().exit_code(), 0);
        assert_eq!(
            session.selection(),
            path_bytes(&fixture.alpha.parent().unwrap().join("needle"))
        );
    }
}

#[test]
fn rediscovered_pruned_history_keeps_preference_on_pty() {
    for args in [
        vec!["target"], // positive control: identity was not pruned
        vec!["seed"],
        vec!["--limit", "1"],
        vec!["--threshold", "2", "target"],
    ] {
        let (dir, _fixture, mut session) =
            spawn_search_fixture("pruned-history", &args, true, false, |root| {
                let home = root.join("home");
                let seed = home.join("seed");
                let visited = home.join("long/deep/target");
                for path in [&seed, &visited, &home.join("target")] {
                    fs::create_dir_all(path).unwrap();
                }
                let history = root.join("data/cdh/history");
                fs::write(
                    history.join("history_raw"),
                    format!("100\t{}\n200\t{}\n", visited.display(), seed.display()),
                )
                .unwrap();
                fs::write(
                    history.join("history_uniq"),
                    format!("{}\n{}\n", visited.display(), seed.display()),
                )
                .unwrap();
            });
        let history = dir.root.join("data/cdh/history");
        let before_raw = fs::read(history.join("history_raw")).unwrap();
        let before_uniq = fs::read(history.join("history_uniq")).unwrap();
        session.wait_ready();
        session.send(b"\x15target"); // Ctrl+U clears CLI OR and searches interactively.
        let deadline = Instant::now() + STEP_TIMEOUT;
        while !stream_plain_contains(&session.state, b"long/deep/target")
            || !stream_plain_contains(&session.state, b"~/target")
        {
            assert!(
                Instant::now() < deadline,
                "both target paths must be visible before selection: {}",
                debug_tail(&session.state)
            );
            thread::sleep(POLL_INTERVAL);
        }
        session.send(b"\x1b[H\r");
        assert_eq!(session.wait_exit().exit_code(), 0);
        assert_eq!(session.selection(), path_bytes(&dir.root.join("home/long/deep/target")), "visited target must outrank equally strong shallow discovery after launch with {args:?}");
        assert_eq!(fs::read(history.join("history_raw")).unwrap(), before_raw);
        assert_eq!(fs::read(history.join("history_uniq")).unwrap(), before_uniq);
    }
}

fn unscanned_history_returns_on_pty(label: &str, discovery: bool, target: &str, symlink: bool) {
    for args in [
        vec![target],
        vec!["seed"],
        vec!["--limit", "1"],
        vec!["--threshold", "2", "seed"],
    ] {
        let (dir, _fixture, mut session) =
            spawn_search_fixture(label, &args, discovery, false, |root| {
                let home = root.join("home");
                let seed = home.join("seed");
                let visited = home.join(target);
                fs::create_dir_all(&seed).unwrap();
                if symlink {
                    fs::create_dir_all(root.join("link-destination")).unwrap();
                    #[cfg(unix)]
                    std::os::unix::fs::symlink(root.join("link-destination"), &visited).unwrap();
                } else {
                    fs::create_dir_all(&visited).unwrap();
                }
                let history = root.join("data/cdh/history");
                fs::write(
                    history.join("history_raw"),
                    format!("100\t{}\n200\t{}\n", visited.display(), seed.display()),
                )
                .unwrap();
                fs::write(
                    history.join("history_uniq"),
                    format!("{}\n{}\n", visited.display(), seed.display()),
                )
                .unwrap();
            });
        let history = dir.root.join("data/cdh/history");
        let before_raw = fs::read(history.join("history_raw")).unwrap();
        let before_uniq = fs::read(history.join("history_uniq")).unwrap();
        session.wait_ready();
        // The direct-query launch is a positive control for history eligibility.
        if args != vec![target] {
            session.send(format!("\x15{target}").as_bytes());
        }
        let deadline = Instant::now() + STEP_TIMEOUT;
        let visible = format!("~/{target}");
        while !stream_plain_contains(&session.state, visible.as_bytes()) {
            assert!(
                Instant::now() < deadline,
                "safe history must return with {args:?}: {}",
                debug_tail(&session.state)
            );
            thread::sleep(POLL_INTERVAL);
        }
        session.send(b"\x1b[H\r");
        assert_eq!(session.wait_exit().exit_code(), 0);
        assert_eq!(
            session.selection(),
            path_bytes(&dir.root.join("home").join(target))
        );
        assert_eq!(fs::read(history.join("history_raw")).unwrap(), before_raw);
        assert_eq!(fs::read(history.join("history_uniq")).unwrap(), before_uniq);
    }
}

#[test]
fn cleared_query_returns_history_with_discovery_disabled_on_pty() {
    unscanned_history_returns_on_pty("unscanned-disabled", false, "offline-target", false);
}

#[test]
fn cleared_query_returns_scanner_pruned_history_on_pty() {
    unscanned_history_returns_on_pty("unscanned-pruned", true, "node_modules", false);
}

#[test]
#[cfg(unix)]
fn cleared_query_returns_directory_symlink_history_on_pty() {
    unscanned_history_returns_on_pty("unscanned-symlink", true, "linked-target", true);
}

#[test]
fn enter_through_pty_prints_top_hit_and_exits_zero() {
    let (_dir, fixture, mut session) = spawn_cdh_on_pty("enter-top-hit");
    session.wait_ready();
    session.send(b"\r");
    let status = session.wait_exit();

    assert_eq!(
        status.exit_code(),
        0,
        "回车应成功退出；输出尾部: {}",
        debug_tail(&session.state)
    );
    assert_eq!(session.selection(), fixture.alpha_bytes());
}

#[test]
fn down_arrow_then_enter_through_pty_prints_second_row() {
    let (_dir, fixture, mut session) = spawn_cdh_on_pty("down-second-row");
    session.wait_ready();
    session.send(b"\x1b[B");
    session.send(b"\r");
    let status = session.wait_exit();

    assert_eq!(status.exit_code(), 0);
    assert_eq!(
        session.selection(),
        fixture.beta_bytes(),
        "Down 后回车应选中第二行 beta"
    );
}

#[test]
fn esc_through_pty_exits_without_selection() {
    // Esc 的语义（handle_key_normal）：预览关、查询空时直接退出且无选择，
    // controller 对 Ok(None) 返回退出码 1、stdout 不写任何字节。
    let (_dir, _fixture, mut session) = spawn_cdh_on_pty("esc-no-selection");
    session.wait_ready();
    session.send(b"\x1b");
    let status = session.wait_exit();

    assert_eq!(status.exit_code(), 1);
    assert!(session.selection().is_empty(), "Esc 退出不应输出路径");
}

#[test]
fn query_filter_then_enter_through_pty_prints_match() {
    let (_dir, fixture, mut session) = spawn_cdh_on_pty("query-filter");
    session.wait_ready();
    session.send(b"beta");
    session.send(b"\r");
    let status = session.wait_exit();

    assert_eq!(status.exit_code(), 0);
    assert_eq!(
        session.selection(),
        fixture.beta_bytes(),
        "过滤词 beta 应只剩 beta 一行，回车即选中它"
    );
}

#[test]
fn sgr_mouse_click_then_enter_through_pty_selects_clicked_row() {
    let (_dir, fixture, mut session) = spawn_cdh_on_pty("mouse-click-row");
    session.wait_ready();

    // SGR 鼠标序列是 1 基坐标，crossterm 解析后减 1 变 0 基：点击
    // (CLICK_COLUMN, LIST_FIRST_ROW + CLICK_LIST_INDEX) = 列表第 2 行 beta。
    // 按下 + 释放成对发送，单击只移动选择（双击才直接跳转，不走那条路径）。
    let row = LIST_FIRST_ROW + CLICK_LIST_INDEX + 1;
    let column = CLICK_COLUMN + 1;
    session.send(format!("\x1b[<0;{column};{row}M").as_bytes());
    session.send(format!("\x1b[<0;{column};{row}m").as_bytes());
    session.send(b"\r");
    let status = session.wait_exit();

    assert_eq!(
        status.exit_code(),
        0,
        "点击列表行后回车应成功退出；输出尾部: {}",
        debug_tail(&session.state)
    );
    assert_eq!(
        session.selection(),
        fixture.beta_bytes(),
        "点击第 2 行应把选择移到 beta"
    );

    // 字节流侧：进入时开了鼠标捕获，退出时先关捕获、再离开备用屏幕。
    // （拆卸顺序本身已由 restore_screen 单测钉住，这里断言的是这些
    // 序列真的经 PTY 字节流发出过。）
    session.wait_stream_closed();
    assert!(
        stream_contains(&session.state, ENTER_ALTERNATE_SCREEN),
        "TUI 字节流应包含 EnterAlternateScreen；输出尾部: {}",
        debug_tail(&session.state)
    );
    let mouse_off = stream_position(&session.state, MOUSE_CAPTURE_DISABLE)
        .expect("退出时应发送 DisableMouseCapture");
    let leave_screen = stream_position(&session.state, LEAVE_ALTERNATE_SCREEN)
        .expect("退出时应发送 LeaveAlternateScreen");
    assert!(
        mouse_off < leave_screen,
        "DisableMouseCapture 应先于 LeaveAlternateScreen"
    );
}
