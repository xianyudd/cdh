// src/config.rs
//! 运行时配置：默认值 + 环境变量（后续可以再加 config.toml）
//!
//! 优先级设计（当前版本）：
//!   1. 内置默认值
//!   2. 环境变量 CDH_* 覆盖
//!   3. 最后由 CLI 参数覆盖（在 controller.rs 里做）
//
// 未来如果要支持 config.toml，可以在这里再加 from_file / from_env_and_file 等方法。

use regex::Regex;
use std::env;

/// “有效配置”——已经合并了默认值和环境变量
#[derive(Debug, Clone)]
pub struct EffectiveConfig {
    /// 推荐列表最大条数（默认不截断）
    pub limit: Option<usize>,
    /// Frecency 半衰期（秒），默认 7 天
    pub half_life: f64,
    /// 最终融合分阈值（< threshold 的条目被丢弃；0 表示不启用）
    pub threshold: f64,
    /// 忽略路径的正则（默认读取 `CDH_IGNORE_RE`，解析失败则忽略）
    pub ignore_re: Option<Regex>,
    /// 是否检查目录存在性（默认 true；可用 CDH_CHECK_DIR=false 关闭）
    pub check_dir: bool,
    /// uniq 的几何衰减系数（最新=1.0，次新=decay，…；默认 0.85）
    pub uniq_decay: f64,
    /// 最近性半衰期（秒），默认 24 小时（CDH_RECENCY_HALF_LIFE）
    pub recency_half_life: f64,
    /// 频次防抖窗口（秒），默认 600（CDH_DEBOUNCE_SECS）
    pub debounce_secs: i64,
    /// 融合权重（默认 0.40/0.10/0.30/0.20，可用 CDH_W_* 覆盖）
    pub w_frecency: f64,
    pub w_uniq: f64,
    pub w_recency: f64,
    pub w_context: f64,
}

impl EffectiveConfig {
    /// 从当前进程环境构造配置（默认值 + CDH_* 环境变量）
    pub fn from_env() -> Result<Self, String> {
        // 默认不截断候选；CDH_LIMIT 可显式限制数量
        let limit = parse_optional_positive_usize_env("CDH_LIMIT")?;

        let half_life = parse_positive_f64_env("CDH_HALF_LIFE", 7.0 * 24.0 * 3600.0)?;

        // threshold 以前只有默认 0，这里顺便支持一下 CDH_THRESHOLD（可选）
        let threshold = std::env::var("CDH_THRESHOLD")
            .ok()
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(0.0);

        // 原 RecommendOpt::default 中的 ignore_re
        let ignore_re = env::var("CDH_IGNORE_RE")
            .ok()
            .and_then(|re| Regex::new(&re).ok());

        // 是否检查目录存在性（默认 true）
        let check_dir = env::var("CDH_CHECK_DIR")
            .ok()
            .and_then(|s| s.parse::<bool>().ok())
            .unwrap_or(true);

        // 原 RecommendOpt::default 中的三个权重相关 env
        let w_frecency = env::var("CDH_W_FRECENCY")
            .ok()
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(0.40);

        let w_uniq = env::var("CDH_W_UNIQ")
            .ok()
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(0.10);

        let w_recency = env::var("CDH_W_RECENCY")
            .ok()
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(0.30);

        let w_context = env::var("CDH_W_CONTEXT")
            .ok()
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(0.20);

        let uniq_decay = env::var("CDH_UNIQ_DECAY")
            .ok()
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(0.85);

        let recency_half_life = parse_positive_f64_env("CDH_RECENCY_HALF_LIFE", 24.0 * 3600.0)?;

        // 防抖窗口允许 0（关闭防抖），负数视为无效回退默认
        let debounce_secs = env::var("CDH_DEBOUNCE_SECS")
            .ok()
            .and_then(|s| s.parse::<i64>().ok())
            .filter(|&v| v >= 0)
            .unwrap_or(600);

        Ok(Self {
            limit,
            half_life,
            threshold,
            ignore_re,
            check_dir,
            uniq_decay,
            recency_half_life,
            debounce_secs,
            w_frecency,
            w_uniq,
            w_recency,
            w_context,
        })
    }
}

fn parse_optional_positive_usize_env(name: &str) -> Result<Option<usize>, String> {
    let value = match env::var(name) {
        Ok(value) => value,
        Err(env::VarError::NotPresent) => return Ok(None),
        Err(env::VarError::NotUnicode(_)) => {
            return Err(format!("{name} 必须是大于 0 的整数"));
        }
    };

    match value.parse::<usize>() {
        Ok(n) if n > 0 => Ok(Some(n)),
        _ => Err(format!("{name} 必须是大于 0 的整数")),
    }
}

fn parse_positive_f64_env(name: &str, default: f64) -> Result<f64, String> {
    let value = match env::var(name) {
        Ok(value) => value,
        Err(env::VarError::NotPresent) => return Ok(default),
        Err(env::VarError::NotUnicode(_)) => {
            return Err(format!("{name} 必须是大于 0 的有限数字"));
        }
    };

    match value.parse::<f64>() {
        Ok(n) if n.is_finite() && n > 0.0 => Ok(n),
        _ => Err(format!("{name} 必须是大于 0 的有限数字")),
    }
}
