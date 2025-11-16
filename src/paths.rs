// src/paths.rs
//! 统一管理 cdh 使用的所有路径（XDG 风格）。
//!
//! 约定：
//!   CONFIG = ${XDG_CONFIG_HOME:-$HOME/.config}/cdh
//!   DATA   = ${XDG_DATA_HOME:-$HOME/.local/share}/cdh
//!   STATE  = ${XDG_STATE_HOME:-$HOME/.local/state}/cdh
//!   CACHE  = ${XDG_CACHE_HOME:-$HOME/.cache}/cdh
//!
//! 历史文件：
//!   DATA/history/history_raw
//!   DATA/history/history_uniq

use std::env;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Paths {
    /// 配置目录：$XDG_CONFIG_HOME/cdh 或 ~/.config/cdh
    pub config_dir: PathBuf,
    /// 数据目录：$XDG_DATA_HOME/cdh 或 ~/.local/share/cdh
    pub data_dir: PathBuf,
    /// 状态目录：$XDG_STATE_HOME/cdh 或 ~/.local/state/cdh
    pub state_dir: PathBuf,
    /// 缓存目录：$XDG_CACHE_HOME/cdh 或 ~/.cache/cdh
    pub cache_dir: PathBuf,

    /// 原始历史日志：DATA/history/history_raw
    pub history_raw: PathBuf,
    /// 最近唯一历史：DATA/history/history_uniq
    pub history_uniq: PathBuf,
}

impl Paths {
    /// 从环境变量推导所有目录 & 文件路径。
    /// 这里只做“字符串拼接”，不做实际 IO。
    pub fn from_env() -> Self {
        // 基础 HOME 目录（尽量简单，不额外引第三方 crate）
        let home = env::var("HOME").unwrap_or_else(|_| ".".into());
        let home = PathBuf::from(home);

        // XDG 基路径（不存在就 fallback 到 HOME 下默认路径）
        let config_base = env::var("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| home.join(".config"));

        let data_base = env::var("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| home.join(".local").join("share"));

        let state_base = env::var("XDG_STATE_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| home.join(".local").join("state"));

        let cache_base = env::var("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| home.join(".cache"));

        // 本项目自己的子目录
        let config_dir = config_base.join("cdh");
        let data_dir   = data_base.join("cdh");
        let state_dir  = state_base.join("cdh");
        let cache_dir  = cache_base.join("cdh");

        // 历史文件放在 DATA/cdh/history/ 下面
        let history_dir  = data_dir.join("history");
        let history_raw  = history_dir.join("history_raw");
        let history_uniq = history_dir.join("history_uniq");

        Self {
            config_dir,
            data_dir,
            state_dir,
            cache_dir,
            history_raw,
            history_uniq,
        }
    }
}

