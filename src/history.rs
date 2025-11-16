// src/history.rs
//! 历史子系统：统一管理 history_raw（追加日志）以及后续的读取/去重等。
//!
//! 目前实现：
//!   - append_raw: 追加一条 <ts_ms>\t<dir> 到 history_raw
//!   - load_raw:   读取 history_raw 为结构化列表（预留给后续推荐使用）

use crate::AppContext;
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// 一条历史记录
#[derive(Debug, Clone)]
pub struct HistoryEntry {
    pub ts_ms: i64,
    pub path: PathBuf,
}

/// 向 history_raw 追加一条记录：当前时间戳 + 目录路径
pub fn append_raw(ctx: &AppContext, dir: &str) -> io::Result<()> {
    // 1. 获取当前时间戳（毫秒）
    let ts_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;

    // 2. 以 append 方式打开 history_raw（不存在就自动创建）
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&ctx.paths.history_raw)?;

    // 3. 写入一行：<ts_ms>\t<dir>\n
    writeln!(f, "{ts_ms}\t{dir}")?;

    Ok(())
}

/// 读取 history_raw，解析为结构化的 HistoryEntry 列表。
/// 目前暂时没有在其他地方使用，但作为公共 API 暴露出来，后续推荐逻辑会用到。
pub fn load_raw(ctx: &AppContext) -> io::Result<Vec<HistoryEntry>> {
    let f = File::open(&ctx.paths.history_raw)?;
    let reader = BufReader::new(f);
    let mut res = Vec::new();

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };
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

