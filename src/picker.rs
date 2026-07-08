//! 交互式目录选择器（ratatui 重写 · 霓虹渐变现代风 + 平滑过渡动画）
//!
//! 设计要点：
//! - 视觉：深色卡片 + 青→紫渐变高亮条；右侧彩色分数条反映 frecency 融合分。
//! - 搜索：默认即可输入，nucleo 模糊匹配（fzf 风格），命中字符高亮，按匹配分排序。
//! - 交互：↑/↓ 移动，PageUp/Down 翻页，Enter/Tab 选中，Esc 清查询/退出，鼠标单击/双击/滚轮。
//! - 动画：高亮条位置缓动 + 分数条增长 + 淡入；仅在有动画时以 ~60fps tick，空闲回到阻塞事件。
//!
//! 兼容：非交互（无 TTY）直接返回第一项；`CDH_COLOR=0` 关色，`CDH_MOUSE=0` 关鼠标，`CDH_ANIM=0` 关动画。

use std::env;
use std::io::{self, IsTerminal, Write};
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
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

// ---------------- 常量 ----------------
const WATCHDOG_TIMEOUT: Duration = Duration::from_secs(15);
const FRAME_MS: u64 = 16; // ~60fps 动画帧
const ANIM_SPEED: f32 = 0.28; // 缓动系数（每帧向目标靠拢的比例）
const ANIM_EPS: f32 = 0.004; // 动画收敛阈值
const SCORE_BAR_CELLS: usize = 5; // 分数条格子数
const DOUBLE_CLICK_MS: u128 = 300;
const MIN_HEIGHT: u16 = 6;

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
    /// 根据分数在青(低)→紫(高)之间取渐变色。
    fn score_color(&self, t: f32) -> Color {
        let t = t.clamp(0.0, 1.0);
        // 青 (80,250,220) → 紫 (189,147,249)
        let r = lerp(0x50 as f32, 0xbd as f32, t) as u8;
        let g = lerp(0xfa as f32, 0x93 as f32, t) as u8;
        let b = lerp(0xdc as f32, 0xf9 as f32, t) as u8;
        self.c(r, g, b)
    }
    fn accent(&self) -> Color {
        self.c(0xbd, 0x93, 0xf9)
    }
}

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
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
    run_ui(items)
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
        let mut scored: Vec<(u32, usize, Vec<u32>)> = Vec::new();
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
                scored.push((score, idx, indices));
            }
        }
        // 匹配分降序；同分时按候选自身分数降序，保证 frecency 高的靠前。
        scored.sort_by(|a, b| {
            b.0.cmp(&a.0).then_with(|| {
                cands[b.1]
                    .score
                    .partial_cmp(&cands[a.1].score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
        });
        scored
            .into_iter()
            .map(|(_, idx, hl)| Match { idx, hl })
            .collect()
    }
}

// ---------------- 应用状态 ----------------
struct App {
    cands: Vec<Candidate>,
    filter: Filter,
    query: String,
    matches: Vec<Match>,
    selected: usize, // matches 内下标
    offset: usize,   // 列表滚动偏移
    // 动画状态
    anim_cursor: f32,      // 平滑高亮行（浮点）
    anim_scores: Vec<f32>, // 每个原始候选当前分数条填充（0~1）
    fade: f32,             // 打开淡入（0→1）
    show_help: bool,
    last_click: Option<(usize, Instant)>,
    /// 上一帧实际渲染的列表区域（供鼠标命中测试对齐真实布局）。
    last_list_area: std::cell::Cell<Rect>,
}
impl App {
    fn new(cands: Vec<Candidate>) -> Self {
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
            anim_scores: vec![0.0; n],
            fade: if anim_enabled() { 0.0 } else { 1.0 },
            show_help: false,
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
                self.anim_scores[m.idx] = self.cands[m.idx].score;
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
            let tgt = self.cands[m.idx].score * self.fade;
            let cur = &mut self.anim_scores[m.idx];
            if (*cur - tgt).abs() > ANIM_EPS {
                *cur += (tgt - *cur) * ANIM_SPEED;
                busy = true;
            } else {
                *cur = tgt;
            }
        }
        busy
    }
}

