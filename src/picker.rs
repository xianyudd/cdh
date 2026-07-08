//! 交互式目录选择器（ratatui 重写 · 霓虹渐变现代风 + 平滑过渡动画）
//!
//! 设计要点：
//! - 视觉：深色卡片 + 青→紫渐变高亮条；右侧彩色分数条反映 frecency 融合分。
//! - 搜索：默认即可输入，nucleo 模糊匹配（fzf 风格），命中字符高亮，按匹配分排序。
//! - 交互：↑/↓ 移动，PageUp/Down 翻页，Enter/Tab 选中，Esc 清查询/退出，鼠标单击/双击/滚轮。
//! - 动画：高亮条位置缓动 + 分数条增长 + 淡入；仅在有动画时以 ~60fps tick，空闲回到阻塞事件。
//!
//! 兼容：非交互（无 TTY）直接返回第一项；`CDH_COLOR=0` 关色，`CDH_MOUSE=0` 关鼠标，`CDH_ANIM=0` 关动画。

use std::collections::{HashMap, VecDeque};
use std::env;
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
        MouseButton, MouseEventKind,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use nucleo_matcher::{
    pattern::{CaseMatching, Normalization, Pattern},
    Config, Matcher, Utf32Str,
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, BorderType, Borders, Clear, List, ListItem, ListState, Padding, Paragraph,
        Scrollbar, ScrollbarOrientation, ScrollbarState,
    },
    Frame, Terminal,
};

use crate::recommend::Recommendation;
use crate::{history, AppContext};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

// ---------------- 常量 ----------------
const WATCHDOG_TIMEOUT: Duration = Duration::from_secs(15);
const FRAME_MS: u64 = 16; // ~60fps 动画帧
const ANIM_SPEED: f32 = 0.28; // 缓动系数（每帧向目标靠拢的比例）
const ANIM_EPS: f32 = 0.004; // 动画收敛阈值
const SCORE_BAR_CELLS: usize = 8; // 分数条格子数
const DOUBLE_CLICK_MS: u128 = 300;
const MIN_HEIGHT: u16 = 6;
const PREVIEW_ENTRY_LIMIT: usize = 24;
const PREVIEW_CACHE_LIMIT: usize = 50;
const PREVIEW_DEBOUNCE: Duration = Duration::from_millis(100);
const PREVIEW_MIN_WIDTH: u16 = 70;

// ---------------- 环境开关 ----------------
fn env_flag(key: &str, default: bool) -> bool {
    match env::var(key) {
        Ok(v) => v == "1" || v.eq_ignore_ascii_case("true"),
        Err(_) => default,
    }
}
fn color_enabled() -> bool {
    env_flag("CDH_COLOR", true)
}
fn mouse_enabled() -> bool {
    env_flag("CDH_MOUSE", true)
}
fn anim_enabled() -> bool {
    color_enabled() && env_flag("CDH_ANIM", true)
}
fn preview_enabled() -> bool {
    env_flag("CDH_PREVIEW", true)
}

