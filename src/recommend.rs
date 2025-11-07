// src/recommend.rs
//! 智能目录推荐（融合 `~/.cd_history_raw` + `~/.cd_history` 的最优实现）
//!
//! 设计要点：
//! - 以 raw 的 Frecency 分数为主、uniq 的“最近唯一”几何衰减分为辅，线性融合（可调权重）。
//! - 流式读取 raw，低内存；一次性 lower tokens；可选校验目录存在性（WSL/网络盘可关）。
//! - 归一化到 [0,1] 再融合；支持阈值、关键词/正则过滤；对连续相同 (ts,path) 去重防抖。
//!
//! 对外接口：
//! - `RecommendOpt`：融合推荐所有配置
//! - `Recommendation{ path, score }`：推荐结果
//! - `recommend(&RecommendOpt) -> Vec<Recommendation>`：路径+融合分
//! - `recommend_paths(&RecommendOpt) -> Vec<String>`：仅路径
//! - `recommend_with_now(&RecommendOpt, now_secs)`：可注入“当前时间”的变体（便于测试）
//!
//! 依赖：本 crate 需已提供 `Frecency` / `FrecencyIndex`（见 src/frecency.rs）。
use crate::frecency::{Frecency, FrecencyIndex};
use regex::Regex;
use std::{
    collections::{HashMap, HashSet},
    fs::File,
    io::{BufRead, BufReader},
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

#[inline]
fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

/// 推荐结果
#[derive(Debug, Clone)]
pub struct Recommendation {
    pub path: String,
    pub score: f64, // 融合后的最终分（0~1）
}

/// 融合推荐的配置
#[derive(Debug, Clone)]
pub struct RecommendOpt {
    /// 原始频次日志：`ts<TAB>path`（默认 `$HOME/.cd_history_raw`）
    pub raw: String,
    /// 最近唯一列表：一行一个 path（默认 `$HOME/.cd_history`）
    pub uniq: String,
    /// 返回最大条数（默认 20）
    pub limit: usize,
    /// Frecency 半衰期（秒），默认 7 天
    pub half_life: f64,
    /// 最终融合分阈值（< threshold 的条目会被丢弃；0 表示不启用）
    pub threshold: f64,
    /// 忽略路径的正则（默认读取 `CDH_IGNORE_RE`）
    pub ignore_re: Option<Regex>,
    /// 关键词过滤（OR 语义，大小写不敏感；为空则不过滤）
    pub tokens: Vec<String>,
    /// 是否校验目录存在性（WSL/远程盘建议置 false 提速；默认 true）
    pub check_dir: bool,
    /// uniq 的几何衰减系数（最新=1.0，次新=decay，…；默认 0.85）
    pub uniq_decay: f64,
    /// 融合权重：frecency 与 uniq（建议和为 1.0；默认 0.7 / 0.3）
    pub w_frecency: f64,
    pub w_uniq: f64,
}

impl Default for RecommendOpt {
    fn default() -> Self {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        let raw = format!("{home}/.cd_history_raw");
        let uniq = format!("{home}/.cd_history");
        let half_life = std::env::var("CDH_HALF_LIFE")
            .ok()
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(7.0 * 24.0 * 3600.0);
        let ignore_re = std::env::var("CDH_IGNORE_RE")
            .ok()
            .and_then(|re| Regex::new(&re).ok());
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
            raw,
            uniq,
            limit: 20,
            half_life,
            threshold: 0.0,
            ignore_re,
            tokens: vec![],
            check_dir: true,
            uniq_decay,
            w_frecency,
            w_uniq,
        }
    }
}

/// 外部主接口：融合 RAW+UNIQ，返回路径+分数（按最终分降序）
pub fn recommend(opt: &RecommendOpt) -> Vec<Recommendation> {
    recommend_with_now(opt, now_secs())
}

