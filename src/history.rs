// src/history.rs
//! 历史子系统：统一管理 history_raw（原始日志）和 history_uniq（最近唯一列表）。
//!
//! 约定：
//!   - history_raw: 每行 `<ts_secs>\t<abs_path>`
//!   - history_uniq: 每行一个 `<abs_path>`，从旧到新，同一路径最多出现一次
//!
//! 对外主要 API：
//!   - log_visit(ctx, dir): 记录一次访问（写 raw + 更新 uniq）
//!   - append_raw(ctx, dir): 仅写 raw（保留给测试/兼容）
//!   - load_raw(ctx): 读 raw 为 HistoryEntry 列表
//!
//! 写入安全：
//!   - 使用粗粒度 flock（OS 劝告锁）串行化并发写，降低并发写失败概率；
//!   - 使用“临时文件 + rename”保证 history_uniq 的更新尽量原子。

use crate::AppContext;
use fs4::FileExt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::path::{Component, Path, PathBuf};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const HISTORY_LOCK_RETRY_MS: u64 = 50;
const HISTORY_LOCK_RETRIES: usize = 20;
const MILLIS_TIMESTAMP_THRESHOLD: i64 = 10_000_000_000;
const MICROS_TIMESTAMP_THRESHOLD: i64 = 10_000_000_000_000;
const NANOS_TIMESTAMP_THRESHOLD: i64 = 10_000_000_000_000_000;

/// 一条历史记录（来自 history_raw）
#[derive(Debug, Clone)]
pub struct HistoryEntry {
    /// 访问时间戳（秒）
    pub ts_secs: i64,
    /// 访问的目录路径
    pub path: PathBuf,
}

/// 统一获取当前时间戳（秒）
fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// 追加一条记录到 history_raw。
///
/// 说明：
/// - 这只是“写 raw 文件”的最小单位操作。
/// - 不做加锁；外层应通过 `log_visit` 来保证并发安全。
pub fn append_raw(ctx: &AppContext, dir: &str) -> io::Result<()> {
    let ts_secs = now_secs();

    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&ctx.paths.history_raw)?;

    // 格式：<ts_secs>\t<dir>\n
    writeln!(f, "{ts_secs}\t{dir}")?;

    Ok(())
}

/// 记录一次目录访问（推荐通过 `cdh log --dir <path>` 调用）。
///
/// - 这是“写历史”的统一高层入口：
///   * 在同一把锁里更新 history_raw + history_uniq；
///   * 以后不管再加什么额外索引/缓存，都可以挂在这里，不改调用方。
pub fn log_visit(ctx: &AppContext, dir: &str) -> io::Result<()> {
    let dir = dir.trim();
    if dir.is_empty() {
        // 空路径直接忽略
        return Ok(());
    }

    let dir = normalize_history_path(dir)?;

    with_history_lock(ctx, || {
        // 1) 追加到 raw
        append_raw(ctx, &dir)?;
        // 2) 更新 uniq（最近唯一列表）
        update_uniq_after_visit(ctx, &dir)?;
        Ok(())
    })
}

/// 从 history_raw 和 history_uniq 中移除指定目录的所有记录。
pub fn remove_path(ctx: &AppContext, dir: &str) -> io::Result<()> {
    let dir = dir.trim();
    if dir.is_empty() {
        return Ok(());
    }

    let dir = normalize_history_path(dir)?;
    with_history_lock(ctx, || {
        rewrite_raw_without_path(ctx, &dir)?;
        rewrite_uniq_without_path(ctx, &dir)?;
        Ok(())
    })
}

/// 规范化用于写入历史文件的路径，保证尽量写入绝对路径。
///
/// 规则：
/// - 相对路径会基于当前工作目录转成绝对路径；
/// - 如果目标存在，优先 canonicalize，去掉 `.` / `..` 并解析软链接；
/// - 如果目标暂时不存在，则退化为词法级规范化，至少保证是绝对路径。
fn normalize_history_path(dir: &str) -> io::Result<String> {
    let path = Path::new(dir);
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };

    let normalized = match fs::canonicalize(&abs) {
        Ok(path) => path,
        Err(_) => normalize_lexically(&abs),
    };

    Ok(normalized.to_string_lossy().into_owned())
}

