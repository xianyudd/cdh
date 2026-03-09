// src/history.rs
//! 历史子系统：统一管理 history_raw（原始日志）和 history_uniq（最近唯一列表）。
//!
//! 约定：
//!   - history_raw: 每行 `<ts_ms>\t<abs_path>`
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
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const HISTORY_LOCK_RETRY_MS: u64 = 50;
const HISTORY_LOCK_RETRIES: usize = 20;
const HISTORY_LOCK_STALE_SECS: u64 = 30;

/// 一条历史记录（来自 history_raw）
#[derive(Debug, Clone)]
pub struct HistoryEntry {
    /// 访问时间戳（毫秒）
    pub ts_ms: i64,
    /// 访问的目录路径
    pub path: PathBuf,
}

/// 统一获取当前时间戳（毫秒）
fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

/// 追加一条记录到 history_raw。
///
/// 说明：
/// - 这只是“写 raw 文件”的最小单位操作。
/// - 不做加锁；外层应通过 `log_visit` 来保证并发安全。
pub fn append_raw(ctx: &AppContext, dir: &str) -> io::Result<()> {
    let ts_ms = now_millis();

    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&ctx.paths.history_raw)?;

    // 格式：<ts_ms>\t<dir>\n
    writeln!(f, "{ts_ms}\t{dir}")?;

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

    with_history_lock(ctx, || {
        // 1) 追加到 raw
        append_raw(ctx, dir)?;
        // 2) 更新 uniq（最近唯一列表）
        update_uniq_after_visit(ctx, dir)?;
        Ok(())
    })
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

/// 读取 history_raw，解析为结构化列表。
///
/// - 如果文件不存在，则返回空列表；
/// - 解析失败的行会被跳过，不会导致整体报错。
pub fn load_raw(ctx: &AppContext) -> io::Result<Vec<HistoryEntry>> {
    parse_history_file(&ctx.paths.history_raw)
}

/// 从指定路径解析历史文件。
/// 文件格式：每行 `<ts_ms>\t<path>`
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

        if let Ok(ts) = ts_str.parse::<i64>() {
            res.push(HistoryEntry {
                ts_ms: ts,
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
            match OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&path)
            {
                Ok(file) => return Ok(FileLock { path, file }),
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
            io::Error::new(
                io::ErrorKind::Other,
                format!("failed to acquire history lock: {}", path.display()),
            )
        }))
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        // 释放锁：删除锁文件，忽略错误
        let _ = fs::remove_file(&self.path);
    }
}

/// 如果锁文件明显过期，尝试清理它。
fn maybe_clear_stale_lock(path: &Path) -> bool {
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

    fn test_config() -> EffectiveConfig {
        EffectiveConfig {
            limit: 20,
            half_life: 7.0 * 24.0 * 3600.0,
            threshold: 0.0,
            ignore_re: None,
            check_dir: true,
            uniq_decay: 0.85,
            w_frecency: 0.7,
            w_uniq: 0.3,
        }
    }

    fn make_test_ctx(name: &str) -> (PathBuf, AppContext) {
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
        };

        fs::create_dir_all(&paths.config_dir).unwrap();
        fs::create_dir_all(&paths.data_dir).unwrap();
        fs::create_dir_all(&paths.state_dir).unwrap();
        fs::create_dir_all(&paths.cache_dir).unwrap();
        if let Some(parent) = paths.history_raw.parent() {
            fs::create_dir_all(parent).unwrap();
        }

        (
            root,
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
    fn log_visit_writes_raw_and_uniq() {
        let (root, ctx) = make_test_ctx("writes_raw_and_uniq");
        let dir = root.join("visited_dir");
        fs::create_dir_all(&dir).unwrap();

        log_visit(&ctx, dir.to_str().unwrap()).unwrap();

        let raw = fs::read_to_string(&ctx.paths.history_raw).unwrap();
        let raw_line = raw.trim();
        let mut parts = raw_line.splitn(2, '\t');
        let ts = parts.next().unwrap();
        let path = parts.next().unwrap();

        assert!(ts.parse::<i64>().is_ok());
        assert_eq!(path, dir.to_string_lossy());
        assert_eq!(
            read_lines(&ctx.paths.history_uniq),
            vec![dir.to_string_lossy().to_string()]
        );

        let _ = fs::remove_dir_all(root);
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

        assert_eq!(
            read_lines(&ctx.paths.history_uniq),
            vec![
                dir_b.to_string_lossy().to_string(),
                dir_a.to_string_lossy().to_string(),
            ]
        );

        let _ = fs::remove_dir_all(root);
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

        let _ = fs::remove_dir_all(root);
    }
}
