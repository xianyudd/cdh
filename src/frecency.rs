//! Frecency：频次 × 时效衰减 打分（单文件实现 + 测试）
//! - 批量评分：从事件时间戳向量计算分数
//! - 在线增量：常数时间更新 score
//! - 索引聚合：多目录 Top-N / 清理 / 容量上限
//! 约定：时间戳单位为秒；半衰期 > 0；未来事件按 1.0 处理（不放大）

use std::cmp::Ordering;
use std::collections::HashMap;

/// Frecency 模型：只有一个参数——半衰期（秒）
#[derive(Debug, Clone, Copy)]
pub struct Frecency {
    half_life_secs: f64,
}

impl Frecency {
    /// 创建模型（half_life_secs 必须 > 0）
    pub fn new(half_life_secs: f64) -> Self {
        assert!(
            half_life_secs.is_finite() && half_life_secs > 0.0,
            "half_life_secs must be > 0"
        );
        Self { half_life_secs }
    }

    /// 衰减权重：0.5 ^ (dt / half_life)
    #[inline]
    fn weight(&self, dt_secs: f64) -> f64 {
        if dt_secs <= 0.0 {
            1.0
        } else {
            0.5f64.powf(dt_secs / self.half_life_secs)
        }
    }

    /// 批量评分： score = Σ 0.5^((now - t_i)/half_life)
    pub fn batch_score(&self, mut events: Vec<i64>, now: i64) -> f64 {
        events.sort_unstable();
        let mut s = 0.0;
        for &t in &events {
            let dt = (now - t) as f64;
            s += self.weight(dt);
        }
        s
    }
}

/// 在线增量状态：维护“最近事件时刻”到 now 的聚合分数
/// 新访问（时间戳需非递减）：
///   decay = 0.5^((ts_new - last_ts)/half_life)
///   score = score * decay + 1
/// 查询 now：
///   score_now = score * 0.5^((now - last_ts)/half_life)
#[derive(Debug, Clone, Copy)]
pub struct FrecencyState {
    pub score: f64,
    pub last_ts: i64,
    initialized: bool,
}

impl FrecencyState {
    pub fn new() -> Self {
        Self {
            score: 0.0,
            last_ts: 0,
            initialized: false,
        }
    }

    /// 记录一次访问。
    /// - 正常（ts >= last_ts）：把已有分数衰减到 ts，再 +1，并前移 last_ts。
    /// - 乱序（ts < last_ts）：视作“与 last_ts 同时发生”，**只 +1，不衰减，也不回拨 last_ts**。
    pub fn observe(&mut self, ts: i64, model: &Frecency) {
        if !self.initialized {
            self.score = 1.0;
            self.last_ts = ts;
            self.initialized = true;
            return;
        }

        let dt_raw = ts - self.last_ts;
        if dt_raw >= 0 {
            // 正常顺序
            let decay = model.weight(dt_raw as f64);
            self.score = self.score * decay + 1.0;
            self.last_ts = ts;
        } else {
            // 乱序事件：不做时间衰减，不修改 last_ts
            self.score += 1.0;
        }
    }

    /// 在 now 的分数（只读）
    pub fn score_at(&self, now: i64, model: &Frecency) -> f64 {
        if !self.initialized {
            return 0.0;
        }
        let dt = (now - self.last_ts).max(0) as f64;
        self.score * model.weight(dt)
    }
}

/// 多目录聚合索引
pub struct FrecencyIndex {
    model: Frecency,
    map: HashMap<String, FrecencyState>,
}

impl FrecencyIndex {
    pub fn new(model: Frecency) -> Self {
        Self {
            model,
            map: HashMap::new(),
        }
    }

    /// 记录某目录一次访问
    pub fn record_visit<S: Into<String>>(&mut self, dir: S, ts: i64) {
        let entry = self
            .map
            .entry(dir.into())
            .or_insert_with(FrecencyState::new);
        entry.observe(ts, &self.model);
    }

    /// 某目录在 now 的分数
    pub fn score_at(&self, dir: &str, now: i64) -> f64 {
        self.map
            .get(dir)
            .map(|st| st.score_at(now, &self.model))
            .unwrap_or(0.0)
    }

    /// Top-N（按 now 的分数）
    pub fn top_n(&self, now: i64, n: usize) -> Vec<(String, f64)> {
        let mut v: Vec<(String, f64)> = self
            .map
            .iter()
            .map(|(k, st)| (k.clone(), st.score_at(now, &self.model)))
            .collect();
        v.sort_by(
            |a, b| match b.1.partial_cmp(&a.1).unwrap_or(Ordering::Equal) {
                Ordering::Equal => a.0.cmp(&b.0),
                other => other,
            },
        );
        if v.len() > n {
            v.truncate(n);
        }
        v
    }

