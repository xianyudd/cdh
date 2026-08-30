// src/recommend.rs
//! 智能目录推荐（融合多信号排序）
//!
//! 评分由四个归一化到 [0,1] 的分量线性融合（权重可配，默认 0.40/0.30/0.20/0.10）：
//! - **频次（frecency）**：raw 日志的时间衰减访问分，经 `ln(1+s)` 对数压缩，
//!   避免 $HOME 之类“巨鲸”目录把中部排序压扁；
//! - **最近性（recency）**：独立的短半衰期信号（默认 24h），让“刚去过”的目录浮上来；
//! - **上下文（context）**：从当前 `pwd` 出发的历史一阶转移权重 + 直接子目录小加成；
//! - **最近唯一（uniq）**：uniq 文件的几何衰减名次。
//!
//! 另有两处关键处理：
//! - **防抖**：同一目录在 `debounce_secs` 窗口内的重复访问只计一次频次，
//!   抵消“每开一个 shell / 标签就记一条 $HOME”导致的分数虚高；
//! - **排除 pwd**：当前目录不出现在结果里（跳到自己无意义）。
//!
//! 实现上流式读取 raw、低内存；一次性 lower tokens；可选校验目录存在性（WSL/网络盘可关）。
//! 支持阈值、关键词/正则过滤。
//!
//! 对外接口：
//! - `RecommendOpt`：融合推荐所有配置
//! - `Recommendation{ path, score }`：推荐结果
//! - `recommend(&RecommendOpt) -> Vec<Recommendation>`：路径+融合分（无关键词时按融合分排序；带关键词时优先按路径匹配质量排序）
//! - `recommend_paths(&RecommendOpt) -> Vec<String>`：仅路径
//! - `recommend_with_now(&RecommendOpt, now_secs)`：可注入“当前时间”的变体（便于测试）
//!
//! 依赖：本 crate 需已提供 `Frecency` / `FrecencyIndex`（见 src/frecency.rs）。
use crate::frecency::{Frecency, FrecencyIndex};
use crate::history;
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

/// 归一化到 [0,1] 的评分子信号。
#[derive(Debug, Clone, PartialEq)]
pub struct ScoreBreakdown {
    pub frecency_norm: f64,
    pub recency_norm: f64,
    pub context_norm: f64,
    pub uniq_norm: f64,
}

/// 推荐结果
#[derive(Debug, Clone)]
pub struct Recommendation {
    pub path: String,
    pub score: f64, // 融合后的最终分（0~1）
    pub breakdown: ScoreBreakdown,
    pub exists: bool,
}

/// 融合推荐的配置
#[derive(Debug, Clone)]
pub struct RecommendOpt {
    /// 原始频次日志：`ts<TAB>path`（由 controller 注入 XDG history_raw 路径）
    pub raw: String,
    /// 最近唯一列表：一行一个 path（由 controller 注入 XDG history_uniq 路径）
    pub uniq: String,
    /// 返回最大条数；None 表示不截断，让 TUI 搜索覆盖完整候选集
    pub limit: Option<usize>,
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
    /// 内部 TUI 开关：保留失效路径并用 `Recommendation::exists` 标记。
    pub include_missing: bool,
    /// uniq 的几何衰减系数（最新=1.0，次新=decay，…；默认 0.85）
    pub uniq_decay: f64,
    /// 当前工作目录：本身会从结果中排除，并作为“转移加成”的锚点。
    pub pwd: Option<String>,
    /// 最近性半衰期（秒）：独立于频次的“刚访问过”信号，默认 24 小时。
    pub recency_half_life: f64,
    /// 防抖窗口（秒）：同一目录在窗口内的重复访问只计一次频次，默认 600。
    /// 抑制“每开一个 shell 就给 $HOME 记一条”造成的分数虚高。
    pub debounce_secs: i64,
    /// 融合权重（建议总和为 1.0）：
    /// 频次（对数压缩），默认 0.40
    pub w_frecency: f64,
    /// uniq 最近唯一名次，默认 0.10
    pub w_uniq: f64,
    /// 最近性（短半衰期），默认 0.30
    pub w_recency: f64,
    /// 上下文（从 pwd 出发的历史转移 + 子目录加成），默认 0.20
    pub w_context: f64,
}

impl Default for RecommendOpt {
    fn default() -> Self {
        // 这里只保留“纯算法默认值”，不再读取环境变量，不再决定文件路径。
        // 路径由 AppContext::paths 决定，参数由 EffectiveConfig 决定。
        Self {
            raw: String::new(),             // 稍后由 controller 用 ctx.paths 覆盖
            uniq: String::new(),            // 同上
            limit: None,                    // 默认不截断；可被 config/CLI 覆盖
            half_life: 7.0 * 24.0 * 3600.0, // 默认 7 天；可被 config/CLI 覆盖
            threshold: 0.0,                 // 默认不开启阈值
            ignore_re: None,                // 默认不忽略任何路径；可由 config/CLI 覆盖
            tokens: Vec::new(),
            check_dir: true, // 默认检查目录存在性；可被 config 覆盖
            include_missing: false,
            uniq_decay: 0.85,
            pwd: None,
            recency_half_life: 24.0 * 3600.0,
            debounce_secs: 600,
            w_frecency: 0.40,
            w_uniq: 0.10,
            w_recency: 0.30,
            w_context: 0.20,
        }
    }
}

#[derive(Debug)]
struct RankedItem {
    path: String,
    final_score: f64,
    breakdown: ScoreBreakdown,
    exists: bool,
    frecency_score: f64,
    match_quality: f64,
}

/// 外部主接口：融合 RAW+UNIQ，返回路径+融合分。
/// 无关键词时按融合分降序；带关键词时优先按路径匹配质量排序。
pub fn recommend(opt: &RecommendOpt) -> Vec<Recommendation> {
    recommend_with_now(opt, now_secs())
}