// ---------------- 预览数据 ----------------
#[derive(Debug, Clone, PartialEq, Eq)]
struct GitInfo {
    branch: String,
    dirty: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PreviewEntry {
    name: String,
    is_dir: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PreviewData {
    git: Option<GitInfo>,
    entries: Vec<PreviewEntry>,
    has_more_entries: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PreviewOutcome {
    Data(PreviewData),
    Error(String),
    Missing,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PreviewRequest {
    path: String,
    generation: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PreviewResponse {
    path: String,
    generation: u64,
    outcome: PreviewOutcome,
}

struct PreviewWorker {
    requests: mpsc::Sender<PreviewRequest>,
    responses: mpsc::Receiver<PreviewResponse>,
}

fn start_preview_worker() -> PreviewWorker {
    let (request_tx, request_rx) = mpsc::channel::<PreviewRequest>();
    let (response_tx, response_rx) = mpsc::channel::<PreviewResponse>();
    thread::spawn(move || {
        while let Ok(req) = request_rx.recv() {
            let outcome = load_preview(&req.path);
            let response = PreviewResponse {
                path: req.path,
                generation: req.generation,
                outcome,
            };
            if response_tx.send(response).is_err() {
                break;
            }
        }
    });
    PreviewWorker {
        requests: request_tx,
        responses: response_rx,
    }
}

fn load_preview(path: &str) -> PreviewOutcome {
    let path_ref = Path::new(path);
    if !path_ref.is_dir() {
        return PreviewOutcome::Missing;
    }

    match read_preview_entries(path_ref) {
        Ok((entries, has_more_entries)) => PreviewOutcome::Data(PreviewData {
            git: read_git_info(path_ref),
            entries,
            has_more_entries,
        }),
        Err(err) => PreviewOutcome::Error(preview_error_message(&err)),
    }
}

fn read_preview_entries(path: &Path) -> io::Result<(Vec<PreviewEntry>, bool)> {
    let mut entries = Vec::with_capacity(PREVIEW_ENTRY_LIMIT);
    let mut has_more_entries = false;
    for entry_result in fs::read_dir(path)?.take(PREVIEW_ENTRY_LIMIT + 1) {
        let entry = entry_result?;
        if entries.len() == PREVIEW_ENTRY_LIMIT {
            has_more_entries = true;
            break;
        }
        let file_type = entry.file_type()?;
        entries.push(PreviewEntry {
            name: entry.file_name().to_string_lossy().into_owned(),
            is_dir: file_type.is_dir(),
        });
    }
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            .then_with(|| a.name.cmp(&b.name))
    });
    Ok((entries, has_more_entries))
}

fn read_git_info(path: &Path) -> Option<GitInfo> {
    let git_dir = find_git_dir(path)?;
    let head = fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let branch = parse_git_head_branch(&head)?;
    Some(GitInfo {
        branch,
        dirty: None,
    })
}

fn find_git_dir(start: &Path) -> Option<PathBuf> {
    for ancestor in start.ancestors() {
        let git = ancestor.join(".git");
        if git.is_dir() {
            return Some(git);
        }
    }
    None
}

fn parse_git_head_branch(head: &str) -> Option<String> {
    let head = head.trim();
    head.strip_prefix("ref: refs/heads/")
        .map(|branch| branch.to_string())
}

fn preview_error_message(err: &io::Error) -> String {
    match err.kind() {
        io::ErrorKind::PermissionDenied => "权限不足".to_string(),
        io::ErrorKind::NotFound => "目录已不存在".to_string(),
        _ => err.to_string(),
    }
}

// ---------------- 霓虹主题 ----------------
/// 深色霓虹配色：青→紫渐变。`color_enabled()==false` 时全部退化为默认色。
struct Theme {
    on: bool,
}
impl Theme {
    fn new() -> Self {
        Self {
            on: color_enabled(),
        }
    }
    fn c(&self, r: u8, g: u8, b: u8) -> Color {
        if self.on {
            Color::Rgb(r, g, b)
        } else {
            Color::Reset
        }
    }
    fn border(&self) -> Style {
        Style::default().fg(self.c(0x3b, 0x4a, 0x6b))
    }
    fn title(&self) -> Style {
        Style::default()
            .fg(self.c(0x8b, 0xe9, 0xfd))
            .add_modifier(Modifier::BOLD)
    }
    fn dim(&self) -> Style {
        Style::default().fg(self.c(0x6a, 0x74, 0x8c))
    }
    fn path(&self) -> Style {
        Style::default().fg(self.c(0xc8, 0xd3, 0xf0))
    }
    fn home_tilde(&self) -> Style {
        Style::default().fg(self.c(0x7c, 0x83, 0xa8))
    }
    /// 选中行前景与背景。
    fn sel_fg(&self) -> Style {
        Style::default()
            .fg(self.c(0xf5, 0xf7, 0xff))
            .add_modifier(if self.on {
                Modifier::BOLD
            } else {
                Modifier::REVERSED
            })
    }
    fn sel_bg(&self) -> Color {
        self.c(0x27, 0x33, 0x5c)
    }
    fn match_hl(&self) -> Style {
        Style::default()
            .fg(self.c(0x50, 0xfa, 0x7b))
            .add_modifier(Modifier::BOLD)
    }
    fn accent(&self) -> Color {
        self.c(0xbd, 0x93, 0xf9)
    }
    fn score_frecency(&self) -> Color {
        self.c(0x50, 0xfa, 0xdc)
    }
    fn score_recency(&self) -> Color {
        self.c(0x66, 0x99, 0xff)
    }
    fn score_context(&self) -> Color {
        self.c(0xbd, 0x93, 0xf9)
    }
    fn score_uniq(&self) -> Color {
        self.c(0x4a, 0x55, 0x78)
    }
}

// ---------------- 对外 API ----------------
/// 交互式选择一个推荐目录。
///
/// - 非交互（stderr/stdin 非 TTY）：保持旧契约，直接返回第一项。
/// - 交互：进入全屏 TUI，返回用户选择的路径（`None` 表示取消/超时）。
pub fn pick(items: &[Recommendation]) -> io::Result<Option<String>> {
    if items.is_empty() {
        return Ok(None);
    }
    if !io::stderr().is_terminal() || !io::stdin().is_terminal() {
        return Ok(items.first().map(|r| r.path.clone()));
    }
    run_ui(items, None)
}

pub fn pick_with_history(ctx: &AppContext, items: &[Recommendation]) -> io::Result<Option<String>> {
    if items.is_empty() {
        return Ok(None);
    }
    if !io::stderr().is_terminal() || !io::stdin().is_terminal() {
        return Ok(items.first().map(|r| r.path.clone()));
    }
    run_ui(items, Some(ctx))
}

// ---------------- 终端守卫 ----------------
struct TermGuard {
    mouse: bool,
}
impl TermGuard {
    fn enter(mouse: bool) -> io::Result<Self> {
        enable_raw_mode()?;
        let mut err = io::stderr();
        execute!(err, EnterAlternateScreen)?;
        if mouse {
            execute!(err, EnableMouseCapture)?;
        }
        Ok(Self { mouse })
    }
}
impl Drop for TermGuard {
    fn drop(&mut self) {
        let mut err = io::stderr();
        if self.mouse {
            let _ = execute!(err, DisableMouseCapture);
        }
        let _ = execute!(err, LeaveAlternateScreen);
        let _ = disable_raw_mode();
    }
}

// ---------------- 候选项（预处理）----------------
/// 预处理后的候选：缓存 `~` 缩写显示串与小写检索串。
struct Candidate {
    /// 返回给调用方的原始绝对路径。
    raw: String,
    /// 展示串（$HOME → ~）。
    display: String,
    /// 归一化分数（0~1）。
    score: f32,
    /// 归一化子信号：frecency / recency / context / uniq。
    breakdown: [f32; 4],
    /// 目录当前是否存在。
    exists: bool,
}

fn build_candidates(items: &[Recommendation]) -> Vec<Candidate> {
    let home = env::var("HOME").ok().filter(|h| !h.is_empty());
    items
        .iter()
        .map(|r| {
            let display = match &home {
                Some(h) if r.path == *h => "~".to_string(),
                Some(h) if r.path.starts_with(&format!("{h}/")) => {
                    format!("~{}", &r.path[h.len()..])
                }
                _ => r.path.clone(),
            };
            Candidate {
                raw: r.path.clone(),
                display,
                score: r.score.clamp(0.0, 1.0) as f32,
                breakdown: [
                    r.breakdown.frecency_norm.clamp(0.0, 1.0) as f32,
                    r.breakdown.recency_norm.clamp(0.0, 1.0) as f32,
                    r.breakdown.context_norm.clamp(0.0, 1.0) as f32,
                    r.breakdown.uniq_norm.clamp(0.0, 1.0) as f32,
                ],
                exists: r.exists,
            }
        })
        .collect()
}

// ---------------- 过滤 / 匹配 ----------------
/// 单条匹配结果：候选下标 + 匹配分 + 在 display 上的命中字符索引。
struct Match {
    idx: usize,
    hl: Vec<u32>,
}

struct Filter {
    matcher: Matcher,
    buf: Vec<char>,
}
impl Filter {
    fn new() -> Self {
        Self {
            matcher: Matcher::new(Config::DEFAULT.match_paths()),
            buf: Vec::new(),
        }
    }

    /// 依据 query 过滤候选，返回排序后的匹配列表。
    /// - 空 query：保持原推荐顺序（即 frecency 融合分降序）。
    /// - 非空：nucleo 模糊匹配，先按匹配分、再按候选分排序。
    fn run(&mut self, cands: &[Candidate], query: &str) -> Vec<Match> {
        let q = query.trim();
        if q.is_empty() {
            return (0..cands.len())
                .map(|idx| Match {
                    idx,
                    hl: Vec::new(),
                })
                .collect();
        }
        let pattern = Pattern::parse(q, CaseMatching::Ignore, Normalization::Smart);
        let mut scored_existing: Vec<(u32, usize, Vec<u32>)> = Vec::new();
        let mut stale_matches: Vec<(usize, Vec<u32>)> = Vec::new();
        for (idx, c) in cands.iter().enumerate() {
            let mut hbuf = Vec::new();
            let haystack = Utf32Str::new(&c.display, &mut self.buf);
            let mut indices = Vec::new();
            if let Some(score) = pattern.score(haystack, &mut self.matcher) {
                // 拿命中索引用于高亮（单独调用，pattern 内部已按 atom 组合）。
                let hay2 = Utf32Str::new(&c.display, &mut hbuf);
                indices.clear();
                let _ = self.matcher.fuzzy_indices(
                    hay2,
                    Utf32Str::new(q, &mut Vec::new()),
                    &mut indices,
                );
                indices.sort_unstable();
                indices.dedup();
                if c.exists {
                    scored_existing.push((score, idx, indices));
                } else {
                    stale_matches.push((idx, indices));
                }
            }
        }
        // 匹配分降序；同分时按候选自身分数降序，保证 frecency 高的靠前。
        scored_existing.sort_by(|a, b| {
            b.0.cmp(&a.0).then_with(|| {
                cands[b.1]
                    .score
                    .partial_cmp(&cands[a.1].score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
        });
        scored_existing
            .into_iter()
            .map(|(_, idx, hl)| Match { idx, hl })
            .chain(stale_matches.into_iter().map(|(idx, hl)| Match { idx, hl }))
            .collect()
    }
}

// ---------------- 应用状态 ----------------
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Normal,
    Help,
    ConfirmDelete { candidate_idx: usize },
}

struct App {
    cands: Vec<Candidate>,
    filter: Filter,
    query: String,
    matches: Vec<Match>,
    selected: usize, // matches 内下标
    offset: usize,   // 列表滚动偏移
    // 动画状态
    anim_cursor: f32,           // 平滑高亮行（浮点）
    anim_scores: Vec<[f32; 4]>, // 每个原始候选当前子信号填充（0~1）
    fade: f32,                  // 打开淡入（0→1）
    mode: Mode,
    preview_visible: bool,
    preview_worker: Option<PreviewWorker>,
    preview_cache: HashMap<String, PreviewOutcome>,
    preview_cache_order: VecDeque<String>,
    preview_generation: u64,
    preview_pending: Option<(String, Instant)>,
    preview_loading: Option<String>,
    preview_current: Option<(String, PreviewOutcome)>,
    preview_selected_path: Option<String>,
    last_click: Option<(usize, Instant)>,
    /// 上一帧实际渲染的列表区域（供鼠标命中测试对齐真实布局）。
    last_list_area: std::cell::Cell<Rect>,
}
impl App {
    fn new(cands: Vec<Candidate>) -> Self {
        let preview_visible = preview_enabled();
        let preview_worker = if preview_visible {
            Some(start_preview_worker())
        } else {
            None
        };
        Self::with_preview_worker(cands, preview_worker, preview_visible)
    }

    fn with_preview_worker(
        cands: Vec<Candidate>,
        preview_worker: Option<PreviewWorker>,
        preview_visible: bool,
    ) -> Self {
        let mut filter = Filter::new();
        let matches = filter.run(&cands, "");
        let n = cands.len();
        Self {
            cands,
            filter,
            query: String::new(),
            matches,
            selected: 0,
            offset: 0,
            anim_cursor: 0.0,
            anim_scores: vec![[0.0; 4]; n],
            fade: if anim_enabled() { 0.0 } else { 1.0 },
            mode: Mode::Normal,
            preview_visible,
            preview_worker,
            preview_cache: HashMap::new(),
            preview_cache_order: VecDeque::new(),
            preview_generation: 0,
            preview_pending: None,
            preview_loading: None,
            preview_current: None,
            preview_selected_path: None,
            last_click: None,
            last_list_area: std::cell::Cell::new(Rect::new(0, 0, 0, 0)),
        }
    }

    /// 依据可视高度把选中项夹进视口，更新滚动偏移。draw 前统一调用。
    fn sync_scroll(&mut self, height: usize) {
        if height == 0 || self.matches.is_empty() {
            self.offset = 0;
            return;
        }
        if self.selected < self.offset {
            self.offset = self.selected;
        } else if self.selected >= self.offset + height {
            self.offset = self.selected + 1 - height;
        }
        let max_off = self.matches.len().saturating_sub(height);
        self.offset = self.offset.min(max_off);
    }

    fn recompute(&mut self) {
        self.matches = self.filter.run(&self.cands, &self.query);
        self.selected = 0;
        self.offset = 0;
        self.anim_cursor = 0.0;
    }

    fn selected_raw(&self) -> Option<String> {
        self.matches
            .get(self.selected)
            .map(|m| self.cands[m.idx].raw.clone())
    }

    fn selected_candidate_idx(&self) -> Option<usize> {
        self.matches.get(self.selected).map(|m| m.idx)
    }

    fn selected_candidate(&self) -> Option<&Candidate> {
        self.selected_candidate_idx().map(|idx| &self.cands[idx])
    }

    fn remove_candidate(&mut self, idx: usize) {
        if idx >= self.cands.len() {
            return;
        }
        self.cands.remove(idx);
        self.anim_scores.remove(idx);
        self.recompute();
        if !self.matches.is_empty() {
            self.selected = self.selected.min(self.matches.len() - 1);
        }
        self.preview_selected_path = None;
    }

    fn move_by(&mut self, delta: isize) {
        if self.matches.is_empty() {
            return;
        }
        let n = self.matches.len() as isize;
        let cur = self.selected as isize;
        self.selected = (cur + delta).rem_euclid(n) as usize;
    }
    fn move_to(&mut self, idx: usize) {
        if idx < self.matches.len() {
            self.selected = idx;
        }
    }

    /// 推进一帧动画，返回是否仍需继续（有未收敛的动画）。
    fn tick_anim(&mut self) -> bool {
        if !anim_enabled() {
            // 无动画：分数条直接到位、无淡入。
            for m in &self.matches {
                self.anim_scores[m.idx] = self.cands[m.idx].breakdown;
            }
            self.anim_cursor = self.selected as f32;
            return false;
        }
        let mut busy = false;

        // 淡入
        if self.fade < 1.0 {
            self.fade = (self.fade + ANIM_SPEED).min(1.0);
            if (1.0 - self.fade).abs() > ANIM_EPS {
                busy = true;
            }
        }

        // 高亮行缓动
        let target = self.selected as f32;
        if (self.anim_cursor - target).abs() > ANIM_EPS {
            self.anim_cursor += (target - self.anim_cursor) * ANIM_SPEED;
            busy = true;
        } else {
            self.anim_cursor = target;
        }

        // 分数条增长（仅当前可见匹配项参与，避免整表抖动）
        for m in &self.matches {
            let tgt = self.cands[m.idx].breakdown.map(|value| value * self.fade);
            let cur = &mut self.anim_scores[m.idx];
            for (current, target) in cur.iter_mut().zip(tgt) {
                if (*current - target).abs() > ANIM_EPS {
                    *current += (target - *current) * ANIM_SPEED;
                    busy = true;
                } else {
                    *current = target;
                }
            }
        }
        busy
    }

    fn update_preview(&mut self, now: Instant) {
        if !self.preview_visible {
            return;
        }
        self.poll_preview_results();
        self.track_preview_selection(now);
        self.maybe_send_preview_request(now);
    }

    fn toggle_preview(&mut self) {
        self.preview_visible = !self.preview_visible;
        if self.preview_visible && self.preview_worker.is_none() {
            self.preview_worker = Some(start_preview_worker());
        }
        self.preview_selected_path = None;
        self.preview_pending = None;
        self.preview_loading = None;
        self.preview_current = None;
    }

    fn track_preview_selection(&mut self, now: Instant) {
        let selected = self
            .selected_candidate()
            .map(|cand| (cand.raw.clone(), cand.exists));
        let selected_path = selected.as_ref().map(|(path, _)| path.clone());
        if self.preview_selected_path == selected_path {
            return;
        }
        self.preview_selected_path = selected_path.clone();
        self.preview_pending = None;
        self.preview_loading = None;

        let Some((path, exists)) = selected else {
            self.preview_current = None;
            return;
        };

        if !exists {
            self.preview_current = Some((path, PreviewOutcome::Missing));
            return;
        }

        if let Some(cached) = self.preview_cache.get(&path).cloned() {
            self.preview_current = Some((path, cached));
        } else {
            self.preview_current = None;
            self.preview_pending = Some((path, now));
        }
    }

    fn maybe_send_preview_request(&mut self, now: Instant) {
        let Some((path, changed_at)) = self.preview_pending.clone() else {
            return;
        };
        if now.duration_since(changed_at) < PREVIEW_DEBOUNCE {
            return;
        }
        self.preview_pending = None;
        if self.preview_cache.contains_key(&path) {
            return;
        }
        let Some(worker) = &self.preview_worker else {
            self.preview_current =
                Some((path, PreviewOutcome::Error("预览功能不可用".to_string())));
            return;
        };

        self.preview_generation = self.preview_generation.saturating_add(1);
        let generation = self.preview_generation;
        let request = PreviewRequest {
            path: path.clone(),
            generation,
        };
        match worker.requests.send(request) {
            Ok(()) => self.preview_loading = Some(path),
            Err(_) => {
                self.preview_worker = None;
                self.preview_loading = None;
                self.preview_current =
                    Some((path, PreviewOutcome::Error("预览功能不可用".to_string())));
            }
        }
    }

    fn poll_preview_results(&mut self) {
        let mut responses = Vec::new();
        if let Some(worker) = &self.preview_worker {
            loop {
                match worker.responses.try_recv() {
                    Ok(response) => responses.push(response),
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        self.preview_worker = None;
                        break;
                    }
                }
            }
        }

        for response in responses {
            self.accept_preview_response(response);
        }
    }

    fn accept_preview_response(&mut self, response: PreviewResponse) {
        if response.generation != self.preview_generation {
            return;
        }
        if self.preview_selected_path.as_deref() != Some(response.path.as_str()) {
            return;
        }
        self.insert_preview_cache(response.path.clone(), response.outcome.clone());
        self.preview_loading = None;
        self.preview_current = Some((response.path, response.outcome));
    }

    fn insert_preview_cache(&mut self, path: String, outcome: PreviewOutcome) {
        if !self.preview_cache.contains_key(&path) {
            self.preview_cache_order.push_back(path.clone());
        }
        self.preview_cache.insert(path, outcome);
        while self.preview_cache_order.len() > PREVIEW_CACHE_LIMIT {
            if let Some(old) = self.preview_cache_order.pop_front() {
                self.preview_cache.remove(&old);
            }
        }
    }
}

// ---------------- 主循环 ----------------
fn run_ui(items: &[Recommendation], ctx: Option<&AppContext>) -> io::Result<Option<String>> {
    let mouse = mouse_enabled();
    let _guard = TermGuard::enter(mouse)?;
    let backend = CrosstermBackend::new(io::stderr());
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new(build_candidates(items));
    let theme = Theme::new();

    let mut idle_since = Instant::now();
    let mut seen_key = false;

    loop {
        // 看门狗：长时间无操作自动退出（保持旧行为）。
        if !seen_key && idle_since.elapsed() > WATCHDOG_TIMEOUT {
            return Ok(None);
        }

        let busy = app.tick_anim();
        app.update_preview(Instant::now());
        terminal.draw(|f| draw(f, &app, &theme))?;
        // draw 记录了真实列表高度；据此收敛滚动偏移，供下一帧与鼠标命中共用。
        app.sync_scroll(app.last_list_area.get().height as usize);

        // 有动画时用短超时驱动帧循环；空闲时用较长超时以省电。
        let poll_timeout = if busy {
            Duration::from_millis(FRAME_MS)
        } else {
            Duration::from_millis(200)
        };

        if !event::poll(poll_timeout)? {
            continue;
        }

        match event::read()? {
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                seen_key = true;
                idle_since = Instant::now();
                if let Some(result) = handle_key(&mut app, key.code, key.modifiers, ctx) {
                    return Ok(result);
                }
            }
            Event::Mouse(me) if mouse => {
                seen_key = true;
                idle_since = Instant::now();
                if app.mode != Mode::Normal {
                    continue;
                }
                if let Some(result) = handle_mouse(&mut app, me)? {
                    return Ok(result);
                }
            }
            _ => {}
        }
    }
}

/// 处理按键。返回 `Some(_)` 表示应当退出主循环并返回该值。
fn handle_key(
    app: &mut App,
    code: KeyCode,
    mods: KeyModifiers,
    ctx: Option<&AppContext>,
) -> Option<Option<String>> {
    match app.mode {
        Mode::Normal => handle_key_normal(app, code, mods),
        Mode::Help => handle_key_help(app),
        Mode::ConfirmDelete { candidate_idx } => {
            handle_key_confirm_delete(app, code, mods, ctx, candidate_idx)
        }
    }
}

fn handle_key_help(app: &mut App) -> Option<Option<String>> {
    app.mode = Mode::Normal;
    None
}

fn handle_key_confirm_delete(
    app: &mut App,
    code: KeyCode,
    mods: KeyModifiers,
    ctx: Option<&AppContext>,
    candidate_idx: usize,
) -> Option<Option<String>> {
    let ctrl = mods.contains(KeyModifiers::CONTROL);
    app.mode = Mode::Normal;
    if matches!(code, KeyCode::Char('d')) && ctrl {
        match ctx {
            Some(ctx) => {
                let Some(raw) = app.cands.get(candidate_idx).map(|cand| cand.raw.clone()) else {
                    beep();
                    return None;
                };
                match history::remove_path(ctx, &raw) {
                    Ok(()) => app.remove_candidate(candidate_idx),
                    Err(_) => beep(),
                }
            }
            None => beep(),
        }
    }
    None
}

fn handle_key_normal(app: &mut App, code: KeyCode, mods: KeyModifiers) -> Option<Option<String>> {
    let ctrl = mods.contains(KeyModifiers::CONTROL);
    match code {
        KeyCode::Char('c') if ctrl => return Some(None),
        KeyCode::Char('g') if ctrl => return Some(None),
        KeyCode::Char('d') if ctrl => {
            let Some(idx) = app.selected_candidate_idx() else {
                beep();
                return None;
            };
            if app.cands[idx].exists {
                beep();
            } else {
                app.mode = Mode::ConfirmDelete { candidate_idx: idx };
            }
        }
        KeyCode::Enter | KeyCode::Tab => {
            if app.matches.is_empty() {
                beep();
                return None;
            }
            if app
                .selected_candidate()
                .map(|cand| !cand.exists)
                .unwrap_or(false)
            {
                beep();
                return None;
            }
            return Some(app.selected_raw());
        }
        KeyCode::Esc => {
            if app.query.is_empty() {
                return Some(None);
            }
            app.query.clear();
            app.recompute();
        }
        KeyCode::Up => app.move_by(-1),
        KeyCode::Down => app.move_by(1),
        KeyCode::Char('p') if ctrl => app.move_by(-1),
        KeyCode::Char('n') if ctrl => app.move_by(1),
        KeyCode::PageUp => app.move_by(-10),
        KeyCode::PageDown => app.move_by(10),
        KeyCode::Home => app.selected = 0,
        KeyCode::End => app.selected = app.matches.len().saturating_sub(1),
        KeyCode::Backspace => {
            app.query.pop();
            app.recompute();
        }
        KeyCode::F(1) => app.mode = Mode::Help,
        KeyCode::F(2) => app.toggle_preview(),
        KeyCode::Char(c) if !ctrl && !c.is_control() => {
            app.query.push(c);
            app.recompute();
        }
        _ => {}
    }
    None
}

fn handle_mouse(app: &mut App, me: event::MouseEvent) -> io::Result<Option<Option<String>>> {
    match me.kind {
        MouseEventKind::ScrollUp => app.move_by(-1),
        MouseEventKind::ScrollDown => app.move_by(1),
        MouseEventKind::Down(MouseButton::Left) => {
            // 使用上一帧 draw 记录的真实列表区域，避免手工推导与实际布局漂移。
            let list_area = app.last_list_area.get();
            if list_area.height > 0
                && me.row >= list_area.y
                && me.row < list_area.y + list_area.height
                && me.column >= list_area.x
                && me.column < list_area.x + list_area.width
            {
                let row = (me.row - list_area.y) as usize + app.offset;
                if row < app.matches.len() {
                    let now = Instant::now();
                    let is_double = app
                        .last_click
                        .map(|(r, t)| {
                            r == row && now.duration_since(t).as_millis() <= DOUBLE_CLICK_MS
                        })
                        .unwrap_or(false);
                    app.move_to(row);
                    if is_double {
                        return Ok(Some(app.selected_raw()));
                    }
                    app.last_click = Some((row, now));
                }
            }
        }
        _ => {}
    }
    Ok(None)
}

// ---------------- 绘制 ----------------
fn draw(f: &mut Frame, app: &App, theme: &Theme) {
    let full = f.area();
    if full.height < MIN_HEIGHT {
        let msg = Paragraph::new("终端太小（至少需要 6 行）").style(theme.dim());
        f.render_widget(msg, full);
        return;
    }

    // 外层：标题栏 + 内容 + 输入行
    let title = Line::from(vec![
        Span::styled(" cdh ", theme.title()),
        Span::styled("• 目录跳转 ", theme.dim()),
    ]);
    let count = if app.query.is_empty() {
        format!(" {} 个目录 ", app.cands.len())
    } else {
        format!(" {}/{} ", app.matches.len(), app.cands.len())
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(theme.border())
        .title(title)
        .title_top(Line::from(Span::styled(count, theme.dim())).right_aligned())
        .padding(Padding::new(1, 1, 0, 0));
    let inner = block.inner(full);
    f.render_widget(block, full);

    // 内容区再切成 列表/预览 + 输入行
    let chunks = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(inner);
    let content_area = chunks[0];
    let input_area = chunks[1];

    f.render_widget(Clear, content_area);
    if preview_layout_enabled(app, full.width) {
        let columns = Layout::horizontal([Constraint::Percentage(60), Constraint::Percentage(40)])
            .split(content_area);
        render_list(f, app, theme, columns[0]);
        render_preview(f, app, theme, columns[1]);
    } else {
        render_list(f, app, theme, content_area);
    }
    render_input(f, app, theme, input_area);

    match app.mode {
        Mode::Normal => {}
        Mode::Help => render_help(f, theme, full),
        Mode::ConfirmDelete { candidate_idx } => {
            render_confirm_delete(f, app, theme, full, candidate_idx);
        }
    }
}

fn preview_layout_enabled(app: &App, width: u16) -> bool {
    app.preview_visible && width >= PREVIEW_MIN_WIDTH
}

fn render_list(f: &mut Frame, app: &App, theme: &Theme, area: Rect) {
    // 记录真实列表区域，供鼠标命中测试对齐。
    app.last_list_area.set(area);

    if app.matches.is_empty() {
        let empty = Paragraph::new(Line::from(vec![
            Span::styled("  ✗ ", Style::default().fg(theme.c(0xff, 0x55, 0x55))),
            Span::styled("无匹配结果", theme.dim()),
        ]));
        f.render_widget(empty, area);
        return;
    }

    let height = area.height as usize;
    let selected = app.selected;
    // offset 已由主循环 sync_scroll 统一维护；这里只做防御性夹取。
    let offset = app
        .offset
        .min(app.matches.len().saturating_sub(height.max(1)));

    // 三段固定布局，避免长路径把分数条挤出行外触发换行：
    //   marker(2) + 空格 + 路径列(path_w，填充/截断) + 空格 + 分数列(8)
    // 分数列 = 5 格条 + " NN"(3) = 8 显示列。
    const MARKER_W: usize = 2;
    const GAP_W: usize = 1;
    const SCORE_W: usize = SCORE_BAR_CELLS + 3;
    let total_w = area.width as usize;
    // 若终端过窄，优先保证不溢出：路径列至少 4 列。
    let reserved = MARKER_W + GAP_W + SCORE_W;
    let path_w = total_w.saturating_sub(reserved).max(4);
    // 分数条是否有空间显示（极窄终端下隐藏，只留路径）。
    let show_score = total_w >= reserved + 4;
    let path_w = if show_score {
        path_w
    } else {
        total_w.saturating_sub(MARKER_W).max(4)
    };

    let cursor_row = app.anim_cursor - offset as f32; // 浮点行（动画）

    let mut rows: Vec<ListItem> = Vec::with_capacity(height.min(app.matches.len()));
    for (vis, m) in app.matches.iter().enumerate().skip(offset).take(height) {
        let cand = &app.cands[m.idx];
        let is_sel = vis == selected;
        let row_idx = vis - offset;

        // 选中行背景：动画时对最接近 cursor_row 的行加深，制造缓动“滑块”观感。
        let glow = if anim_enabled() {
            let d = (row_idx as f32 - cursor_row).abs();
            (1.0 - d).clamp(0.0, 1.0)
        } else if is_sel {
            1.0
        } else {
            0.0
        };

        let mut spans: Vec<Span> = Vec::new();

        // 指示符（用确定 1 显示列的 Narrow 字符，避免 Ambiguous 宽度在 CJK 终端占 2 列导致溢出）
        let marker = if is_sel { "❯ " } else { "  " };
        let marker_style = if cand.exists {
            Style::default()
                .fg(theme.accent())
                .add_modifier(Modifier::BOLD)
        } else {
            theme.dim()
        };
        spans.push(Span::styled(marker, marker_style));

        // 路径列（~ 着色 + 命中高亮 + 中截省略 + 右填充到 path_w）
        spans.extend(path_spans(cand, &m.hl, path_w, theme, is_sel));

        // 右侧分数条
        if show_score {
            spans.push(Span::raw(" "));
            if cand.exists {
                let filled = app.anim_scores[m.idx];
                spans.extend(score_bar_spans(filled, cand.score, theme));
            } else {
                spans.extend(stale_badge_spans(theme));
            }
        }

        let mut line = Line::from(spans);
        if glow > 0.0 {
            // 用背景色制造高亮条；非彩色终端下退化为反显（见 sel_fg）。
            let bg = theme.sel_bg();
            line = line.style(Style::default().bg(bg));
        }
        rows.push(ListItem::new(line));
    }

    let mut state = ListState::default();
    state.select(Some(selected.saturating_sub(offset)));
    let list = List::new(rows);
    f.render_stateful_widget(list, area, &mut state);

    // 滚动条（仅当溢出时）
    if app.matches.len() > height {
        let mut sb_state = ScrollbarState::new(app.matches.len()).position(selected);
        let sb = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None)
            .track_symbol(Some("│"))
            .thumb_symbol("┃")
            .style(theme.dim())
            .thumb_style(Style::default().fg(theme.accent()));
        f.render_stateful_widget(sb, area, &mut sb_state);
    }
}

/// 生成路径列 span：$HOME 前缀 `~` 淡色，命中字符高亮，按显示宽度中截，
/// 并在末尾右填充空格到固定 `col_w` 显示列，使右侧分数条对齐。
fn path_spans<'a>(
    cand: &'a Candidate,
    hl: &[u32],
    col_w: usize,
    theme: &Theme,
    is_sel: bool,
) -> Vec<Span<'a>> {
    let base = if is_sel { theme.sel_fg() } else { theme.path() };
    let base = if cand.exists {
        base
    } else {
        theme.dim().add_modifier(Modifier::CROSSED_OUT)
    };
    let disp = trim_middle(&cand.display, col_w);
    let disp_w = UnicodeWidthStr::width(disp.as_str());
    let pad = col_w.saturating_sub(disp_w);
    let truncated = disp != cand.display;

    let mut spans = Vec::new();

    // 命中索引针对完整 display 的字符序号；截断后无法对齐，故仅未截断时逐字符高亮。
    if !hl.is_empty() && !truncated {
        let hl_set: std::collections::HashSet<u32> = hl.iter().copied().collect();
        for (i, ch) in cand.display.chars().enumerate() {
            let style = if !cand.exists {
                base
            } else if hl_set.contains(&(i as u32)) {
                theme.match_hl()
            } else if i == 0 && ch == '~' {
                theme.home_tilde()
            } else {
                base
            };
            spans.push(Span::styled(ch.to_string(), style));
        }
    } else if let Some(rest) = disp.strip_prefix('~') {
        let tilde_style = if cand.exists {
            theme.home_tilde()
        } else {
            base
        };
        spans.push(Span::styled("~", tilde_style));
        spans.push(Span::styled(rest.to_string(), base));
    } else {
        spans.push(Span::styled(disp, base));
    }

    // 右填充：与选中行同背景（背景由整行 Line.style 统一给出，这里补空格宽度即可）。
    if pad > 0 {
        spans.push(Span::styled(" ".repeat(pad), base));
    }
    spans
}

fn stale_badge_spans(theme: &Theme) -> Vec<Span<'static>> {
    const BADGE_W: usize = SCORE_BAR_CELLS + 3;
    let label = "已失效";
    let pad = BADGE_W.saturating_sub(UnicodeWidthStr::width(label));
    vec![Span::styled(
        format!("{label}{}", " ".repeat(pad)),
        theme.dim(),
    )]
}

