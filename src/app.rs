// src/app.rs
//! 全局运行时上下文：汇总 Paths + Config 等信息。

use crate::paths::Paths;
use crate::config::EffectiveConfig;
use std::fs;
use std::path::Path;

/// 程序运行时的全局上下文。
/// - paths: 所有用到的路径（历史文件 / XDG 目录等）
/// - config: 合并后的配置（默认值 + 环境变量，后续可加文件）
#[derive(Debug, Clone)]
pub struct AppContext {
    pub paths: Paths,
    pub config: EffectiveConfig,
}

impl AppContext {
    /// 从当前进程环境构建上下文，并确保必要的目录/文件已经存在。
    pub fn init_from_process() -> Self {
        let paths = Paths::from_env();

        // 确保 XDG 目录和历史文件存在（失败时只打印 warning，不直接 panic）
        ensure_dirs_and_files(&paths);

        let config = EffectiveConfig::from_env();

        AppContext { paths, config }
    }
}

/// 创建必要的目录，并在历史文件不存在时创建空文件。
fn ensure_dirs_and_files(paths: &Paths) {
    // 1. 创建四个基础目录（config/data/state/cache）
    for dir in [
        &paths.config_dir,
        &paths.data_dir,
        &paths.state_dir,
        &paths.cache_dir,
    ] {
        if let Err(e) = fs::create_dir_all(dir) {
            eprintln!("[cdh] warn: failed to create dir {:?}: {e}", dir);
        }
    }

    // 2. 创建 history 目录（以 history_raw 的 parent 为准）
    if let Some(history_dir) = paths.history_raw.parent() {
        if let Err(e) = fs::create_dir_all(history_dir) {
            eprintln!("[cdh] warn: failed to create history dir {:?}: {e}", history_dir);
        }
    }

    // 3. touch 两个历史文件：不存在就创建空文件
    touch_file(&paths.history_raw);
    touch_file(&paths.history_uniq);
}

/// 简单的 touch 实现：
/// - 若文件存在：什么都不做
/// - 若不存在：确保父目录存在，然后创建一个空文件
fn touch_file(path: &Path) {
    if path.exists() {
        return;
    }

    if let Some(parent) = path.parent() {
        if let Err(e) = fs::create_dir_all(parent) {
            eprintln!("[cdh] warn: failed to create parent dir {:?}: {e}", parent);
            return;
        }
    }

    if let Err(e) = fs::File::create(path) {
        eprintln!("[cdh] warn: failed to create file {:?}: {e}", path);
    }
}