/// 变体：可注入“当前时间”，便于测试
pub fn recommend_with_now(opt: &RecommendOpt, now: i64) -> Vec<Recommendation> {
    // 预处理 tokens（一次性 lower）
    let tokens_lc: Vec<String> = opt
        .tokens
        .iter()
        .map(|t| t.trim().to_lowercase())
        .filter(|t| !t.is_empty())
        .collect();

    // 1) uniq -> 生成 “最近唯一”几何衰减分（本身已在 (0,1]，无需再归一化）
    let uniq_scores = load_uniq_scores(
        &opt.uniq,
        &opt.ignore_re,
        &tokens_lc,
        opt.check_dir,
        opt.include_missing,
        opt.uniq_decay,
    );

    // 2) raw -> 流式提取信号：频次索引 + 每目录最近访问时刻 + 从 pwd 出发的转移权重
    let signals = build_raw_signals(opt, &tokens_lc, now);

    // 3) 候选集 = raw ∪ uniq，排除当前目录（跳到自己所在目录没有意义）
    let mut candidates: HashSet<String> = signals.seen;
    candidates.extend(uniq_scores.keys().cloned());
    if let Some(pwd) = &opt.pwd {
        candidates.remove(pwd);
    }

    // 4) 频次分（对数压缩）：ln(1+s)/ln(1+s_max)。
    //    线性 min-max 会被“巨鲸”目录（分数是他人几十倍）压扁中部排序，对数压缩保留区分度。
    let mut fre_max = 0.0f64;
    for dir in &candidates {
        fre_max = fre_max.max(signals.idx.score_at(dir, now));
    }
    let fre_log_max = (1.0 + fre_max).ln();

    // 5) 上下文分归一化基准
    let ctx_max = signals
        .transitions
        .values()
        .fold(0.0f64, |acc, &v| acc.max(v));

    // 6) 四分量融合 + 阈值过滤 + 排序
    let mut items: Vec<RankedItem> = Vec::with_capacity(candidates.len());
    for dir in candidates {
        let exists = !opt.check_dir || Path::new(&dir).is_dir();
        // 频次（对数压缩后 0~1）
        let fz = {
            let s = signals.idx.score_at(&dir, now);
            if s > 0.0 && fre_log_max > 0.0 {
                (1.0 + s).ln() / fre_log_max
            } else {
                0.0
            }
        };
        // 最近性：短半衰期的“刚访问过”信号（0~1）
        let rz = signals
            .last_visit
            .get(&dir)
            .map(|&t| decay_weight(now - t, opt.recency_half_life))
            .unwrap_or(0.0);
        // 上下文：从 pwd 出发的历史转移 + 子目录小加成（0~1）
        let cz = {
            let trans = match signals.transitions.get(&dir) {
                Some(&v) if ctx_max > 0.0 => v / ctx_max,
                _ => 0.0,
            };
            let subdir_bonus = match &opt.pwd {
                Some(pwd) if is_direct_child(&dir, pwd) => 0.25,
                _ => 0.0,
            };
            (trans + subdir_bonus).min(1.0)
        };
        // 最近唯一名次（0~1）
        let uz = *uniq_scores.get(&dir).unwrap_or(&0.0);

        let final_score =
            opt.w_frecency * fz + opt.w_recency * rz + opt.w_context * cz + opt.w_uniq * uz;
        if opt.threshold <= 0.0 || final_score >= opt.threshold {
            let match_quality = if tokens_lc.is_empty() {
                0.0
            } else {
                keyword_match_quality_lc(&dir.to_lowercase(), &tokens_lc)
            };
            items.push(RankedItem {
                path: dir,
                final_score,
                breakdown: ScoreBreakdown {
                    frecency_norm: fz,
                    recency_norm: rz,
                    context_norm: cz,
                    uniq_norm: uz,
                },
                exists,
                frecency_score: fz,
                match_quality,
            });
        }
    }

    if tokens_lc.is_empty() {
        // 主排序：final desc；次排序：frecency desc；再次：路径字典序
        items.sort_by(|a, b| {
            b.exists
                .cmp(&a.exists)
                .then_with(|| cmp_f64_desc(b.final_score, a.final_score))
                .then_with(|| cmp_f64_desc(b.frecency_score, a.frecency_score))
                .then(a.path.cmp(&b.path))
        });
    } else {
        // 带关键词时，先按路径匹配质量排序，再回退到历史推荐分。
        items.sort_by(|a, b| {
            b.exists
                .cmp(&a.exists)
                .then_with(|| cmp_f64_desc(b.match_quality, a.match_quality))
                .then_with(|| cmp_f64_desc(b.final_score, a.final_score))
                .then_with(|| cmp_f64_desc(b.frecency_score, a.frecency_score))
                .then(a.path.cmp(&b.path))
        });
    }

    let iter = items.into_iter().map(|item| Recommendation {
        path: item.path,
        score: item.final_score,
        breakdown: item.breakdown,
        exists: item.exists,
    });
    match opt.limit {
        Some(limit) => iter.take(limit).collect(),
        None => iter.collect(),
    }
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
    include_missing: bool,
    decay: f64,
) -> HashMap<String, f64> {
    let f = match File::open(uniq_file) {
        Ok(f) => f,
        Err(_) => return HashMap::new(),
    };
    let mut lines = read_trimmed_lines(BufReader::new(f));

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
            if !path_matches_tokens_lc(&lp, tokens_lc) {
                continue;
            }
        }
        if check_dir && !include_missing && !Path::new(&p).is_dir() {
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

/// 从 raw 流式提取的全部排序信号。
struct RawSignals {
    /// 频次索引（带防抖）
    idx: FrecencyIndex,
    /// 出现过的路径（候选集来源）
    seen: HashSet<String>,
    /// 每目录最近一次访问时刻（不受防抖影响，反映真实最近性）
    last_visit: HashMap<String, i64>,
    /// 从 `opt.pwd` 出发的转移权重：key = 紧跟在 pwd 之后访问的目录，
    /// value = Σ 0.5^((now-t)/half_life)（越近的转移权重越大）
    transitions: HashMap<String, f64>,
}

/// 流式扫描 raw：
/// - 防抖：同一目录在 `debounce_secs` 窗口内的重复访问只计一次频次
///   （新开 shell/多标签会反复记录同一目录，不防抖会把 $HOME 类目录分数灌爆）；
/// - last_visit：始终更新（防抖只影响频次，不影响“最近去过”事实）；
/// - transitions：统计紧跟在 pwd 之后访问的目录（一阶转移），带时间衰减。
fn build_raw_signals(opt: &RecommendOpt, tokens_lc: &[String], now: i64) -> RawSignals {
    let model = Frecency::new(opt.half_life);
    let mut signals = RawSignals {
        idx: FrecencyIndex::new(model),
        seen: HashSet::new(),
        last_visit: HashMap::new(),
        transitions: HashMap::new(),
    };

    let f = match File::open(&opt.raw) {
        Ok(f) => f,
        Err(_) => return signals,
    };

    // 每目录“上次计入频次”的时刻（防抖窗口判断用）
    let mut last_counted: HashMap<String, i64> = HashMap::new();
    // 上一条计入的访问（转移统计的前驱）
    let mut prev: Option<(i64, String)> = None;

    for line in read_trimmed_lines(BufReader::new(f)) {
        let Some((ts, p)) = line.split_once('\t') else {
            continue;
        };
        let Some(t) = history::parse_history_ts_secs(ts) else {
            continue;
        };
        let path = p.trim().to_string();

        if let Some(rx) = &opt.ignore_re {
            if rx.is_match(&path) {
                continue;
            }
        }
        if !tokens_lc.is_empty() {
            let lp = path.to_lowercase();
            if !path_matches_tokens_lc(&lp, tokens_lc) {
                continue;
            }
        }
        if opt.check_dir && !opt.include_missing && !Path::new(&path).is_dir() {
            continue;
        }

        // 最近访问时刻始终更新
        signals
            .last_visit
            .entry(path.clone())
            .and_modify(|old| *old = (*old).max(t))
            .or_insert(t);
        signals.seen.insert(path.clone());

        // 防抖：窗口内的重复访问不再计入频次/转移
        if let Some(&last) = last_counted.get(&path) {
            let dt = t - last;
            if dt >= 0 && dt < opt.debounce_secs {
                continue;
            }
        }
        last_counted.insert(path.clone(), t);

        // 转移统计：前驱是 pwd 且目标不同，则给目标加衰减权重
        if let (Some(pwd), Some((_, prev_path))) = (&opt.pwd, &prev) {
            if prev_path == pwd && path != *pwd {
                *signals.transitions.entry(path.clone()).or_insert(0.0) +=
                    decay_weight(now - t, opt.half_life);
            }
        }

        signals.idx.record_visit(path.clone(), t);
        prev = Some((t, path));
    }
    signals
}

/// 指数衰减权重：0.5^(dt/half_life)，dt<=0 时为 1.0。
fn decay_weight(dt_secs: i64, half_life: f64) -> f64 {
    if dt_secs <= 0 {
        1.0
    } else {
        0.5f64.powf(dt_secs as f64 / half_life)
    }
}

/// `dir` 是否是 `parent` 的直接子目录（如 /a/b 之于 /a；/a/b/c 不算）。
fn is_direct_child(dir: &str, parent: &str) -> bool {
    match dir.strip_prefix(parent) {
        Some(rest) => {
            let rest = rest.strip_prefix('/').unwrap_or(rest);
            let rest = rest.strip_suffix('/').unwrap_or(rest); // 容忍末尾斜杠
            !rest.is_empty() && !rest.contains('/')
        }
        None => false,
    }
}

fn cmp_f64_desc(left: f64, right: f64) -> std::cmp::Ordering {
    left.partial_cmp(&right)
        .unwrap_or(std::cmp::Ordering::Equal)
}

fn read_trimmed_lines(reader: impl BufRead) -> Vec<String> {
    let mut lines = Vec::new();
    for line_res in reader.lines() {
        let line = match line_res {
            Ok(line) => line,
            Err(_) => continue,
        };
        let line = line.trim();
        if !line.is_empty() {
            lines.push(line.to_string());
        }
    }
    lines
}

fn path_matches_tokens_lc(path_lc: &str, tokens_lc: &[String]) -> bool {
    tokens_lc.iter().any(|token| path_lc.contains(token))
}

fn keyword_match_quality_lc(path_lc: &str, tokens_lc: &[String]) -> f64 {
    if tokens_lc.is_empty() {
        return 0.0;
    }

    let mut matched = 0usize;
    let mut sum = 0.0f64;
    let mut best = 0.0f64;

    for token in tokens_lc {
        let quality = token_match_quality_lc(path_lc, token);
        if quality > 0.0 {
            matched += 1;
            sum += quality;
            if quality > best {
                best = quality;
            }
        }
    }

    if matched == 0 {
        return 0.0;
    }

    let coverage = matched as f64 / tokens_lc.len() as f64;
    let average_quality = sum / matched as f64;
    coverage * 1000.0 + average_quality + best / 1000.0
}

fn token_match_quality_lc(path_lc: &str, token_lc: &str) -> f64 {
    if token_lc.is_empty() {
        return 0.0;
    }

    let bytes = path_lc.as_bytes();
    let token_len = token_lc.len();
    let mut best = 0.0;

    for (start, _) in path_lc.match_indices(token_lc) {
        let end = start + token_len;
        let segment_start_idx = path_lc[..start]
            .rfind(['/', '\\'])
            .map(|idx| idx + 1)
            .unwrap_or(0);
        let segment_end_idx = path_lc[end..]
            .find(['/', '\\'])
            .map(|idx| end + idx)
            .unwrap_or(path_lc.len());
        let distance_from_basename = path_lc[segment_end_idx..]
            .bytes()
            .filter(|&byte| matches!(byte, b'/' | b'\\'))
            .count();

        // Avoid letting broad ancestors such as `github.com` make every repository
        // look like a strong match for `git`. A token must match the basename or
        // its direct parent segment; broader ancestors are too generic for ranking.
        if distance_from_basename >= 2 {
            continue;
        }

        let segment_start = start == segment_start_idx;
        let segment_end = end == segment_end_idx;
        let before_boundary = start == 0 || is_match_boundary(bytes.get(start - 1).copied());
        let after_boundary = end == path_lc.len() || is_match_boundary(bytes.get(end).copied());
        let in_basename = distance_from_basename == 0;

        let quality = if segment_start && segment_end {
            if in_basename {
                120.0
            } else {
                100.0
            }
        } else if before_boundary && after_boundary {
            if in_basename {
                110.0
            } else {
                90.0
            }
        } else if !in_basename {
            // Parent directory matches are only useful when they match a clear token
            // boundary. Prefixes like `git` in `github_parent` are too broad and
            // would keep unrelated children such as `github_parent/plain-project`.
            0.0
        } else if segment_start {
            100.0
        } else if before_boundary {
            90.0
        } else {
            35.0
        };

        if quality > best {
            best = quality;
        }
    }

    best
}

fn is_match_boundary(byte: Option<u8>) -> bool {
    matches!(
        byte,
        None | Some(b'/') | Some(b'\\') | Some(b'-') | Some(b'_') | Some(b'.') | Some(b' ')
    )
}

/* ---------------------------------- 测试 ---------------------------------- */
#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::process;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::{env, fs};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    /// 一次测试的临时根目录：raw/uniq 和候选目录都放在里面，`Drop` 时整棵删掉。
    ///
    /// 名字带 pid 和单调序号，所以并行测试互不干扰。此前这里既有只拼路径、
    /// 从不删除的 `tmp_file`，也有 `/tmp/cdh_test_a` 这类固定名字——后者跨进程共享，
    /// 顺手删掉会踩到同时在跑的另一个测试进程。
    ///
    /// 前缀刻意不含任何测试里用到的 token（`git` / `cdh` / `workspace` / `alpha`
    /// / `beta`）：token 过滤是对整条绝对路径做子串匹配的（见 `path_matches_tokens_lc`），
    /// 根目录名里只要出现 token，就会让本该只命中一个 token 的候选凭空多命中一个，
    /// 把按命中数排序的断言悄悄变成恒真。
    struct TempCase {
        root: PathBuf,
    }

    impl TempCase {
        fn new(name: &str) -> Self {
            let seq = SEQ.fetch_add(1, Ordering::Relaxed);
            let root = env::temp_dir().join(format!(
                "recommend-tests-{name}-{}-{}-{seq}",
                process::id(),
                now_secs()
            ));
            fs::create_dir_all(&root).unwrap();
            Self { root }
        }

        fn root(&self) -> &Path {
            &self.root
        }

        /// 根内的一个文件路径；只拼不建，交给调用方写入。
        fn file(&self, rel: &str) -> String {
            self.root.join(rel).to_string_lossy().into_owned()
        }

        /// 根内的一个子目录，创建后返回。
        fn dir(&self, rel: &str) -> PathBuf {
            let path = self.root.join(rel);
            fs::create_dir_all(&path).unwrap();
            path
        }

        /// 根内的一个路径；只拼不建，供调用方自行 `create_dir_all`。
        fn join(&self, rel: &str) -> PathBuf {
            self.root.join(rel)
        }
    }

    impl Drop for TempCase {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn fusion_prefers_recent_unique_when_raw_ties() {
        let case = TempCase::new("fusion");
        // 候选目录必须真实存在（check_dir=true）。
        let dir_a = case.dir("a");
        let dir_b = case.dir("b");

        // 构造 raw：两个路径访问次数相同 & 接近
        let raw = case.file("raw.tsv");
        let mut f = File::create(&raw).unwrap();
        // t, path
        writeln!(f, "{}\t{}", 1000, dir_a.display()).unwrap();
        writeln!(f, "{}\t{}", 1000, dir_b.display()).unwrap();
        writeln!(f, "{}\t{}", 2000, dir_a.display()).unwrap();
        writeln!(f, "{}\t{}", 2000, dir_b.display()).unwrap();

        // 构造 uniq：b 比 a 更新
        let uniq = case.file("uniq.txt");
        fs::write(&uniq, format!("{}\n{}\n", dir_a.display(), dir_b.display())).unwrap();

        let opt = RecommendOpt {
            raw,
            uniq,
            limit: Some(2),
            half_life: 24.0 * 3600.0,
            threshold: 0.0,
            ignore_re: None,
            tokens: vec![],
            check_dir: true,
            uniq_decay: 0.85,
            w_frecency: 0.7,
            w_uniq: 0.3,
            ..RecommendOpt::default()
        };

        let out = recommend_with_now(&opt, 3000);
        assert_eq!(out.len(), 2);
        // b 更新更近，应优于 a
        assert_eq!(out[0].path, dir_b.to_string_lossy());
        assert!(out[0].score >= out[1].score);
    }

    #[test]
    fn token_and_regex_filtering() {
        // raw + uniq 混合，只有包含 token 的且不匹配 ignore_re 的应留下
        let case = TempCase::new("token_regex");
        let keep = case.dir("keep_alpha");
        let skip = case.dir("skip_beta");

        let raw = case.file("raw2.tsv");
        let mut f = File::create(&raw).unwrap();
        writeln!(f, "{}\t{}", 1, keep.display()).unwrap();
        writeln!(f, "{}\t{}", 2, skip.display()).unwrap();

        let uniq = case.file("uniq2.txt");
        fs::write(&uniq, format!("{}\n{}\n", keep.display(), skip.display())).unwrap();

        let ignore_re = Regex::new("skip_").ok();
        let opt = RecommendOpt {
            raw,
            uniq,
            limit: Some(10),
            half_life: 3600.0,
            threshold: 0.0,
            ignore_re,
            tokens: vec!["ALPHA".into()], // 大小写不敏感
            check_dir: true,
            uniq_decay: 0.85,
            w_frecency: 0.7,
            w_uniq: 0.3,
            ..RecommendOpt::default()
        };
        let paths = recommend_paths(&opt);
        assert_eq!(paths, vec![keep.to_string_lossy().to_string()]);
    }

    #[test]
    fn default_limit_does_not_truncate_candidates() {
        let case = TempCase::new("unlimited");
        let raw = case.file("raw_unlimited.tsv");
        let uniq = case.file("uniq_unlimited.txt");

        let mut raw_file = File::create(&raw).unwrap();
        let mut uniq_body = String::new();
        for i in 0..25 {
            let dir = case.dir(&format!("dir_{i:02}"));
            writeln!(raw_file, "{}\t{}", 1_000 + i, dir.display()).unwrap();
            uniq_body.push_str(&format!("{}\n", dir.display()));
        }
        fs::write(&uniq, uniq_body).unwrap();

        let opt = RecommendOpt {
            raw,
            uniq,
            limit: None,
            half_life: 24.0 * 3600.0,
            threshold: 0.0,
            ignore_re: None,
            tokens: vec![],
            check_dir: true,
            uniq_decay: 0.85,
            w_frecency: 0.7,
            w_uniq: 0.3,
            ..RecommendOpt::default()
        };

        let out = recommend_with_now(&opt, 2_000);
        assert_eq!(out.len(), 25);
    }

    #[test]
    fn recommend_normalizes_millisecond_timestamps() {
        let case = TempCase::new("millis");
        let raw = case.file("raw_millis.tsv");
        let uniq = case.file("uniq_millis.txt");
        let base = case.root().to_path_buf();
        let older = base.join("older");
        let newer = base.join("newer");
        fs::create_dir_all(&older).unwrap();
        fs::create_dir_all(&newer).unwrap();

        let mut raw_file = File::create(&raw).unwrap();
        writeln!(raw_file, "1000000000000\t{}", older.display()).unwrap();
        writeln!(raw_file, "1000000100\t{}", newer.display()).unwrap();
        fs::write(&uniq, "").unwrap();

        let opt = RecommendOpt {
            raw,
            uniq,
            limit: None,
            half_life: 10_000.0,
            threshold: 0.0,
            ignore_re: None,
            tokens: vec![],
            check_dir: true,
            uniq_decay: 0.85,
            w_frecency: 1.0,
            w_uniq: 0.0,
            w_recency: 0.0,
            w_context: 0.0,
            ..RecommendOpt::default()
        };

        let out = recommend_with_now(&opt, 1_000_000_200);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].path, newer.to_string_lossy());
    }

    #[test]
    fn keyword_quality_prefers_segment_exact_over_plain_substring() {
        let case = TempCase::new("keyword_exact");
        let raw = case.file("raw_keyword_exact.tsv");
        let uniq = case.file("uniq_keyword_exact.txt");
        let base = case.root().to_path_buf();
        let exact = base.join("git");
        let weak = base.join("digital-archive");
        fs::create_dir_all(&exact).unwrap();
        fs::create_dir_all(&weak).unwrap();

        let mut raw_file = File::create(&raw).unwrap();
        writeln!(raw_file, "1000\t{}", exact.display()).unwrap();
        for ts in 2000..2005 {
            writeln!(raw_file, "{}\t{}", ts, weak.display()).unwrap();
        }
        fs::write(&uniq, format!("{}\n{}\n", exact.display(), weak.display())).unwrap();

        let opt = RecommendOpt {
            raw,
            uniq,
            limit: None,
            half_life: 24.0 * 3600.0,
            threshold: 0.0,
            ignore_re: None,
            tokens: vec!["git".into()],
            check_dir: true,
            uniq_decay: 0.85,
            w_frecency: 0.7,
            w_uniq: 0.3,
            ..RecommendOpt::default()
        };

        let out = recommend_with_now(&opt, 3000);
        assert_eq!(out[0].path, exact.to_string_lossy());
    }

    #[test]
    fn keyword_quality_prefers_paths_matching_more_tokens() {
        let case = TempCase::new("keyword_multi");
        let raw = case.file("raw_keyword_multi.tsv");
        let uniq = case.file("uniq_keyword_multi.txt");
        let base = case.root().to_path_buf();
        let one_token = base.join("git-only");
        let two_tokens = base.join("cdh-git");
        fs::create_dir_all(&one_token).unwrap();
        fs::create_dir_all(&two_tokens).unwrap();

        let mut raw_file = File::create(&raw).unwrap();
        for ts in 1000..1005 {
            writeln!(raw_file, "{}\t{}", ts, one_token.display()).unwrap();
        }
        writeln!(raw_file, "2000\t{}", two_tokens.display()).unwrap();
        fs::write(
            &uniq,
            format!("{}\n{}\n", two_tokens.display(), one_token.display()),
        )
        .unwrap();

        let opt = RecommendOpt {
            raw,
            uniq,
            limit: None,
            half_life: 24.0 * 3600.0,
            threshold: 0.0,
            ignore_re: None,
            tokens: vec!["git".into(), "cdh".into()],
            check_dir: true,
            uniq_decay: 0.85,
            w_frecency: 0.7,
            w_uniq: 0.3,
            ..RecommendOpt::default()
        };

        let out = recommend_with_now(&opt, 3000);
        assert_eq!(out[0].path, two_tokens.to_string_lossy());
    }

    #[test]
    fn keyword_filter_keeps_parent_substring_but_ranks_path_match_first() {
        let case = TempCase::new("keyword_parent");
        let raw = case.file("raw_keyword_parent.tsv");
        let uniq = case.file("uniq_keyword_parent.txt");
        // token 必须落在**祖先目录名**里：`unrelated` 自己的名字不含 `git`，
        // 只能靠父目录 `github` 命中，这正是本用例要测的路径。
        let base = case.dir("github");
        let unrelated = base.join("plain-project");
        let target = base.join("git-tools");
        fs::create_dir_all(&unrelated).unwrap();
        fs::create_dir_all(&target).unwrap();

        let mut raw_file = File::create(&raw).unwrap();
        writeln!(raw_file, "1000\t{}", unrelated.display()).unwrap();
        writeln!(raw_file, "1001\t{}", target.display()).unwrap();
        fs::write(
            &uniq,
            format!("{}\n{}\n", unrelated.display(), target.display()),
        )
        .unwrap();

        let opt = RecommendOpt {
            raw,
            uniq,
            limit: None,
            half_life: 24.0 * 3600.0,
            threshold: 0.0,
            ignore_re: None,
            tokens: vec!["git".into()],
            check_dir: true,
            uniq_decay: 0.85,
            w_frecency: 0.7,
            w_uniq: 0.3,
            ..RecommendOpt::default()
        };

        let paths = recommend_paths(&opt);
        assert_eq!(paths[0], target.to_string_lossy().to_string());
        assert!(paths.contains(&unrelated.to_string_lossy().to_string()));
    }

    #[test]
    fn keyword_filter_keeps_grandparent_substring_matches() {
        let case = TempCase::new("keyword_grandparent");
        let raw = case.file("raw_keyword_grandparent.tsv");
        let uniq = case.file("uniq_keyword_grandparent.txt");
        // token 只出现在祖父目录名里，末段和父段都不含它——这正是本例要覆盖的情形。
        let target = case.join("workspace/repos/cdh");
        fs::create_dir_all(&target).unwrap();

        let mut raw_file = File::create(&raw).unwrap();
        writeln!(raw_file, "1000\t{}", target.display()).unwrap();
        fs::write(&uniq, format!("{}\n", target.display())).unwrap();

        let opt = RecommendOpt {
            raw,
            uniq,
            limit: None,
            half_life: 24.0 * 3600.0,
            threshold: 0.0,
            ignore_re: None,
            tokens: vec!["workspace".into()],
            check_dir: true,
            uniq_decay: 0.85,
            w_frecency: 0.7,
            w_uniq: 0.3,
            ..RecommendOpt::default()
        };

        let paths = recommend_paths(&opt);
        assert_eq!(paths, vec![target.to_string_lossy().to_string()]);
    }

    #[test]
    fn token_filtering_keeps_or_semantics() {
        let case = TempCase::new("keyword_or");
        let raw = case.file("raw_keyword_or.tsv");
        let uniq = case.file("uniq_keyword_or.txt");
        let base = case.root().to_path_buf();
        let alpha = base.join("alpha-project");
        let beta = base.join("beta-project");
        let gamma = base.join("gamma-project");
        fs::create_dir_all(&alpha).unwrap();
        fs::create_dir_all(&beta).unwrap();
        fs::create_dir_all(&gamma).unwrap();

        let mut raw_file = File::create(&raw).unwrap();
        writeln!(raw_file, "1000\t{}", alpha.display()).unwrap();
        writeln!(raw_file, "1001\t{}", beta.display()).unwrap();
        writeln!(raw_file, "1002\t{}", gamma.display()).unwrap();
        fs::write(
            &uniq,
            format!(
                "{}\n{}\n{}\n",
                alpha.display(),
                beta.display(),
                gamma.display()
            ),
        )
        .unwrap();

        let opt = RecommendOpt {
            raw,
            uniq,
            limit: None,
            half_life: 24.0 * 3600.0,
            threshold: 0.0,
            ignore_re: None,
            tokens: vec!["alpha".into(), "beta".into()],
            check_dir: true,
            uniq_decay: 0.85,
            w_frecency: 0.7,
            w_uniq: 0.3,
            ..RecommendOpt::default()
        };

        let paths = recommend_paths(&opt);
        assert!(paths.contains(&alpha.to_string_lossy().to_string()));
        assert!(paths.contains(&beta.to_string_lossy().to_string()));
        assert!(!paths.contains(&gamma.to_string_lossy().to_string()));
    }

    /// 辅助：在一个自清理的临时根里备好 raw/uniq 路径。
    ///
    /// 返回的 `TempCase` 必须被绑定住：一旦提前 drop，整棵目录就没了。
    fn setup(name: &str) -> (String, String, TempCase) {
        let case = TempCase::new(name);
        let raw = case.file(&format!("{name}_raw"));
        let uniq = case.file(&format!("{name}_uniq"));
        (raw, uniq, case)
    }

    #[test]
    fn recency_lets_fresh_dir_beat_stale_high_frequency() {
        // frequent：很久以前被访问过很多次；fresh：刚刚访问过一次。
        // 旧算法（纯 frecency）frequent 会赢；新算法有 recency 分量，fresh 应更靠前或接近。
        let (raw, uniq, base) = setup("recency");
        let frequent = base.join("old-but-frequent");
        let fresh = base.join("just-now");
        fs::create_dir_all(&frequent).unwrap();
        fs::create_dir_all(&fresh).unwrap();

        let now = 1_000_000i64;
        let mut rf = File::create(&raw).unwrap();
        // frequent：30 天前访问 20 次（半衰期 7 天，已衰减很多）
        for i in 0..20 {
            writeln!(
                rf,
                "{}\t{}",
                now - 30 * 86400 + i * 1000,
                frequent.display()
            )
            .unwrap();
        }
        // fresh：1 分钟前访问 1 次
        writeln!(rf, "{}\t{}", now - 60, fresh.display()).unwrap();
        fs::write(
            &uniq,
            format!("{}\n{}\n", frequent.display(), fresh.display()),
        )
        .unwrap();

        let opt = RecommendOpt {
            raw,
            uniq,
            check_dir: true,
            ..RecommendOpt::default()
        };
        let out = recommend_with_now(&opt, now);
        assert_eq!(
            out[0].path,
            fresh.to_string_lossy(),
            "fresh 应排第一, got {out:?}"
        );
    }

    #[test]
    fn debounce_folds_burst_visits() {
        // 同一目录在防抖窗口内被记录很多次（新开 shell/多标签），
        // 不应因此在频次上碾压一个真正被多次“独立”访问的目录。
        let (raw, uniq, base) = setup("debounce");
        let bursty = base.join("home-like");
        let genuine = base.join("real-work");
        fs::create_dir_all(&bursty).unwrap();
        fs::create_dir_all(&genuine).unwrap();

        let now = 1_000_000i64;
        let mut rf = File::create(&raw).unwrap();
        // bursty：同一秒附近记录 50 次（窗口内），防抖后只算 1 次
        for i in 0..50 {
            writeln!(rf, "{}\t{}", now - 300 + i, bursty.display()).unwrap();
        }
        // genuine：10 次真正间隔开的访问（每次间隔 > 防抖窗口）
        for i in 0..10 {
            writeln!(rf, "{}\t{}", now - i * 1800, genuine.display()).unwrap();
        }
        fs::write(
            &uniq,
            format!("{}\n{}\n", bursty.display(), genuine.display()),
        )
        .unwrap();

        let opt = RecommendOpt {
            raw,
            uniq,
            check_dir: true,
            // 仅看频次，隔离防抖效果
            w_frecency: 1.0,
            w_uniq: 0.0,
            w_recency: 0.0,
            w_context: 0.0,
            ..RecommendOpt::default()
        };
        let out = recommend_with_now(&opt, now);
        // 防抖生效：genuine（10 次独立访问）应压过 bursty（50 次被折叠成 1）
        assert_eq!(
            out[0].path,
            genuine.to_string_lossy(),
            "genuine 应排第一, got {out:?}"
        );
    }

    #[test]
    fn pwd_is_excluded_from_results() {
        let (raw, uniq, base) = setup("exclude_pwd");
        let here = base.join("current");
        let other = base.join("other");
        fs::create_dir_all(&here).unwrap();
        fs::create_dir_all(&other).unwrap();

        let mut rf = File::create(&raw).unwrap();
        writeln!(rf, "1000\t{}", here.display()).unwrap();
        writeln!(rf, "1001\t{}", other.display()).unwrap();
        fs::write(&uniq, format!("{}\n{}\n", here.display(), other.display())).unwrap();

        let opt = RecommendOpt {
            raw,
            uniq,
            check_dir: true,
            pwd: Some(here.to_string_lossy().into_owned()),
            ..RecommendOpt::default()
        };
        let paths = recommend_paths(&opt);
        assert!(
            !paths.contains(&here.to_string_lossy().to_string()),
            "pwd 应被排除"
        );
        assert!(paths.contains(&other.to_string_lossy().to_string()));
    }

    #[test]
    fn context_boosts_transition_target() {
        // 历史里从 pwd 出发常去 target；另一个 rival 频次相同但与 pwd 无转移关系。
        // context 分量应把 target 顶到前面。
        let (raw, uniq, base) = setup("context");
        let pwd = base.join("project-root");
        let target = base.join("frequent-next");
        let rival = base.join("unrelated");
        fs::create_dir_all(&pwd).unwrap();
        fs::create_dir_all(&target).unwrap();
        fs::create_dir_all(&rival).unwrap();

        let now = 1_000_000i64;
        let mut rf = File::create(&raw).unwrap();
        // 交替 pwd -> target 五轮（建立转移），rival 独立访问同样次数
        for i in 0..5 {
            let t = now - (10 - i) * 5000;
            writeln!(rf, "{}\t{}", t, pwd.display()).unwrap();
            writeln!(rf, "{}\t{}", t + 100, target.display()).unwrap();
            writeln!(rf, "{}\t{}", t + 200, rival.display()).unwrap();
        }
        fs::write(
            &uniq,
            format!(
                "{}\n{}\n{}\n",
                pwd.display(),
                target.display(),
                rival.display()
            ),
        )
        .unwrap();

        let opt = RecommendOpt {
            raw,
            uniq,
            check_dir: true,
            pwd: Some(pwd.to_string_lossy().into_owned()),
            ..RecommendOpt::default()
        };
        let out = recommend_with_now(&opt, now);
        assert_eq!(
            out[0].path,
            target.to_string_lossy(),
            "转移目标应排第一, got {out:?}"
        );
    }

    #[test]
    fn log_compression_keeps_mid_tier_distinguishable() {
        // whale：频次是他人几十倍。线性 min-max 会把中部三项压到几乎并列 0，
        // 对数压缩后中部仍应按频次拉开且严格降序。
        let (raw, uniq, base) = setup("logcomp");
        let whale = base.join("whale");
        let mid_hi = base.join("mid-hi");
        let mid_lo = base.join("mid-lo");
        for d in [&whale, &mid_hi, &mid_lo] {
            fs::create_dir_all(d).unwrap();
        }

        let now = 1_000_000i64;
        let mut rf = File::create(&raw).unwrap();
        // 每次访问间隔 > 防抖窗口，确保都计入频次
        for i in 0..100 {
            writeln!(rf, "{}\t{}", now - i * 1000, whale.display()).unwrap();
        }
        for i in 0..8 {
            writeln!(rf, "{}\t{}", now - i * 1000, mid_hi.display()).unwrap();
        }
        for i in 0..3 {
            writeln!(rf, "{}\t{}", now - i * 1000, mid_lo.display()).unwrap();
        }
        fs::write(
            &uniq,
            format!(
                "{}\n{}\n{}\n",
                whale.display(),
                mid_hi.display(),
                mid_lo.display()
            ),
        )
        .unwrap();

        let opt = RecommendOpt {
            raw,
            uniq,
            check_dir: true,
            w_frecency: 1.0,
            w_uniq: 0.0,
            w_recency: 0.0,
            w_context: 0.0,
            ..RecommendOpt::default()
        };
        let out = recommend_with_now(&opt, now);
        let score_of = |p: &std::path::Path| {
            out.iter()
                .find(|r| r.path == p.to_string_lossy())
                .map(|r| r.score)
                .unwrap()
        };
        // 中部两项分数应明显区分（差值 > 0.05），而非被 whale 压成并列。
        let gap = score_of(&mid_hi) - score_of(&mid_lo);
        assert!(gap > 0.05, "中部区分度不足: mid_hi-mid_lo={gap}");
    }

    #[test]
    fn is_direct_child_semantics() {
        assert!(is_direct_child("/a/b", "/a"));
        assert!(is_direct_child("/a/b/", "/a")); // 末尾斜杠边界
        assert!(!is_direct_child("/a/b/c", "/a")); // 孙目录不算
        assert!(!is_direct_child("/a", "/a")); // 自身不算
        assert!(!is_direct_child("/x/y", "/a")); // 无关
    }

    #[test]
    fn recommendation_score_matches_weighted_breakdown() {
        let (raw, uniq, base) = setup("breakdown_fusion");
        let pwd = base.join("pwd");
        let target = base.join("target");
        let other = base.join("other");
        for dir in [&pwd, &target, &other] {
            fs::create_dir_all(dir).unwrap();
        }

        let now = 1_000_000i64;
        let mut rf = File::create(&raw).unwrap();
        writeln!(rf, "{}\t{}", now - 4000, pwd.display()).unwrap();
        writeln!(rf, "{}\t{}", now - 3900, target.display()).unwrap();
        writeln!(rf, "{}\t{}", now - 1000, other.display()).unwrap();
        fs::write(
            &uniq,
            format!(
                "{}\n{}\n{}\n",
                pwd.display(),
                other.display(),
                target.display()
            ),
        )
        .unwrap();

        let opt = RecommendOpt {
            raw,
            uniq,
            check_dir: true,
            pwd: Some(pwd.to_string_lossy().into_owned()),
            w_frecency: 0.25,
            w_recency: 0.35,
            w_context: 0.30,
            w_uniq: 0.10,
            ..RecommendOpt::default()
        };

        let out = recommend_with_now(&opt, now);
        assert!(!out.is_empty());
        for item in &out {
            let b = &item.breakdown;
            for value in [b.frecency_norm, b.recency_norm, b.context_norm, b.uniq_norm] {
                assert!(
                    (0.0..=1.0).contains(&value),
                    "breakdown out of range: {value}"
                );
            }
            let expected = opt.w_frecency * b.frecency_norm
                + opt.w_recency * b.recency_norm
                + opt.w_context * b.context_norm
                + opt.w_uniq * b.uniq_norm;
            assert!(
                (item.score - expected).abs() < 1e-12,
                "score changed: got {}, expected {} for {}",
                item.score,
                expected,
                item.path
            );
        }
    }

    #[test]
    fn include_missing_keeps_stale_paths_after_existing_dirs() {
        let (raw, uniq, base) = setup("include_missing");
        let exists = base.join("exists");
        let missing = base.join("missing");
        fs::create_dir_all(&exists).unwrap();

        let mut rf = File::create(&raw).unwrap();
        writeln!(rf, "1000\t{}", missing.display()).unwrap();
        writeln!(rf, "1001\t{}", exists.display()).unwrap();
        fs::write(
            &uniq,
            format!("{}\n{}\n", missing.display(), exists.display()),
        )
        .unwrap();

        let filtered = recommend_with_now(
            &RecommendOpt {
                raw: raw.clone(),
                uniq: uniq.clone(),
                check_dir: true,
                include_missing: false,
                ..RecommendOpt::default()
            },
            2000,
        );
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].path, exists.to_string_lossy());
        assert!(filtered[0].exists);

        let unfiltered = recommend_with_now(
            &RecommendOpt {
                raw,
                uniq,
                check_dir: true,
                include_missing: true,
                ..RecommendOpt::default()
            },
            2000,
        );
        assert_eq!(unfiltered.len(), 2);
        assert_eq!(unfiltered[0].path, exists.to_string_lossy());
        assert!(unfiltered[0].exists);
        assert_eq!(unfiltered[1].path, missing.to_string_lossy());
        assert!(!unfiltered[1].exists);
    }
}