/// 分数条：`filled` 是动画中的当前子信号，`score` 是融合目标分（右侧数字）。
fn score_bar_spans(filled: [f32; 4], score: f32, theme: &Theme) -> Vec<Span<'static>> {
    let cells = SCORE_BAR_CELLS;
    let segment_cells = score_segment_cells(filled, cells);
    let colors = [
        theme.score_frecency(),
        theme.score_recency(),
        theme.score_context(),
        theme.score_uniq(),
    ];
    let mut spans = Vec::with_capacity(cells + 1);
    for (idx, &count) in segment_cells.iter().enumerate() {
        for _ in 0..count {
            spans.push(Span::styled("▰", Style::default().fg(colors[idx])));
        }
    }
    while spans.len() < cells {
        spans.push(Span::styled("▱", theme.dim()));
    }
    let pct = (score * 100.0).round() as u32;
    spans.push(Span::styled(format!(" {pct:>2}"), theme.dim()));
    spans
}

fn score_segment_cells(values: [f32; 4], cells: usize) -> [usize; 4] {
    let values = values.map(|value| value.clamp(0.0, 1.0));
    let total: f32 = values.iter().sum();
    if total <= f32::EPSILON || cells == 0 {
        return [0; 4];
    }

    let filled = ((total / values.len() as f32) * cells as f32).round() as usize;
    let filled = filled.min(cells);
    if filled == 0 {
        return [0; 4];
    }

    let mut counts = [0usize; 4];
    let mut remainders = [(0.0f32, 0usize); 4];
    let mut used = 0usize;
    for (idx, value) in values.iter().enumerate() {
        let raw = *value / total * filled as f32;
        let whole = raw.floor() as usize;
        counts[idx] = whole;
        remainders[idx] = (raw - whole as f32, idx);
        used += whole;
    }

    remainders.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    for &(_, idx) in remainders.iter().take(filled.saturating_sub(used)) {
        counts[idx] += 1;
    }

    counts
}