/// 对路径做不访问文件系统的词法级规范化。
fn normalize_lexically(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    let is_absolute = path.is_absolute();

    for component in path.components() {
        match component {
            Component::Prefix(prefix) => out.push(prefix.as_os_str()),
            Component::RootDir => out.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() && !is_absolute {
                    out.push(component.as_os_str());
                }
            }
            Component::Normal(part) => out.push(part),
        }
    }

    out
}

/// 在一次新的访问之后，按“最近唯一”语义更新 history_uniq。
///
/// 语义：
///   - history_uniq 每行一个绝对路径
///   - 同一个路径最多出现一次
///   - 越靠后的行表示“访问时间越新”
///
/// 实现：
///   - 读出旧 uniq（如果不存在则视为空）
///   - 过滤掉所有等于当前 dir 的行
///   - 在末尾追加当前 dir
///   - 写入临时文件，再原子 rename 覆盖原文件
fn update_uniq_after_visit(ctx: &AppContext, dir: &str) -> io::Result<()> {
    let uniq_path = &ctx.paths.history_uniq;
    let tmp_path = uniq_path.with_extension("tmp");

    // 1) 读旧 uniq
    let mut paths: Vec<String> = Vec::new();
    match File::open(uniq_path) {
        Ok(file) => {
            let reader = BufReader::new(file);
            for line_res in reader.lines() {
                let line = line_res?;
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                if line == dir {
                    // 去掉旧记录
                    continue;
                }
                paths.push(line.to_string());
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            // 不存在视为空 uniq
        }
        Err(e) => return Err(e),
    }

    // 2) 追加当前目录
    paths.push(dir.to_string());

    // 3) 写入临时文件
    if let Some(parent) = uniq_path.parent() {
        fs::create_dir_all(parent)?;
    }
    {
        let file = File::create(&tmp_path)?;
        let mut writer = BufWriter::new(file);
        for p in &paths {
            writeln!(writer, "{p}")?;
        }
        writer.flush()?;
    }

    // 4) 原子替换
    fs::rename(&tmp_path, uniq_path)?;

    Ok(())
}

fn rewrite_raw_without_path(ctx: &AppContext, dir: &str) -> io::Result<()> {
    let raw_path = &ctx.paths.history_raw;
    let tmp_path = raw_path.with_extension("tmp");

    let mut lines: Vec<String> = Vec::new();
    match File::open(raw_path) {
        Ok(file) => {
            let reader = BufReader::new(file);
            for line_res in reader.lines() {
                let line = line_res?;
                let keep = line
                    .split_once('\t')
                    .map(|(_, path)| path.trim() != dir)
                    .unwrap_or(true);
                if keep {
                    lines.push(line);
                }
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    }

    if let Some(parent) = raw_path.parent() {
        fs::create_dir_all(parent)?;
    }
    {
        let file = File::create(&tmp_path)?;
        let mut writer = BufWriter::new(file);
        for line in &lines {
            writeln!(writer, "{line}")?;
        }
        writer.flush()?;
    }
    fs::rename(&tmp_path, raw_path)?;
    Ok(())
}

fn rewrite_uniq_without_path(ctx: &AppContext, dir: &str) -> io::Result<()> {
    let uniq_path = &ctx.paths.history_uniq;
    let tmp_path = uniq_path.with_extension("tmp");

    let mut paths: Vec<String> = Vec::new();
    match File::open(uniq_path) {
        Ok(file) => {
            let reader = BufReader::new(file);
            for line_res in reader.lines() {
                let line = line_res?;
                let line = line.trim();
                if line.is_empty() || line == dir {
                    continue;
                }
                paths.push(line.to_string());
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    }

    if let Some(parent) = uniq_path.parent() {
        fs::create_dir_all(parent)?;
    }
    {
        let file = File::create(&tmp_path)?;
        let mut writer = BufWriter::new(file);
        for p in &paths {
            writeln!(writer, "{p}")?;
        }
        writer.flush()?;
    }
    fs::rename(&tmp_path, uniq_path)?;
    Ok(())
}

/// 读取 history_raw，解析为结构化列表。
///
/// - 如果文件不存在，则返回空列表；
/// - 解析失败的行会被跳过，不会导致整体报错。
pub fn load_raw(ctx: &AppContext) -> io::Result<Vec<HistoryEntry>> {
    parse_history_file(&ctx.paths.history_raw)
}

pub(crate) fn parse_history_ts_secs(ts: &str) -> Option<i64> {
    let ts = ts.parse::<i64>().ok()?;
    if ts >= NANOS_TIMESTAMP_THRESHOLD {
        Some(ts / 1_000_000_000)
    } else if ts >= MICROS_TIMESTAMP_THRESHOLD {
        Some(ts / 1_000_000)
    } else if ts >= MILLIS_TIMESTAMP_THRESHOLD {
        Some(ts / 1000)
    } else {
        Some(ts)
    }
}

/// 从指定路径解析历史文件。
/// 文件格式：每行 `<ts_secs>\t<path>`
fn parse_history_file(path: &Path) -> io::Result<Vec<HistoryEntry>> {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            // 文件不存在视为空历史
            return Ok(Vec::new());
        }
        Err(e) => return Err(e),
    };

    let reader = BufReader::new(file);
    let mut res = Vec::new();

    for line_res in reader.lines() {
        let line = match line_res {
            Ok(s) => s,
            Err(_) => continue,
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let mut parts = line.splitn(2, '\t');
        let ts_str = match parts.next() {
            Some(s) => s,
            None => continue,
        };
        let path_str = match parts.next() {
            Some(s) => s,
            None => continue,
        };

        if let Some(ts) = parse_history_ts_secs(ts_str) {
            res.push(HistoryEntry {
                ts_secs: ts,
                path: PathBuf::from(path_str),
            });
        }
    }

    Ok(res)
}

/// 基于 flock（OS 劝告锁）的粗粒度历史写锁。
///
/// 语义与旧「土锁」的根本差别：
/// - 锁由**内核**持有，进程一旦退出（正常或崩溃）内核**自动释放**——不再有残留
///   锁文件卡住后续写入（消除旧协议的 H2 卡顿）。
/// - 加锁本身是原子的 test-and-set，没有「查存在→判死→删→重建」这套 check-then-act
///   竞态窗口（消除旧协议的 H1 级联误删与 F2/F3 的 TOCTOU 误删活锁）。
/// - 锁文件**常驻**：不再在 Drop 时删除它。flock 锁的是打开的文件描述，文件本身
///   存不存在与锁状态无关，删文件反而会制造新的竞态。
///
/// 取舍：flock 是**劝告锁**，只对同样走 flock 的写者生效；在 NFS / 9p 等网络文件
/// 系统上语义可能弱化（退化为 best-effort，甚至无效）。这是相对土锁可以接受的代价——
/// 本地文件系统上它严格优于原来的竞态协议，而 cdh 的状态目录几乎总在本地盘。
///
/// 注意：
/// - 这是一个“粗粒度”锁：目前所有历史写操作共用一把锁；
/// - 后续如果需要细分（比如 raw/uniq 分离），可以在这里扩展。
struct FileLock {
    file: File,
}

impl FileLock {
    fn acquire(path: PathBuf) -> io::Result<FileLock> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        // 常驻锁文件：存在则用、不存在则建（不用 create_new）。锁的是这个已打开的
        // 文件描述，不是「文件是否存在」，所以并发打开同一路径是正常且安全的。
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)?;

        // 有界重试拿排他锁：`cdh log` 每次 cd 都会调用，绝不能永久阻塞。用
        // try_lock（非阻塞）+ 短睡眠轮询，耗尽重试就返回 Err，让上层静默失败。
        for attempt in 0..=HISTORY_LOCK_RETRIES {
            // 用 UFCS 强制走 fs4 的 trait 方法：std 1.89+ 给 `File` 加了同名的
            // 固有 `try_lock`（返回 `std::fs::TryLockError`），固有方法会优先于
            // trait，`file.try_lock()` 在 >=1.89 的工具链上会解析到 std 而非 fs4，
            // 导致下面按 `fs4::TryLockError` 的匹配编译失败。
            match FileExt::try_lock(&file) {
                Ok(()) => return Ok(FileLock { file }),
                // 锁被别的写者持有：短睡后重试。
                Err(fs4::TryLockError::WouldBlock) => {
                    if attempt == HISTORY_LOCK_RETRIES {
                        break;
                    }
                    thread::sleep(Duration::from_millis(HISTORY_LOCK_RETRY_MS));
                }
                // 真正的 I/O 错误：直接上报，不重试。
                Err(fs4::TryLockError::Error(e)) => return Err(e),
            }
        }

        Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            format!("failed to acquire history lock: {}", path.display()),
        ))
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        // 显式释放 flock。即便这里不显式 unlock，内核也会在 File 关闭（Drop）时
        // 自动释放；显式调用只是让释放时机更清晰。**不删锁文件**——它是常驻的。
        let _ = FileExt::unlock(&self.file);
    }
}