/// 变体：可注入“当前时间”，便于测试
pub fn recommend_with_now(opt: &RecommendOpt, now: i64) -> Vec<Recommendation> {
    // 预处理 tokens（一次性 lower）
    let tokens_lc: Vec<String> = opt.tokens.iter().map(|t| t.to_lowercase()).collect();

    // 1) uniq -> 生成 “最近唯一”几何衰减分
    let uniq_scores = load_uniq_scores(
        &opt.uniq,
        &opt.ignore_re,
        &tokens_lc,
        opt.check_dir,
        opt.uniq_decay,
    );

    // 2) raw -> 建 Frecency 索引（流式），并记录出现过的路径
    let (idx, seen_raw) = build_frecency_from_raw(
        &opt.raw,
        &opt.ignore_re,
        &tokens_lc,
        opt.check_dir,
        opt.half_life,
    );

    // 3) 候选集 = raw ∪ uniq
    let mut candidates: HashSet<String> = seen_raw;
    candidates.extend(uniq_scores.keys().cloned());

    // 4) 计算 frecency 分（未归一）
    let mut fre_scores: HashMap<String, f64> = HashMap::with_capacity(candidates.len());
    for dir in &candidates {
        let s = idx.score_at(dir, now);
        if s > 0.0 {
            fre_scores.insert(dir.clone(), s);
        }
    }

    // 5) 归一化到 [0,1]
    let fre_norm = normalize01(&fre_scores);
    let uniq_norm = normalize01(&uniq_scores);

    // 6) 融合 + 阈值过滤 + 排序
    let mut items: Vec<(String, f64, f64, f64)> = Vec::with_capacity(candidates.len());
    let (wf, wu) = (opt.w_frecency, opt.w_uniq);
    for dir in candidates {
        let fz = *fre_norm.get(&dir).unwrap_or(&0.0);
        let uz = *uniq_norm.get(&dir).unwrap_or(&0.0);
        let final_score = wf * fz + wu * uz;
        if opt.threshold <= 0.0 || final_score >= opt.threshold {
            items.push((dir, final_score, fz, uz));
        }
    }

    // 主排序：final desc；次排序：frecency desc；再次：路径字典序
    items.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal))
            .then(a.0.cmp(&b.0))
    });

    items
        .into_iter()
        .take(opt.limit)
        .map(|(path, score, ..)| Recommendation { path, score })
        .collect()
}

/// 仅返回路径（同排序/同截断）
pub fn recommend_paths(opt: &RecommendOpt) -> Vec<String> {
    recommend(opt).into_iter().map(|r| r.path).collect()
}

/* ----------------------------- 内部实现细节 ----------------------------- */

/// 从 uniq 生成几何衰减分：
/// - 假设 uniq 文件通常“旧->新”，因此从尾到头赋分（最新=1.0，次新=decay，…）
/// - 支持 ignore_re / tokens / check_dir 过滤
fn load_uniq_scores(
    uniq_file: &str,
    ignore_re: &Option<Regex>,
    tokens_lc: &[String],
    check_dir: bool,
    decay: f64,
) -> HashMap<String, f64> {
    let f = match File::open(uniq_file) {
        Ok(f) => f,
        Err(_) => return HashMap::new(),
    };
    let mut lines: Vec<String> = BufReader::new(f)
        .lines()
        .flatten()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    if lines.is_empty() {
        return HashMap::new();
    }

    let mut scores = HashMap::with_capacity(lines.len());
    let mut k: usize = 0;
    for p in lines.drain(..).rev() {
        if let Some(rx) = ignore_re {
            if rx.is_match(&p) {
                continue;
            }
        }
        if !tokens_lc.is_empty() {
            let lp = p.to_lowercase();
            if !tokens_lc.iter().any(|tk| lp.contains(tk)) {
                continue;
            }
        }
        if check_dir && !Path::new(&p).is_dir() {
            continue;
        }
        let s = decay.powi(k as i32);
        // 若重复，保留更“新”的那次（分更大）
        scores
            .entry(p)
            .and_modify(|old| {
                if s > *old {
                    *old = s
                }
            })
            .or_insert(s);
        k += 1;
    }
    scores
}

/// 从 raw 流式构建 Frecency 索引，并记录出现过的路径
/// - 连续重复 (ts,path) 去重（防抖）
/// - 支持 ignore_re / tokens / check_dir 过滤
fn build_frecency_from_raw(
    raw_file: &str,
    ignore_re: &Option<Regex>,
    tokens_lc: &[String],
    check_dir: bool,
    half_life: f64,
) -> (FrecencyIndex, HashSet<String>) {
    let model = Frecency::new(half_life);
    let mut idx = FrecencyIndex::new(model);
    let mut seen: HashSet<String> = HashSet::new();

    let f = match File::open(raw_file) {
        Ok(f) => f,
        Err(_) => return (idx, seen),
    };

    let mut last: Option<(i64, String)> = None;
    for line in BufReader::new(f).lines().flatten() {
        if let Some((ts, p)) = line.split_once('\t') {
            if let Ok(t) = ts.parse::<i64>() {
                let path = p.trim().to_string();

                if let Some(rx) = ignore_re {
                    if rx.is_match(&path) {
                        continue;
                    }
                }
                if !tokens_lc.is_empty() {
                    let lp = path.to_lowercase();
                    if !tokens_lc.iter().any(|tk| lp.contains(tk)) {
                        continue;
                    }
                }
                if check_dir && !Path::new(&path).is_dir() {
                    continue;
                }
                if let Some((lts, ref lp)) = last {
                    if lts == t && lp == &path {
                        continue;
                    }
                }
                last = Some((t, path.clone()));
                idx.record_visit(path.clone(), t);
                seen.insert(path);
            }
        }
    }
    (idx, seen)
}

