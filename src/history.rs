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
//!   - 使用粗粒度文件锁 + 短暂重试/过期锁清理，降低并发写失败概率；
//!   - 使用“临时文件 + rename”保证 history_uniq 的更新尽量原子。

use crate::AppContext;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::path::{Component, Path, PathBuf};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const HISTORY_LOCK_RETRY_MS: u64 = 50;
const HISTORY_LOCK_RETRIES: usize = 20;
const HISTORY_LOCK_STALE_SECS: u64 = 30;
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

/// 简单文件锁：在 state_dir 下创建一个 lock 文件，
/// 同一时刻只有一个进程能持有它。
///
/// 注意：
/// - 这是一个“粗粒度”锁：目前所有历史写操作共用一把锁；
/// - 后续如果需要细分（比如 raw/uniq 分离），可以在这里扩展。
struct FileLock {
    path: PathBuf,
    #[allow(dead_code)]
    file: File,
}

impl FileLock {
    fn acquire(path: PathBuf) -> io::Result<FileLock> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let mut last_err: Option<io::Error> = None;

        for attempt in 0..=HISTORY_LOCK_RETRIES {
            match OpenOptions::new().create_new(true).write(true).open(&path) {
                Ok(mut file) => {
                    // 写入当前进程 PID，供后续 `maybe_clear_stale_lock` 的存活探测使用（H2）。
                    // 写失败不致命：读侧读不到合法 PID 时会退回 mtime 兜底。
                    let _ = write!(file, "{}", std::process::id());
                    let _ = file.flush();
                    return Ok(FileLock { path, file });
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                    last_err = Some(e);

                    if maybe_clear_stale_lock(&path) {
                        continue;
                    }

                    if attempt == HISTORY_LOCK_RETRIES {
                        break;
                    }

                    thread::sleep(Duration::from_millis(HISTORY_LOCK_RETRY_MS));
                }
                Err(e) => return Err(e),
            }
        }