fn render_preview(f: &mut Frame, app: &App, theme: &Theme, area: Rect) {
    let block = Block::default()
        .borders(Borders::LEFT)
        .border_style(theme.border())
        .padding(Padding::new(1, 0, 0, 0));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut lines = Vec::new();
    let col_w = inner.width.saturating_sub(1) as usize;
    let Some(cand) = app.selected_candidate() else {
        lines.push(Line::from(Span::styled("无选中项", theme.dim())));
        f.render_widget(Paragraph::new(lines), inner);
        return;
    };

    lines.push(Line::from(Span::styled(
        trim_middle(&cand.raw, col_w),
        theme.path().add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::raw(""));

    let outcome = preview_outcome_for_selected(app, cand);
    match outcome {
        PreviewPanelOutcome::Loading => {
            lines.push(Line::from(Span::styled("加载中…", theme.dim())));
        }
        PreviewPanelOutcome::Missing => {
            lines.push(Line::from(Span::styled("目录已不存在", theme.dim())));
        }
        PreviewPanelOutcome::Outcome(PreviewOutcome::Missing) => {
            lines.push(Line::from(Span::styled("目录已不存在", theme.dim())));
        }
        PreviewPanelOutcome::Outcome(PreviewOutcome::Error(message)) => {
            lines.push(Line::from(vec![
                Span::styled("无法读取: ", theme.dim()),
                Span::styled(trim_middle(message, col_w.saturating_sub(10)), theme.path()),
            ]));
        }
        PreviewPanelOutcome::Outcome(PreviewOutcome::Data(data)) => {
            if let Some(git) = &data.git {
                let dot_style = if git.dirty == Some(true) {
                    Style::default().fg(theme.accent())
                } else {
                    theme.match_hl()
                };
                lines.push(Line::from(vec![
                    Span::styled("● ", dot_style),
                    Span::styled(
                        trim_middle(&git.branch, col_w.saturating_sub(2)),
                        theme.path(),
                    ),
                ]));
                lines.push(Line::raw(""));
            }
            if data.entries.is_empty() {
                lines.push(Line::from(Span::styled("空目录", theme.dim())));
            } else {
                for entry in &data.entries {
                    let icon = if entry.is_dir { "▸ " } else { "· " };
                    let name_w = col_w.saturating_sub(2);
                    lines.push(Line::from(vec![
                        Span::styled(icon, theme.dim()),
                        Span::styled(trim_middle(&entry.name, name_w), theme.path()),
                    ]));
                }
                if data.has_more_entries {
                    lines.push(Line::from(Span::styled("… 还有更多项", theme.dim())));
                }
            }
        }
    }

    f.render_widget(Paragraph::new(lines), inner);
}

enum PreviewPanelOutcome<'a> {
    Loading,
    Missing,
    Outcome(&'a PreviewOutcome),
}

fn preview_outcome_for_selected<'a>(app: &'a App, cand: &Candidate) -> PreviewPanelOutcome<'a> {
    if !cand.exists {
        return PreviewPanelOutcome::Missing;
    }
    if app.preview_loading.as_deref() == Some(cand.raw.as_str())
        || app
            .preview_pending
            .as_ref()
            .map(|(path, _)| path == &cand.raw)
            .unwrap_or(false)
    {
        return PreviewPanelOutcome::Loading;
    }
    if let Some((path, outcome)) = &app.preview_current {
        if path == &cand.raw {
            return PreviewPanelOutcome::Outcome(outcome);
        }
    }
    PreviewPanelOutcome::Loading
}