/// 获取历史锁文件路径。
fn history_lock_path(ctx: &AppContext) -> PathBuf {
    // 放在 STATE 目录下，避免污染 DATA/history 目录
    ctx.paths.state_dir.join("lock")
}

/// 在“历史锁”保护下执行闭包，用于所有写历史的高层操作。
fn with_history_lock<F, T>(ctx: &AppContext, f: F) -> io::Result<T>
where
    F: FnOnce() -> io::Result<T>,
{
    let lock_path = history_lock_path(ctx);
    let _lock = FileLock::acquire(lock_path)?;
    f()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AppContext, EffectiveConfig, Paths};
    use std::env;
    use std::process;
    use std::sync::{Arc, Barrier};

    fn test_config() -> EffectiveConfig {
        EffectiveConfig {
            limit: None,
            half_life: 7.0 * 24.0 * 3600.0,
            threshold: 0.0,
            ignore_re: None,
            check_dir: true,
            uniq_decay: 0.85,
            recency_half_life: 24.0 * 3600.0,
            debounce_secs: 600,
            w_frecency: 0.40,
            w_uniq: 0.10,
            w_recency: 0.30,
            w_context: 0.20,
        }
    }

    /// 测试用临时根目录，`Drop` 时整棵删掉。
    ///
    /// 此前每个测试在末尾手写 `remove_dir_all`：断言一失败就跳过清理，
    /// 于是 `/tmp` 里攒下一堆 `cdh_history_test_*`。RAII 让失败路径也能清干净。
    struct TempRoot {
        path: PathBuf,
    }

    impl std::ops::Deref for TempRoot {
        type Target = Path;

        fn deref(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn make_test_ctx(name: &str) -> (TempRoot, AppContext) {
        let uniq = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = env::temp_dir().join(format!(
            "cdh_history_test_{}_{}_{}",
            name,
            process::id(),
            uniq
        ));

        let paths = Paths {
            config_dir: root.join("config"),
            data_dir: root.join("data"),
            state_dir: root.join("state"),
            cache_dir: root.join("cache"),
            history_raw: root.join("data").join("history").join("history_raw"),
            history_uniq: root.join("data").join("history").join("history_uniq"),
            excludes: root.join("data").join("excludes"),
        };

        fs::create_dir_all(&paths.config_dir).unwrap();
        fs::create_dir_all(&paths.data_dir).unwrap();
        fs::create_dir_all(&paths.state_dir).unwrap();
        fs::create_dir_all(&paths.cache_dir).unwrap();
        if let Some(parent) = paths.history_raw.parent() {
            fs::create_dir_all(parent).unwrap();
        }

        (
            TempRoot { path: root },
            AppContext {
                paths,
                config: test_config(),
            },
        )
    }

    fn read_lines(path: &Path) -> Vec<String> {
        fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .map(|line| line.to_string())
            .collect()
    }

    #[test]
    fn parse_history_ts_secs_normalizes_subsecond_timestamps() {
        assert_eq!(
            parse_history_ts_secs("1763331934955000000"),
            Some(1_763_331_934)
        );
        assert_eq!(
            parse_history_ts_secs("1763331934955000"),
            Some(1_763_331_934)
        );
        assert_eq!(parse_history_ts_secs("1763331934955"), Some(1_763_331_934));
        assert_eq!(parse_history_ts_secs("1763331934"), Some(1_763_331_934));
        assert_eq!(parse_history_ts_secs("not-a-timestamp"), None);
    }

    #[test]
    fn load_raw_normalizes_millisecond_timestamps() {
        let (root, ctx) = make_test_ctx("load_raw_millis");
        fs::write(
            &ctx.paths.history_raw,
            format!("1763331934955\t{}\n", root.display()),
        )
        .unwrap();

        let entries = load_raw(&ctx).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].ts_secs, 1_763_331_934);
        assert_eq!(entries[0].path, *root);
    }

    #[test]
    fn log_visit_writes_raw_and_uniq() {
        let (root, ctx) = make_test_ctx("writes_raw_and_uniq");
        let dir = root.join("visited_dir");
        fs::create_dir_all(&dir).unwrap();

        log_visit(&ctx, dir.to_str().unwrap()).unwrap();

        let raw = fs::read_to_string(&ctx.paths.history_raw).unwrap();
        let raw_line = raw.trim();
        let (ts, path) = raw_line.split_once('\t').unwrap();

        // 期望值必须过 canonicalize：log_visit 会解析软链接，而 macOS 的临时目录
        // 在 /var/folders/... 下，/var 本身是 /private/var 的软链接。
        let expected = fs::canonicalize(&dir)
            .unwrap()
            .to_string_lossy()
            .to_string();

        assert!(ts.parse::<i64>().is_ok());
        assert_eq!(path, expected);
        assert_eq!(read_lines(&ctx.paths.history_uniq), vec![expected]);
    }

    #[test]
    fn log_visit_moves_existing_path_to_end_of_uniq() {
        let (root, ctx) = make_test_ctx("moves_existing_path");
        let dir_a = root.join("a");
        let dir_b = root.join("b");
        fs::create_dir_all(&dir_a).unwrap();
        fs::create_dir_all(&dir_b).unwrap();

        log_visit(&ctx, dir_a.to_str().unwrap()).unwrap();
        log_visit(&ctx, dir_b.to_str().unwrap()).unwrap();
        log_visit(&ctx, dir_a.to_str().unwrap()).unwrap();

        // 同上，比对的是解析过软链接的路径
        let expected_a = fs::canonicalize(&dir_a)
            .unwrap()
            .to_string_lossy()
            .to_string();
        let expected_b = fs::canonicalize(&dir_b)
            .unwrap()
            .to_string_lossy()
            .to_string();

        assert_eq!(
            read_lines(&ctx.paths.history_uniq),
            vec![expected_b, expected_a]
        );
    }

    #[test]
    fn remove_path_deletes_from_raw_and_uniq() {
        let (root, ctx) = make_test_ctx("remove_path");
        let stale = root.join("stale");
        let keep = root.join("keep");
        fs::create_dir_all(&keep).unwrap();

        fs::write(
            &ctx.paths.history_raw,
            format!(
                "100\t{}\n101\t{}\n102\t{}\nmalformed\n",
                stale.display(),
                keep.display(),
                stale.display()
            ),
        )
        .unwrap();
        fs::write(
            &ctx.paths.history_uniq,
            format!("{}\n{}\n", stale.display(), keep.display()),
        )
        .unwrap();

        remove_path(&ctx, stale.to_str().unwrap()).unwrap();

        let raw = fs::read_to_string(&ctx.paths.history_raw).unwrap();
        assert!(!raw.contains(stale.to_str().unwrap()));
        assert!(raw.contains(keep.to_str().unwrap()));
        assert!(raw.contains("malformed"));
        assert_eq!(
            read_lines(&ctx.paths.history_uniq),
            vec![keep.to_string_lossy().to_string()]
        );
    }

    #[test]
    fn remove_path_and_log_visit_do_not_overwrite_each_other() {
        let (root, ctx) = make_test_ctx("remove_log_race");
        let stale = root.join("stale");
        let keep = root.join("keep");
        fs::create_dir_all(&keep).unwrap();

        fs::write(
            &ctx.paths.history_raw,
            format!("100\t{}\n", stale.display()),
        )
        .unwrap();
        fs::write(&ctx.paths.history_uniq, format!("{}\n", stale.display())).unwrap();

        let barrier = Arc::new(Barrier::new(2));
        let remove_ctx = ctx.clone();
        let remove_barrier = Arc::clone(&barrier);
        let stale_for_remove = stale.clone();
        let remove_thread = thread::spawn(move || {
            remove_barrier.wait();
            remove_path(&remove_ctx, stale_for_remove.to_str().unwrap())
        });

        let log_ctx = ctx.clone();
        let log_barrier = Arc::clone(&barrier);
        let keep_for_log = keep.clone();
        let log_thread = thread::spawn(move || {
            log_barrier.wait();
            log_visit(&log_ctx, keep_for_log.to_str().unwrap())
        });

        remove_thread.join().unwrap().unwrap();
        log_thread.join().unwrap().unwrap();

        let keep_canon = fs::canonicalize(&keep)
            .unwrap()
            .to_string_lossy()
            .to_string();
        let raw = fs::read_to_string(&ctx.paths.history_raw).unwrap();
        assert!(!raw.contains(stale.to_str().unwrap()));
        assert!(raw.contains(&keep_canon));
        assert_eq!(read_lines(&ctx.paths.history_uniq), vec![keep_canon]);
    }

    #[test]
    fn log_visit_returns_error_when_uniq_path_is_invalid() {
        let (root, mut ctx) = make_test_ctx("uniq_open_error");
        let bad_parent = root.join("not_a_dir");
        let dir = root.join("visited_dir");

        fs::write(&bad_parent, b"not a directory").unwrap();
        fs::create_dir_all(&dir).unwrap();

        ctx.paths.history_uniq = bad_parent.join("history_uniq");

        let result = log_visit(&ctx, dir.to_str().unwrap());
        assert!(result.is_err());
    }

    #[test]
    fn log_visit_normalizes_relative_path_to_absolute() {
        let (root, ctx) = make_test_ctx("normalize_relative_path");
        let workspace = root.join("workspace");
        let nested = workspace.join("nested");
        let target = workspace.join("target_dir");
        fs::create_dir_all(&nested).unwrap();
        fs::create_dir_all(&target).unwrap();

        let old_cwd = env::current_dir().unwrap();
        env::set_current_dir(&nested).unwrap();

        let result = log_visit(&ctx, "../target_dir");

        env::set_current_dir(old_cwd).unwrap();
        result.unwrap();

        let expected = fs::canonicalize(&target)
            .unwrap()
            .to_string_lossy()
            .to_string();

        let raw = fs::read_to_string(&ctx.paths.history_raw).unwrap();
        let raw_path = raw.trim().split_once('\t').unwrap().1;

        assert_eq!(raw_path, expected);
        assert_eq!(read_lines(&ctx.paths.history_uniq), vec![expected]);
    }

    /// 持锁期间，另一个独立 handle 对同一锁文件 `try_lock` 必须失败
    /// （WouldBlock）——这正是 flock 提供的互斥语义，也是串行化并发写的根基。
    #[test]
    fn flock_blocks_second_try_lock_while_held() {
        let (root, _ctx) = make_test_ctx("flock_contended");
        let lock_path = root.join("lock");

        let held = FileLock::acquire(lock_path.clone()).unwrap();

        // 第二个独立打开的 handle 指向同一文件，try_lock 应因锁被占而 WouldBlock。
        let other = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .unwrap();
        match FileExt::try_lock(&other) {
            Err(fs4::TryLockError::WouldBlock) => {}
            Ok(()) => panic!("持锁期间第二个 try_lock 不应成功（flock 互斥失效）"),
            Err(fs4::TryLockError::Error(e)) => panic!("try_lock 意外 I/O 错误: {e}"),
        }

        drop(held);
    }

    /// 持锁者 Drop（→ unlock + 关文件）后，锁必须可被再次获取。
    /// 这条守的是「释放确实生效」——若 Drop 没释放，acquire 会耗尽重试而 Err。
    #[test]
    fn flock_released_after_drop_allows_reacquire() {
        let (root, _ctx) = make_test_ctx("flock_reacquire");
        let lock_path = root.join("lock");

        let held = FileLock::acquire(lock_path.clone()).unwrap();
        // Drop 里显式 `FileExt::unlock` 是释放的兜底；即便省略它，`File` close
        // 也会由内核释放 flock——两条路径构成双保险，这里验的是释放确实生效。
        drop(held);

        // 释放后应能立刻拿到；acquire 内部有重试，但正常释放后首次 try 就该成功。
        let again = FileLock::acquire(lock_path.clone());
        assert!(again.is_ok(), "持锁者 Drop 后锁应可被重新获取");

        // 锁文件是常驻的：Drop 不删它。
        assert!(
            lock_path.exists(),
            "flock 锁文件应常驻，不该在 Drop 时被删除"
        );
    }

    /// `with_history_lock` 串行执行闭包、透传返回值，且闭包内的历史写入落盘。
    #[test]
    fn with_history_lock_runs_closure_and_returns_value() {
        let (root, ctx) = make_test_ctx("with_lock_closure");
        let dir = root.join("visited_dir");
        fs::create_dir_all(&dir).unwrap();
        let dir_str = fs::canonicalize(&dir)
            .unwrap()
            .to_string_lossy()
            .to_string();

        let ret = with_history_lock(&ctx, || {
            append_raw(&ctx, &dir_str)?;
            Ok(42_u32)
        })
        .unwrap();

        assert_eq!(ret, 42, "返回值必须透传");

        let raw = fs::read_to_string(&ctx.paths.history_raw).unwrap();
        assert!(raw.contains(&dir_str), "闭包内 append_raw 的写入必须落盘");
    }

    /// 两个线程各自 `with_history_lock` 递增共享计数，锁必须串行化临界区，
    /// 结果无交错（最终恰好为迭代总数）。用「持锁期间对方无法进入」来承重。
    #[test]
    fn with_history_lock_serializes_concurrent_writers() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        // 保留 _root 绑定：其 Drop 负责清理临时目录，须活到测试结束。
        let (_root, ctx) = make_test_ctx("with_lock_serialize");

        let counter = Arc::new(AtomicUsize::new(0));
        // 临界区内的「占用」标志：若两个闭包同时进入，in_critical 会 >1。
        let in_critical = Arc::new(AtomicUsize::new(0));
        let overlaps = Arc::new(AtomicUsize::new(0));
        let barrier = Arc::new(Barrier::new(2));

        let iters = 50;
        let mut handles = Vec::new();
        for _ in 0..2 {
            let ctx = ctx.clone();
            let counter = Arc::clone(&counter);
            let in_critical = Arc::clone(&in_critical);
            let overlaps = Arc::clone(&overlaps);
            let barrier = Arc::clone(&barrier);
            handles.push(thread::spawn(move || {
                barrier.wait();
                for _ in 0..iters {
                    with_history_lock(&ctx, || {
                        // 进入临界区：若此刻已有别人在内，记一次重叠。
                        if in_critical.fetch_add(1, Ordering::SeqCst) != 0 {
                            overlaps.fetch_add(1, Ordering::SeqCst);
                        }
                        counter.fetch_add(1, Ordering::SeqCst);
                        // 撑开一段可观测的耗时窗口：若锁失效、两个闭包能同时进入，
                        // 对方必然落在这段 sleep 里被上面的 fetch_add 观测到（overlaps>0）。
                        // 只靠三条原子操作窗口太窄，几乎不重叠——那样测试就不承重了。
                        thread::sleep(Duration::from_micros(200));
                        in_critical.fetch_sub(1, Ordering::SeqCst);
                        Ok(())
                    })
                    .unwrap();
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }

        assert_eq!(
            counter.load(Ordering::SeqCst),
            2 * iters,
            "所有临界区都应执行且不丢失"
        );
        assert_eq!(
            overlaps.load(Ordering::SeqCst),
            0,
            "临界区不得交错：flock 未能串行化并发写者"
        );
    }
}
