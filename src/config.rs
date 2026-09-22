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
        // 允许负/零（负 threshold 语义上等于关闭），但拒绝 nan/inf——
        // recommend 里 `threshold <= 0 || score >= threshold` 遇到 nan 时两边
        // 都为 false，会把所有历史候选静默清空。
        let threshold = parse_finite_f64_env("CDH_THRESHOLD", 0.0)?;

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
        // 权重要求有限且 >= 0：nan 会让排序失稳，inf/负值会把融合分推出 [0,1]。
        let w_frecency = parse_weight_env("CDH_W_FRECENCY", 0.40)?;

        let w_uniq = parse_weight_env("CDH_W_UNIQ", 0.10)?;

        let w_recency = parse_weight_env("CDH_W_RECENCY", 0.30)?;

        let w_context = parse_weight_env("CDH_W_CONTEXT", 0.20)?;

        // uniq_decay 是几何衰减系数，只有落在 [0,1] 才有意义：
        // nan/负/>1 会让 `decay.powi(k)` 产生 NaN 或越界值。
        let uniq_decay = parse_unit_f64_env("CDH_UNIQ_DECAY", 0.85)?;

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

/// 解析要求“有限”的 f64 环境变量（允许负/零，供 threshold 用——
/// 负 threshold 语义上等于关闭过滤）。缺失回退默认，nan/inf 报错。
fn parse_finite_f64_env(name: &str, default: f64) -> Result<f64, String> {
    let value = match env::var(name) {
        Ok(value) => value,
        Err(env::VarError::NotPresent) => return Ok(default),
        Err(env::VarError::NotUnicode(_)) => {
            return Err(format!("{name} 必须是有限数字"));
        }
    };

    match value.parse::<f64>() {
        Ok(n) if n.is_finite() => Ok(n),
        _ => Err(format!("{name} 必须是有限数字")),
    }
}

/// 解析融合权重环境变量：要求有限且 >= 0。缺失回退默认，nan/inf/负值报错。
fn parse_weight_env(name: &str, default: f64) -> Result<f64, String> {
    let value = match env::var(name) {
        Ok(value) => value,
        Err(env::VarError::NotPresent) => return Ok(default),
        Err(env::VarError::NotUnicode(_)) => {
            return Err(format!("{name} 必须是 >= 0 的有限数字"));
        }
    };

    match value.parse::<f64>() {
        Ok(n) if n.is_finite() && n >= 0.0 => Ok(n),
        _ => Err(format!("{name} 必须是 >= 0 的有限数字")),
    }
}

/// 解析落在 [0, 1] 的 f64 环境变量（供 uniq_decay 用）。缺失回退默认，
/// nan/inf/越界报错。
fn parse_unit_f64_env(name: &str, default: f64) -> Result<f64, String> {
    let value = match env::var(name) {
        Ok(value) => value,
        Err(env::VarError::NotPresent) => return Ok(default),
        Err(env::VarError::NotUnicode(_)) => {
            return Err(format!("{name} 必须是 0 到 1 之间的有限数字"));
        }
    };

    match value.parse::<f64>() {
        Ok(n) if n.is_finite() && (0.0..=1.0).contains(&n) => Ok(n),
        _ => Err(format!("{name} 必须是 0 到 1 之间的有限数字")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// `from_env` 读进程级的 CDH_* 变量，测试并发跑会互相污染。这里串行化，
    /// 并在每个用例结束时清掉自己设过的变量，避免泄漏到别的用例。
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// 在锁保护下清空本簇校验涉及的所有 CDH_* 变量，跑一次 `from_env`。
    /// `set` 里给出的键会先设值，函数返回前统一清理，无论断言是否失败。
    fn from_env_with(set: &[(&str, &str)]) -> Result<EffectiveConfig, String> {
        const KEYS: &[&str] = &[
            "CDH_THRESHOLD",
            "CDH_W_FRECENCY",
            "CDH_W_UNIQ",
            "CDH_W_RECENCY",
            "CDH_W_CONTEXT",
            "CDH_UNIQ_DECAY",
        ];
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        for key in KEYS {
            env::remove_var(key);
        }
        for (key, value) in set {
            env::set_var(key, value);
        }
        let result = EffectiveConfig::from_env();
        for key in KEYS {
            env::remove_var(key);
        }
        result
    }

    #[test]
    fn threshold_nan_is_rejected() {
        assert!(from_env_with(&[("CDH_THRESHOLD", "nan")]).is_err());
    }

    #[test]
    fn threshold_negative_is_accepted() {
        let cfg = from_env_with(&[("CDH_THRESHOLD", "-1")]).expect("负 threshold 应视为关闭过滤");
        assert_eq!(cfg.threshold, -1.0);
    }

    #[test]
    fn weight_inf_is_rejected() {
        assert!(from_env_with(&[("CDH_W_FRECENCY", "inf")]).is_err());
    }

    #[test]
    fn weight_non_numeric_is_rejected() {
        assert!(from_env_with(&[("CDH_W_UNIQ", "abc")]).is_err());
    }

    #[test]
    fn weight_negative_is_rejected() {
        assert!(from_env_with(&[("CDH_W_RECENCY", "-0.1")]).is_err());
    }

    #[test]
    fn uniq_decay_above_one_is_rejected() {
        assert!(from_env_with(&[("CDH_UNIQ_DECAY", "2.0")]).is_err());
    }

    #[test]
    fn uniq_decay_negative_is_rejected() {
        assert!(from_env_with(&[("CDH_UNIQ_DECAY", "-0.5")]).is_err());
    }

    #[test]
    fn uniq_decay_in_unit_range_is_accepted() {
        let cfg = from_env_with(&[("CDH_UNIQ_DECAY", "0.5")]).expect("0.5 落在 [0,1] 内");
        assert_eq!(cfg.uniq_decay, 0.5);
    }
}