fn render_input(f: &mut Frame, app: &App, theme: &Theme, area: Rect) {
    let caret = "▌";
    let prompt_style = Style::default()
        .fg(theme.accent())
        .add_modifier(Modifier::BOLD);
    let mut spans = vec![
        Span::styled("  ", Style::default()),
        Span::styled("❯ ", prompt_style),
    ];
    if app.query.is_empty() {
        spans.push(Span::styled("输入以模糊搜索…", theme.dim()));
        spans.push(Span::styled(caret, theme.dim()));
    } else {
        spans.push(Span::styled(app.query.clone(), theme.path()));
        spans.push(Span::styled(caret, Style::default().fg(theme.accent())));
    }
    // 右侧快捷键提示：按显示宽度核算，空间不足时降级为短版或省略。
    let used: usize = spans
        .iter()
        .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
        .sum();
    let full_hint = "↑↓ 选择 · ⏎ 跳转 · F2 预览 · F1 帮助";
    let short_hint = "⏎ 跳转 · esc 退出";
    let avail = (area.width as usize).saturating_sub(used).saturating_sub(1);
    let hint = if UnicodeWidthStr::width(full_hint) <= avail {
        Some(full_hint)
    } else if UnicodeWidthStr::width(short_hint) <= avail {
        Some(short_hint)
    } else {
        None
    };
    if let Some(hint) = hint {
        let pad = avail.saturating_sub(UnicodeWidthStr::width(hint));
        spans.push(Span::raw(" ".repeat(pad)));
        spans.push(Span::styled(hint, theme.dim()));
    }

    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_help(f: &mut Frame, theme: &Theme, full: Rect) {
    let lines = vec![
        Line::from(Span::styled(" 快捷键", theme.title())),
        Line::raw(""),
        help_row("↑ / ↓ / k / j", "移动选择", theme),
        help_row("Ctrl+N / Ctrl+P", "下 / 上移动", theme),
        help_row("PageUp / PageDown", "翻页（±10）", theme),
        help_row("Home / End", "首 / 末项", theme),
        help_row("任意字符", "模糊搜索过滤", theme),
        help_row("Enter / Tab", "跳转到选中目录", theme),
        help_row("Ctrl+D", "删除失效目录记录", theme),
        help_row("F2", "切换预览面板", theme),
        help_row("Esc", "清空搜索 / 退出", theme),
        help_row("Ctrl+C / Ctrl+G", "退出", theme),
        help_row("鼠标", "单击选中 · 双击跳转 · 滚轮滚动", theme),
        help_row("分数条", "青=常去 蓝=最近 紫=相关 灰=去重", theme),
        Line::raw(""),
        Line::from(Span::styled(" 按任意键关闭", theme.dim())),
    ];
    let w = 64u16.min(full.width.saturating_sub(4));
    let h = (lines.len() as u16 + 2).min(full.height.saturating_sub(2));
    let area = centered(full, w, h);
    f.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.accent()))
        .padding(Padding::new(1, 1, 0, 0));
    let inner = block.inner(area);
    f.render_widget(block, area);
    f.render_widget(Paragraph::new(lines), inner);
}