    /// 清理：低于阈值的条目
    pub fn prune_below(&mut self, now: i64, threshold: f64) {
        self.map
            .retain(|_, st| st.score_at(now, &self.model) >= threshold);
    }

    /// 容量上限：保留 now 分数最高的前 max_entries
    pub fn cap_len(&mut self, now: i64, max_entries: usize) {
        if self.map.len() <= max_entries {
            return;
        }
        let mut pairs: Vec<(&String, f64)> = self
            .map
            .iter()
            .map(|(k, st)| (k, st.score_at(now, &self.model)))
            .collect();
        pairs.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(Ordering::Equal));

        let mut keep: HashMap<String, ()> = HashMap::with_capacity(max_entries);
        for (i, (k, _)) in pairs.into_iter().enumerate() {
            if i < max_entries {
                keep.insert(k.clone(), ());
            } else {
                break;
            }
        }
        self.map.retain(|k, _| keep.contains_key(k));
    }
}

/* ------------------------------ Tests ------------------------------ */

#[cfg(test)]
mod tests {
    use super::*;

    fn approx_eq(a: f64, b: f64, eps: f64) -> bool {
        (a - b).abs() <= eps
    }

    #[test]
    fn half_life_halves_score() {
        // 单次事件：过了一个半衰期，分数应为 1/2
        let hl = 10_000.0;
        let model = Frecency::new(hl);
        let t0 = 100_000i64;
        let now = t0 + hl as i64;

        let score = model.batch_score(vec![t0], now);
        assert!(approx_eq(score, 0.5, 1e-12), "score={}", score);
    }

    #[test]
    fn batch_vs_incremental_equivalence_sorted() {
        // 有序事件：批量与增量应一致
        let hl = 50_000.0;
        let model = Frecency::new(hl);
        let t0 = 1_000_000i64;
        let events = vec![t0, t0 + 100, t0 + 5_000, t0 + 60_000];
        let now = t0 + 70_000;

        let batch = model.batch_score(events.clone(), now);

        let mut st = FrecencyState::new();
        for ts in &events {
            st.observe(*ts, &model);
        }
        let incr = st.score_at(now, &model);

        assert!(
            approx_eq(batch, incr, 1e-12),
            "batch={} incr={}",
            batch,
            incr
        );
    }

    #[test]
    fn batch_handles_future_as_one() {
        // 未来事件按 1.0 处理（不放大）
        let model = Frecency::new(10_000.0);
        let now = 1_000_000i64;
        let future = now + 10_000;
        let past = now - 10_000;

        let s = model.batch_score(vec![future, past], now);
        // future:1.0, past:0.5
        assert!(approx_eq(s, 1.5, 1e-12), "score={}", s);
    }

    #[test]
    fn incremental_non_decreasing_ts() {
        // 倒序写入：第二次只 +1，不衰减，不回拨 last_ts
        let model = Frecency::new(10_000.0);
        let t1 = 1_000i64;
        let t0 = 500i64;

        let mut st = FrecencyState::new();
        st.observe(t1, &model); // score=1, last_ts=t1
        st.observe(t0, &model); // 倒序：score=2, last_ts 仍为 t1

        let s = st.score_at(t1, &model);
        assert!(approx_eq(s, 2.0, 1e-12), "score={}", s);
    }

    #[test]
    fn index_topn_and_prune() {
        let model = Frecency::new(60_000.0);
        let mut idx = FrecencyIndex::new(model);
        let base = 1_000_000i64;

        idx.record_visit("A", base);
        idx.record_visit("A", base + 10);
        idx.record_visit("A", base + 20);

        idx.record_visit("B", base + 5);

        idx.record_visit("C", base - 60_000); // 一整半衰期之前

        let now = base + 30;

        let top2 = idx.top_n(now, 2);
        assert_eq!(top2.len(), 2);
        assert_eq!(top2[0].0, "A");
        assert_eq!(top2[1].0, "B");

        idx.prune_below(now, 0.6); // C≈0.5 被清理
        assert!(idx.map.contains_key("A"));
        assert!(idx.map.contains_key("B"));
        assert!(!idx.map.contains_key("C"));
    }

    #[test]
    fn cap_len_keeps_best() {
        let model = Frecency::new(60_000.0);
        let mut idx = FrecencyIndex::new(model);
        let base = 2_000_000i64;

        idx.record_visit("A", base);
        idx.record_visit("B", base);
        idx.record_visit("B", base + 1);
        idx.record_visit("C", base);
        idx.record_visit("C", base + 1);
        idx.record_visit("C", base + 2);
        idx.record_visit("D", base);

        let now = base + 10;
        idx.cap_len(now, 2);

        let keys: Vec<_> = idx.top_n(now, 10).into_iter().map(|x| x.0).collect();
        assert_eq!(keys, vec!["C".to_string(), "B".to_string()]);
    }
}