        Err(last_err.unwrap_or_else(|| {
            io::Error::other(format!(
                "failed to acquire history lock: {}",
                path.display()
            ))
        }))
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        // 释放锁：只删「仍然是我们创建的那把」锁文件（H1）。
        //
        // 用创建时保留的句柄 fstat 拿到我们那个 inode（即使 path 已被 unlink/替换，
        // 句柄仍指向原文件），与 path 当前指向的 inode 比对：
        //   - 一致  → 确是我们的锁，删；
        //   - path 不存在 / inode 不同 → 已被别的进程 unlink 并重建，删了就是删别人的锁，
        //     会引发级联失效，故不删。
        // 全程忽略错误，但宁可不删也不误删。
        if lock_file_still_ours(&self.file, &self.path) {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// 判断 `path` 当前指向的锁文件是否就是 `file` 句柄创建的那一个（inode 相同）。
///
/// 用 fstat（句柄）对 stat（路径）比对 inode，避免删掉别的进程重建的同名锁。
#[cfg(unix)]
fn lock_file_still_ours(file: &File, path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;

    let ours = match file.metadata() {
        Ok(meta) => meta.ino(),
        Err(_) => return false,
    };
    let current = match fs::metadata(path) {
        Ok(meta) => meta.ino(),
        Err(_) => return false,
    };
    ours == current
}

/// 非 unix 平台无 inode 语义，退化为「path 仍存在即视为我们的锁」，保持旧行为。
#[cfg(not(unix))]
fn lock_file_still_ours(_file: &File, path: &Path) -> bool {
    path.exists()
}

/// 若能确凿判定持锁进程已死（仅 Linux，读锁文件里的 PID + 探 `/proc/<pid>`），
/// 立即清理以消除 H2 卡顿；任何不确定都返回 false，交给调用方的 mtime 兜底。
///
/// 安全性：本函数只用于「加速」清理，绝不延长持锁——
/// 即便 PID 看似存活（可能是 PID 复用），也返回 false，让 mtime>阈值 的旧逻辑兜底。
#[cfg(target_os = "linux")]
fn lock_owner_is_dead(path: &Path) -> bool {
    // /proc 不存在（异常环境）则无法判定 → 交给 mtime 兜底。
    if !Path::new("/proc").exists() {
        return false;
    }
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(_) => return false,
    };
    // 读不到合法正整数 PID（空文件 / 旧格式 / 损坏）→ 不确定，交给 mtime 兜底。
    let pid = match contents.trim().parse::<u32>() {
        Ok(pid) if pid > 0 => pid,
        _ => return false,
    };
    // /proc/<pid> 不存在 ⇒ 该进程确已退出。
    !Path::new("/proc").join(pid.to_string()).exists()
}

/// 非 Linux（如 macOS CI）没有 `/proc`，无法做存活探测，一律退化到 mtime 判定。
#[cfg(not(target_os = "linux"))]
fn lock_owner_is_dead(_path: &Path) -> bool {
    false
}

/// 如果锁文件明显过期，尝试清理它。
fn maybe_clear_stale_lock(path: &Path) -> bool {
    // 优先：若能确定持锁进程已死，立即清理（H2 加速），无需等满 mtime 阈值。
    if lock_owner_is_dead(path) {
        return match fs::remove_file(path) {
            Ok(()) => true,
            Err(e) if e.kind() == io::ErrorKind::NotFound => true,
            Err(_) => false,
        };
    }

    let meta = match fs::metadata(path) {
        Ok(meta) => meta,
        Err(_) => return false,
    };

    let modified = match meta.modified() {
        Ok(modified) => modified,
        Err(_) => return false,
    };

    let elapsed = match modified.elapsed() {
        Ok(elapsed) => elapsed,
        Err(_) => return false,
    };

    if elapsed < Duration::from_secs(HISTORY_LOCK_STALE_SECS) {
        return false;
    }

    match fs::remove_file(path) {
        Ok(()) => true,
        Err(e) if e.kind() == io::ErrorKind::NotFound => true,
        Err(_) => false,
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

    #[test]
    fn acquire_writes_current_pid_into_lock_file() {
        let (root, _ctx) = make_test_ctx("lock_pid_written");
        let lock_path = root.join("lock");

        let lock = FileLock::acquire(lock_path.clone()).unwrap();
        let contents = fs::read_to_string(&lock_path).unwrap();
        assert_eq!(contents.trim(), process::id().to_string());
        drop(lock);
    }

    #[test]
    fn drop_removes_our_own_lock() {
        let (root, _ctx) = make_test_ctx("lock_drop_own");
        let lock_path = root.join("lock");

        let lock = FileLock::acquire(lock_path.clone()).unwrap();
        assert!(lock_path.exists());
        drop(lock);
        assert!(!lock_path.exists(), "自己的锁在 Drop 时应被清理");
    }

    /// H1 级联回归：锁被别的进程 unlink 后重建成同名不同 inode 的文件，
    /// 我们的 Drop 不得删掉那个替换文件。
    #[test]
    fn drop_does_not_remove_lock_recreated_by_another_process() {
        let (root, _ctx) = make_test_ctx("lock_inode_guard");
        let lock_path = root.join("lock");

        let lock = FileLock::acquire(lock_path.clone()).unwrap();
        // 模拟：我们的锁被 unlink，另一个进程重建了同名锁（新 inode、新内容）。
        fs::remove_file(&lock_path).unwrap();
        fs::write(&lock_path, b"other-process-lock").unwrap();

        drop(lock);

        // 替换文件必须仍在，且内容原封不动（否则就是删了别人的锁 → H1 级联）。
        assert!(lock_path.exists(), "别的进程重建的锁被误删了 (H1 级联)");
        assert_eq!(
            fs::read_to_string(&lock_path).unwrap(),
            "other-process-lock"
        );
    }

    /// H2：锁文件里的 PID 已死时，即便 mtime 很新也应被立即清理。
    #[test]
    fn maybe_clear_stale_lock_removes_dead_pid_immediately() {
        let (root, _ctx) = make_test_ctx("lock_dead_pid");
        let lock_path = root.join("lock");

        // 写一个确定不存在的 PID（超出任何合法 pid），文件 mtime 很新。
        fs::write(&lock_path, format!("{}", u32::MAX)).unwrap();

        let cleared = maybe_clear_stale_lock(&lock_path);

        // Linux：能探测到 PID 已死 → 立即清；非 Linux：退化到 mtime，因很新故不清。
        #[cfg(target_os = "linux")]
        {
            assert!(cleared, "死 PID 的锁应被立即清理，不必等满 30s");
            assert!(!lock_path.exists());
        }
        #[cfg(not(target_os = "linux"))]
        {
            assert!(!cleared, "非 Linux 无 /proc，mtime 很新时不应清理");
            assert!(lock_path.exists());
        }
    }

    /// 安全性：锁文件里的 PID 是活着的进程（当前进程自己）、mtime 很新时，
    /// 绝不能清理，避免误删活锁。两平台都应保持不清。
    #[test]
    fn maybe_clear_stale_lock_keeps_live_pid() {
        let (root, _ctx) = make_test_ctx("lock_live_pid");
        let lock_path = root.join("lock");

        fs::write(&lock_path, format!("{}", process::id())).unwrap();

        let cleared = maybe_clear_stale_lock(&lock_path);
        assert!(!cleared, "活着的持锁进程不应被清 (避免误删活锁)");
        assert!(lock_path.exists());
    }
}