fn render_confirm_delete(f: &mut Frame, app: &App, theme: &Theme, full: Rect, idx: usize) {
    let path = app
        .cands
        .get(idx)
        .map(|cand| trim_middle(&cand.display, 34))
        .unwrap_or_else(|| "未知目录".to_string());
    let lines = vec![
        Line::from(Span::styled(" 删除失效记录", theme.title())),
        Line::raw(""),
        Line::from(vec![
            Span::styled("  ", theme.dim()),
            Span::styled(path, theme.dim().add_modifier(Modifier::CROSSED_OUT)),
        ]),
        Line::raw(""),
        Line::from(Span::styled(" 再按 Ctrl+D 删除，按其他键取消", theme.dim())),
    ];
    let w = 44u16.min(full.width.saturating_sub(4));
    let h = (lines.len() as u16 + 2).min(full.height.saturating_sub(2));
    let area = centered(full, w, h);
    f.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.accent()))
        .padding(Padding::new(1, 1, 0, 0));
    let inner = block.inner(area);
    f.render_widget(block, area);
    f.render_widget(Paragraph::new(lines), inner);
}

fn help_row<'a>(key: &'a str, desc: &'a str, theme: &Theme) -> Line<'a> {
    // 按显示宽度把 key 列填充到固定 20 列（中文 key 也能对齐 desc 列）。
    const KEY_COL: usize = 20;
    let kw = UnicodeWidthStr::width(key);
    let pad = KEY_COL.saturating_sub(kw);
    Line::from(vec![
        Span::styled(
            format!("  {key}{}", " ".repeat(pad)),
            Style::default().fg(theme.accent()),
        ),
        Span::styled(desc.to_string(), theme.path()),
    ])
}

