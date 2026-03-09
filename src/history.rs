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
//!   - 使用粗粒度文件锁 + “临时文件 + rename” 来保证 history_uniq 的更新在进程间是原子的。

use crate::AppContext;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

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
    if let Ok(file) = File::open(uniq_path) {
        let reader = BufReader::new(file);
        for line in reader.lines() {
            let line = match line {
                Ok(l) => l,
                Err(_) => continue,
            };
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

    // 2) 追加当前目录
    paths.push(dir.to_string());

    // 3) 写入临时文件
    if let Some(parent) = uniq_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    {
        let file = File::create(&tmp_path)?;
        let mut writer = BufWriter::new(file);
        for p in &paths {
            writeln!(writer, "{p}")?;
        }
        // BufWriter drop 时会 flush
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

        // 使用 create_new 保证“如果文件已存在就失败”，实现互斥
        let file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)?;

        Ok(FileLock { path, file })
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        // 释放锁：删除锁文件，忽略错误
        let _ = fs::remove_file(&self.path);
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