// ---------------- 主循环 ----------------
fn run_ui(items: &[Recommendation]) -> io::Result<Option<String>> {
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
                if let Some(result) = handle_key(&mut app, key.code, key.modifiers) {
                    return Ok(result);
                }
            }
            Event::Mouse(me) if mouse => {
                seen_key = true;
                idle_since = Instant::now();
                if app.show_help {
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
fn handle_key(app: &mut App, code: KeyCode, mods: KeyModifiers) -> Option<Option<String>> {
    // 帮助浮层：任意键关闭。
    if app.show_help {
        app.show_help = false;
        return None;
    }
    let ctrl = mods.contains(KeyModifiers::CONTROL);
    match code {
        KeyCode::Char('c') if ctrl => return Some(None),
        KeyCode::Char('g') if ctrl => return Some(None),
        KeyCode::Enter | KeyCode::Tab => {
            if app.matches.is_empty() {
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
        KeyCode::F(1) => app.show_help = true,
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

    // 内容区再切成 列表 + 输入行
    let chunks = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(inner);
    let list_area = chunks[0];
    let input_area = chunks[1];

    render_list(f, app, theme, list_area);
    render_input(f, app, theme, input_area);

    if app.show_help {
        render_help(f, theme, full);
    }
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
        spans.push(Span::styled(
            marker,
            Style::default()
                .fg(theme.accent())
                .add_modifier(Modifier::BOLD),
        ));

        // 路径列（~ 着色 + 命中高亮 + 中截省略 + 右填充到 path_w）
        spans.extend(path_spans(cand, &m.hl, path_w, theme, is_sel));

        // 右侧分数条
        if show_score {
            let filled = app.anim_scores[m.idx];
            spans.push(Span::raw(" "));
            spans.extend(score_bar_spans(filled, cand.score, theme));
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
    let disp = trim_middle(&cand.display, col_w);
    let disp_w = UnicodeWidthStr::width(disp.as_str());
    let pad = col_w.saturating_sub(disp_w);
    let truncated = disp != cand.display;

    let mut spans = Vec::new();

    // 命中索引针对完整 display 的字符序号；截断后无法对齐，故仅未截断时逐字符高亮。
    if !hl.is_empty() && !truncated {
        let hl_set: std::collections::HashSet<u32> = hl.iter().copied().collect();
        for (i, ch) in cand.display.chars().enumerate() {
            let style = if hl_set.contains(&(i as u32)) {
                theme.match_hl()
            } else if i == 0 && ch == '~' {
                theme.home_tilde()
            } else {
                base
            };
            spans.push(Span::styled(ch.to_string(), style));
        }
    } else if let Some(rest) = disp.strip_prefix('~') {
        spans.push(Span::styled("~", theme.home_tilde()));
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

/// 分数条：`filled` 是动画中的当前填充（0~1），`score` 是目标分（决定颜色深浅）。
fn score_bar_spans(filled: f32, score: f32, theme: &Theme) -> Vec<Span<'static>> {
    let cells = SCORE_BAR_CELLS;
    let filled_cells = (filled * cells as f32).round() as usize;
    let mut spans = Vec::with_capacity(cells + 1);
    for i in 0..cells {
        let t = (i as f32 + 0.5) / cells as f32;
        if i < filled_cells {
            spans.push(Span::styled("▰", Style::default().fg(theme.score_color(t))));
        } else {
            spans.push(Span::styled("▱", theme.dim()));
        }
    }
    let pct = (score * 100.0).round() as u32;
    spans.push(Span::styled(format!(" {pct:>2}"), theme.dim()));
    spans
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
    let full_hint = "↑↓ 选择 · ⏎ 跳转 · esc 退出 · F1 帮助";
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
        help_row("Esc", "清空搜索 / 退出", theme),
        help_row("Ctrl+C / Ctrl+G", "退出", theme),
        help_row("鼠标", "单击选中 · 双击跳转 · 滚轮滚动", theme),
        Line::raw(""),
        Line::from(Span::styled(" 按任意键关闭", theme.dim())),
    ];
    let w = 46u16.min(full.width.saturating_sub(4));
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

    fn recs(paths: &[(&str, f64)]) -> Vec<Recommendation> {
        paths
            .iter()
            .map(|(p, s)| Recommendation {
                path: p.to_string(),
                score: *s,
            })
            .collect()
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
}