fn centered(full: Rect, w: u16, h: u16) -> Rect {
    let x = full.x + (full.width.saturating_sub(w)) / 2;
    let y = full.y + (full.height.saturating_sub(h)) / 2;
    Rect::new(x, y, w, h)
}

// ---------------- 文本工具 ----------------
/// 按“显示宽度”中截：超过 `max` 显示列时保留首尾、中间用 … 省略。
/// CJK/宽字符按 2 列计，保证不会把布局撑破。
fn trim_middle(s: &str, max: usize) -> String {
    let total = UnicodeWidthStr::width(s);
    if total <= max || max == 0 {
        return s.to_string();
    }
    if max < 2 {
        return "…".to_string();
    }
    // 目标：head_w + 1(…) + tail_w <= max，head 略多于 tail。
    let budget = max - 1;
    let left_budget = budget - budget / 2; // 等价于 ceil(budget/2)，避免 div_ceil 的 MSRV 依赖
    let right_budget = budget - left_budget;

    let head = take_width_front(s, left_budget);
    let tail = take_width_back(s, right_budget);
    format!("{head}…{tail}")
}

/// 从前向后取不超过 `w` 显示列的前缀。
fn take_width_front(s: &str, w: usize) -> String {
    let mut acc = 0;
    let mut out = String::new();
    for ch in s.chars() {
        let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
        if acc + cw > w {
            break;
        }
        acc += cw;
        out.push(ch);
    }
    out
}

/// 从后向前取不超过 `w` 显示列的后缀。
fn take_width_back(s: &str, w: usize) -> String {
    let mut acc = 0;
    let mut rev = String::new();
    for ch in s.chars().rev() {
        let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
        if acc + cw > w {
            break;
        }
        acc += cw;
        rev.push(ch);
    }
    rev.chars().rev().collect()
}

fn beep() {
    let mut err = io::stderr();
    let _ = err.write_all(b"\x07");
    let _ = err.flush();
}