/// 把 map 的值线性归一化到 [0,1]
fn normalize01(map: &HashMap<String, f64>) -> HashMap<String, f64> {
    if map.is_empty() {
        return HashMap::new();
    }
    let (mut vmin, mut vmax) = (f64::INFINITY, f64::NEG_INFINITY);
    for &v in map.values() {
        if v < vmin {
            vmin = v
        }
        if v > vmax {
            vmax = v
        }
    }
    if !vmin.is_finite() || !vmax.is_finite() || (vmax - vmin).abs() < f64::EPSILON {
        // 退化：全部给 1.0（单元素或全相等），避免除零
        return map.iter().map(|(k, _)| (k.clone(), 1.0)).collect();
    }
    let span = vmax - vmin;
    map.iter()
        .map(|(k, &v)| (k.clone(), (v - vmin) / span))
        .collect()
}

/* ---------------------------------- 测试 ---------------------------------- */
#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::{env, fs};
    fn tmp_file(name: &str) -> String {
        let mut p = env::temp_dir();
        p.push(format!("cdh_test_{}_{}", name, now_secs()));
        p.to_string_lossy().to_string()
    }

    #[test]
    fn fusion_prefers_recent_unique_when_raw_ties() {
        // 构造 raw：两个路径访问次数相同 & 接近
        let raw = tmp_file("raw.tsv");
        let mut f = File::create(&raw).unwrap();
        // t, path
        writeln!(f, "{}\t/home/user/a", 1000).unwrap();
        writeln!(f, "{}\t/home/user/b", 1000).unwrap();
        writeln!(f, "{}\t/home/user/a", 2000).unwrap();
        writeln!(f, "{}\t/home/user/b", 2000).unwrap();

        // 构造 uniq：b 比 a 更新
        let uniq = tmp_file("uniq.txt");
        fs::write(&uniq, "/home/user/a\n/home/user/b\n").unwrap();

        // 创建假目录（check_dir=true 时需要存在）
        fs::create_dir_all("/tmp/cdh_test_a").ok();
        fs::create_dir_all("/tmp/cdh_test_b").ok();

        // 替换为真实存在路径以通过校验
        let raw_fixed = tmp_file("raw_fixed.tsv");
        let mut ff = File::create(&raw_fixed).unwrap();
        writeln!(ff, "{}\t/tmp/cdh_test_a", 1000).unwrap();
        writeln!(ff, "{}\t/tmp/cdh_test_b", 1000).unwrap();
        writeln!(ff, "{}\t/tmp/cdh_test_a", 2000).unwrap();
        writeln!(ff, "{}\t/tmp/cdh_test_b", 2000).unwrap();
        let uniq_fixed = tmp_file("uniq_fixed.txt");
        fs::write(&uniq_fixed, "/tmp/cdh_test_a\n/tmp/cdh_test_b\n").unwrap();

        let opt = RecommendOpt {
            raw: raw_fixed,
            uniq: uniq_fixed,
            limit: 2,
            half_life: 24.0 * 3600.0,
            threshold: 0.0,
            ignore_re: None,
            tokens: vec![],
            check_dir: true,
            uniq_decay: 0.85,
            w_frecency: 0.7,
            w_uniq: 0.3,
        };

        let out = recommend_with_now(&opt, 3000);
        assert_eq!(out.len(), 2);
        // b 更新更近，应优于 a
        assert_eq!(out[0].path, "/tmp/cdh_test_b");
        assert!(out[0].score >= out[1].score);
    }

    #[test]
    fn token_and_regex_filtering() {
        // raw + uniq 混合，只有包含 token 的且不匹配 ignore_re 的应留下
        let raw = tmp_file("raw2.tsv");
        let mut f = File::create(&raw).unwrap();
        writeln!(f, "{}\t/tmp/keep_alpha", 1).unwrap();
        writeln!(f, "{}\t/tmp/skip_beta", 2).unwrap();

        let uniq = tmp_file("uniq2.txt");
        fs::write(&uniq, "/tmp/keep_alpha\n/tmp/skip_beta\n").unwrap();

        fs::create_dir_all("/tmp/keep_alpha").ok();
        fs::create_dir_all("/tmp/skip_beta").ok();

        let ignore_re = Regex::new("skip_").ok();
        let opt = RecommendOpt {
            raw,
            uniq,
            limit: 10,
            half_life: 3600.0,
            threshold: 0.0,
            ignore_re,
            tokens: vec!["ALPHA".into()], // 大小写不敏感
            check_dir: true,
            uniq_decay: 0.85,
            w_frecency: 0.7,
            w_uniq: 0.3,
        };
        let paths = recommend_paths(&opt);
        assert_eq!(paths, vec!["/tmp/keep_alpha"]);
    }
}
