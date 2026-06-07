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

/// “有效配置”——已经合并了默认值和环境变量
#[derive(Debug, Clone)]
pub struct EffectiveConfig {
    /// 推荐列表最大条数（默认 20）
    pub limit: usize,
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
    /// 融合权重：frecency 与 uniq（建议和为 1.0；默认 0.7 / 0.3）
    pub w_frecency: f64,
    pub w_uniq: f64,
}

impl EffectiveConfig {
    /// 从当前进程环境构造配置（默认值 + CDH_* 环境变量）
    pub fn from_env() -> Self {
        // 默认 limit = 20（原行为）
        let limit = std::env::var("CDH_LIMIT")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(20);

        // 原 RecommendOpt::default 中的 half_life 环境逻辑
        let half_life = std::env::var("CDH_HALF_LIFE")
            .ok()
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(7.0 * 24.0 * 3600.0);

        // threshold 以前只有默认 0，这里顺便支持一下 CDH_THRESHOLD（可选）
        let threshold = std::env::var("CDH_THRESHOLD")
            .ok()
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(0.0);

        // 原 RecommendOpt::default 中的 ignore_re
        let ignore_re = std::env::var("CDH_IGNORE_RE")
            .ok()
            .and_then(|re| Regex::new(&re).ok());

        // 是否检查目录存在性（默认 true）
        let check_dir = std::env::var("CDH_CHECK_DIR")
            .ok()
            .and_then(|s| s.parse::<bool>().ok())
            .unwrap_or(true);

        // 原 RecommendOpt::default 中的三个权重相关 env
        let w_frecency = std::env::var("CDH_W_FRECENCY")
            .ok()
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(0.7);

        let w_uniq = std::env::var("CDH_W_UNIQ")
            .ok()
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(0.3);

        let uniq_decay = std::env::var("CDH_UNIQ_DECAY")
            .ok()
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(0.85);

        Self {
            limit,
            half_life,
            threshold,
            ignore_re,
            check_dir,
            uniq_decay,
            w_frecency,
            w_uniq,
        }
    }
}