// ---------------- 测试 ----------------
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EffectiveConfig, Paths};
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn recs(paths: &[(&str, f64)]) -> Vec<Recommendation> {
        paths
            .iter()
            .map(|(p, s)| Recommendation {
                path: p.to_string(),
                score: *s,
                breakdown: crate::recommend::ScoreBreakdown {
                    frecency_norm: *s,
                    recency_norm: *s,
                    context_norm: *s,
                    uniq_norm: *s,
                },
                exists: true,
            })
            .collect()
    }

    fn recs_with_exists(paths: &[(&str, f64, bool)]) -> Vec<Recommendation> {
        paths
            .iter()
            .map(|(p, s, exists)| Recommendation {
                path: p.to_string(),
                score: *s,
                breakdown: crate::recommend::ScoreBreakdown {
                    frecency_norm: *s,
                    recency_norm: *s,
                    context_norm: *s,
                    uniq_norm: *s,
                },
                exists: *exists,
            })
            .collect()
    }

    fn test_ctx(name: &str) -> (PathBuf, AppContext) {
        let uniq = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("cdh_picker_test_{name}_{uniq}"));
        let paths = Paths {
            config_dir: root.join("config"),
            data_dir: root.join("data"),
            state_dir: root.join("state"),
            cache_dir: root.join("cache"),
            history_raw: root.join("data").join("history").join("history_raw"),
            history_uniq: root.join("data").join("history").join("history_uniq"),
        };
        fs::create_dir_all(paths.history_raw.parent().unwrap()).unwrap();
        (
            root,
            AppContext {
                paths,
                config: EffectiveConfig {
                    limit: None,
                    half_life: 7.0 * 24.0 * 3600.0,
                    threshold: 0.0,
                    ignore_re: None,
                    check_dir: true,
                    uniq_decay: 0.85,
                    recency_half_life: 24.0 * 3600.0,
                    debounce_secs: 600,
                    w_frecency: 0.40,
                    w_uniq: 0.10,
                    w_recency: 0.30,
                    w_context: 0.20,
                },
            },
        )
    }

    #[test]
    fn empty_returns_none() {
        assert_eq!(pick(&[]).unwrap(), None);
    }

    #[test]
    fn trim_middle_basic() {
        assert_eq!(trim_middle("abcdefgh", 5), "ab…gh");
        assert_eq!(trim_middle("short", 10), "short");
        assert_eq!(trim_middle("abc", 0), "abc");
    }

    #[test]
    fn build_candidates_tilde_abbreviation() {
        std::env::set_var("HOME", "/home/tester");
        let cands = build_candidates(&recs(&[
            ("/home/tester", 1.0),
            ("/home/tester/work", 0.5),
            ("/etc", 0.1),
        ]));
        assert_eq!(cands[0].display, "~");
        assert_eq!(cands[1].display, "~/work");
        assert_eq!(cands[2].display, "/etc");
    }

    #[test]
    fn filter_empty_query_preserves_order() {
        let cands = build_candidates(&recs(&[("/a", 0.9), ("/b", 0.5), ("/c", 0.1)]));
        let mut filter = Filter::new();
        let m = filter.run(&cands, "");
        assert_eq!(m.len(), 3);
        assert_eq!(m[0].idx, 0);
        assert_eq!(m[2].idx, 2);
    }

    #[test]
    fn filter_places_stale_matches_after_existing_matches() {
        let cands = build_candidates(&recs_with_exists(&[
            ("/work/missing-cdh", 0.99, false),
            ("/work/live-cdh", 0.10, true),
        ]));
        let mut filter = Filter::new();
        let m = filter.run(&cands, "cdh");
        assert_eq!(m.len(), 2);
        assert_eq!(cands[m[0].idx].raw, "/work/live-cdh");
        assert_eq!(cands[m[1].idx].raw, "/work/missing-cdh");
    }

    #[test]
    fn filter_fuzzy_matches_subsequence() {
        let cands = build_candidates(&recs(&[
            ("/home/work/repos/cdh", 0.9),
            ("/etc/nginx", 0.5),
            ("/var/log", 0.1),
        ]));
        let mut filter = Filter::new();
        let m = filter.run(&cands, "cdh");
        assert!(!m.is_empty());
        // 命中 cdh 的应排在最前
        assert_eq!(cands[m[0].idx].raw, "/home/work/repos/cdh");
    }

    #[test]
    fn filter_no_match_returns_empty() {
        let cands = build_candidates(&recs(&[("/a", 0.9), ("/b", 0.5)]));
        let mut filter = Filter::new();
        let m = filter.run(&cands, "zzzzzz");
        assert!(m.is_empty());
    }

    #[test]
    fn move_by_wraps() {
        let mut app = App::new(build_candidates(&recs(&[("/a", 0.9), ("/b", 0.5)])));
        assert_eq!(app.selected, 0);
        app.move_by(-1);
        assert_eq!(app.selected, 1); // 环绕到末尾
        app.move_by(1);
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn build_candidates_clamps_score_breakdown() {
        let items = vec![Recommendation {
            path: "/wide".to_string(),
            score: 1.5,
            breakdown: crate::recommend::ScoreBreakdown {
                frecency_norm: -1.0,
                recency_norm: 0.25,
                context_norm: 2.0,
                uniq_norm: 0.75,
            },
            exists: true,
        }];

        let cands = build_candidates(&items);
        assert_eq!(cands[0].score, 1.0);
        assert_eq!(cands[0].breakdown, [0.0, 0.25, 1.0, 0.75]);
    }

    #[test]
    fn ctrl_d_confirmation_removes_stale_candidate_and_history() {
        let (root, ctx) = test_ctx("ctrl_d_delete");
        let stale = root.join("stale");
        let keep = root.join("keep");
        fs::create_dir_all(&keep).unwrap();
        fs::write(
            &ctx.paths.history_raw,
            format!(
                "100\t{}\n101\t{}\n102\t{}\n",
                stale.display(),
                keep.display(),
                stale.display()
            ),
        )
        .unwrap();
        fs::write(
            &ctx.paths.history_uniq,
            format!("{}\n{}\n", stale.display(), keep.display()),
        )
        .unwrap();

        let items = recs_with_exists(&[
            (keep.to_str().unwrap(), 0.5, true),
            (stale.to_str().unwrap(), 0.9, false),
        ]);
        let mut app = App::new(build_candidates(&items));
        app.selected = 1;

        assert_eq!(
            handle_key(
                &mut app,
                KeyCode::Char('d'),
                KeyModifiers::CONTROL,
                Some(&ctx)
            ),
            None
        );
        assert_eq!(app.mode, Mode::ConfirmDelete { candidate_idx: 1 });

        assert_eq!(
            handle_key(
                &mut app,
                KeyCode::Char('d'),
                KeyModifiers::CONTROL,
                Some(&ctx)
            ),
            None
        );

        assert_eq!(app.cands.len(), 1);
        assert_eq!(app.cands[0].raw, keep.to_string_lossy());
        let raw = fs::read_to_string(&ctx.paths.history_raw).unwrap();
        assert!(!raw.contains(stale.to_str().unwrap()));
        assert!(raw.contains(keep.to_str().unwrap()));
        assert_eq!(
            fs::read_to_string(&ctx.paths.history_uniq).unwrap(),
            format!("{}\n", keep.display())
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn ctrl_d_can_remove_last_stale_candidate_without_invalid_selection() {
        let (root, ctx) = test_ctx("ctrl_d_delete_last");
        let stale = root.join("stale");
        fs::write(
            &ctx.paths.history_raw,
            format!("100\t{}\n", stale.display()),
        )
        .unwrap();
        fs::write(&ctx.paths.history_uniq, format!("{}\n", stale.display())).unwrap();

        let items = recs_with_exists(&[(stale.to_str().unwrap(), 0.9, false)]);
        let mut app = App::new(build_candidates(&items));

        assert_eq!(
            handle_key(
                &mut app,
                KeyCode::Char('d'),
                KeyModifiers::CONTROL,
                Some(&ctx)
            ),
            None
        );
        assert_eq!(app.mode, Mode::ConfirmDelete { candidate_idx: 0 });
        assert_eq!(
            handle_key(
                &mut app,
                KeyCode::Char('d'),
                KeyModifiers::CONTROL,
                Some(&ctx)
            ),
            None
        );

        assert!(app.cands.is_empty());
        assert!(app.matches.is_empty());
        assert_eq!(app.selected, 0);
        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(
            handle_key(&mut app, KeyCode::Enter, KeyModifiers::NONE, Some(&ctx)),
            None
        );
        assert_eq!(fs::read_to_string(&ctx.paths.history_raw).unwrap(), "");
        assert_eq!(fs::read_to_string(&ctx.paths.history_uniq).unwrap(), "");

        let _ = fs::remove_dir_all(root);
    }

    fn preview_data(names: &[&str]) -> PreviewOutcome {
        PreviewOutcome::Data(PreviewData {
            git: None,
            entries: names
                .iter()
                .map(|name| PreviewEntry {
                    name: (*name).to_string(),
                    is_dir: false,
                })
                .collect(),
            has_more_entries: false,
        })
    }

    #[test]
    fn stale_preview_generation_is_ignored() {
        let mut app = App::with_preview_worker(build_candidates(&recs(&[("/a", 0.9)])), None, true);
        app.preview_selected_path = Some("/a".to_string());
        app.preview_generation = 2;

        app.accept_preview_response(PreviewResponse {
            path: "/a".to_string(),
            generation: 1,
            outcome: preview_data(&["old"]),
        });

        assert!(app.preview_current.is_none());
        assert!(app.preview_cache.is_empty());
    }

    #[test]
    fn preview_cache_hit_avoids_worker_request() {
        let now = Instant::now();
        let mut app = App::with_preview_worker(build_candidates(&recs(&[("/a", 0.9)])), None, true);
        app.insert_preview_cache("/a".to_string(), preview_data(&["cached"]));

        app.update_preview(now + PREVIEW_DEBOUNCE + Duration::from_millis(1));

        assert_eq!(app.preview_generation, 0);
        assert!(app.preview_pending.is_none());
        assert!(app.preview_loading.is_none());
        assert_eq!(
            app.preview_current,
            Some(("/a".to_string(), preview_data(&["cached"])))
        );
    }

    #[test]
    fn preview_worker_disconnect_is_reported_without_panic() {
        let (request_tx, request_rx) = mpsc::channel::<PreviewRequest>();
        drop(request_rx);
        let (_response_tx, response_rx) = mpsc::channel::<PreviewResponse>();
        let worker = PreviewWorker {
            requests: request_tx,
            responses: response_rx,
        };
        let now = Instant::now();
        let mut app =
            App::with_preview_worker(build_candidates(&recs(&[("/a", 0.9)])), Some(worker), true);

        app.update_preview(now);
        app.update_preview(now + PREVIEW_DEBOUNCE + Duration::from_millis(1));

        assert!(app.preview_worker.is_none());
        assert_eq!(
            app.preview_current,
            Some((
                "/a".to_string(),
                PreviewOutcome::Error("预览功能不可用".to_string())
            ))
        );
    }

    #[test]
    fn f2_toggles_preview_visibility() {
        let mut app = App::with_preview_worker(build_candidates(&recs(&[("/a", 0.9)])), None, true);

        assert!(app.preview_visible);
        assert_eq!(
            handle_key(&mut app, KeyCode::F(2), KeyModifiers::NONE, None),
            None
        );
        assert!(!app.preview_visible);
        assert_eq!(
            handle_key(&mut app, KeyCode::F(2), KeyModifiers::NONE, None),
            None
        );
        assert!(app.preview_visible);
    }

    #[test]
    fn preview_layout_respects_width_and_visibility() {
        let mut app = App::with_preview_worker(build_candidates(&recs(&[("/a", 0.9)])), None, true);

        assert!(!preview_layout_enabled(&app, PREVIEW_MIN_WIDTH - 1));
        assert!(preview_layout_enabled(&app, PREVIEW_MIN_WIDTH));
        app.preview_visible = false;
        assert!(!preview_layout_enabled(&app, PREVIEW_MIN_WIDTH));
    }
}
