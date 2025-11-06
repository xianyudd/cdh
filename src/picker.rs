//! 交互式列表选择器（默认贴底；搜索 i/ESC；q 仅主界面退出；鼠标单击移动/双击选中）
//! - 主界面：↑/↓/k/j 移动；←/→/p/n 翻页；0..9 数字直达；Enter 选；q 退；h 帮助；i 搜索
//! - 搜索模式：字符均加入查询（含 j/k/p/n/q/数字）；↑/↓/←/→ 移动/翻页；Ctrl+N/P 下/上；Enter/Tab 选；Esc 返回
//! - 搜索优化：粘性焦点 + 单结果回车直接选中 + 结果为 0 时 Beep

use crossterm::{
    cursor::{Hide, MoveTo, Show},
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyModifiers, MouseButton,
        MouseEvent, MouseEventKind,
    },
    style::{
        Attribute, Color, Print, ResetColor, SetAttribute, SetBackgroundColor, SetForegroundColor,
    },
    terminal::{
        disable_raw_mode, enable_raw_mode, size, Clear, ClearType, EnterAlternateScreen,
        LeaveAlternateScreen,
    },
    ExecutableCommand, QueueableCommand,
};
use std::env;
use std::io::{self, IsTerminal, Stdout, Write};
use std::time::{Duration, Instant};
use unicode_width::UnicodeWidthStr;

const PER_PAGE: usize = 10;
const WATCHDOG_TIMEOUT: Duration = Duration::from_secs(15);
const CARET_BLINK_MS: u64 = 500;
const DOUBLE_CLICK_MS: u64 = 300;

fn color_enabled() -> bool {
    env::var("CDH_COLOR")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(true)
}
fn mouse_enabled() -> bool {
    env::var("CDH_MOUSE")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(true)
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Mode {
    Normal,
    Help,
    Search,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum InputPos {
    Bottom,
    Top,
    Title,
    Overlay,
}
fn input_pos_from_env() -> InputPos {
    match env::var("CDH_INPUT_POS").map(|s| s.to_lowercase()) {
        Ok(s) if s == "top" => InputPos::Top,
        Ok(s) if s == "title" => InputPos::Title,
        Ok(s) if s == "overlay" => InputPos::Overlay,
        _ => InputPos::Bottom,
    }
}

// ---------------- UI 守卫 ----------------
struct UiGuard {
    active: bool,
    mouse: bool,
}
impl UiGuard {
    fn new(mouse: bool) -> io::Result<Self> {
        enable_raw_mode()?;
        let mut out = io::stdout();
        out.execute(EnterAlternateScreen)?;
        out.execute(Hide)?;
        if mouse {
            out.execute(EnableMouseCapture)?;
        }
        out.flush()?;
        Ok(Self { active: true, mouse })
    }
}
impl Drop for UiGuard {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        let mut out = io::stdout();
        let _ = out.execute(Show);
        if self.mouse {
            let _ = out.execute(DisableMouseCapture);
        }
        let _ = out.execute(LeaveAlternateScreen);
        let _ = out.flush();
        let _ = disable_raw_mode();
    }
}

// ---------------- 对外 API ----------------
pub fn pick<S: AsRef<str>>(items: &[S]) -> io::Result<Option<String>> {
    if !io::stderr().is_terminal() || !io::stdin().is_terminal() {
        return Ok(items.get(0).map(|s| s.as_ref().to_string()));
    }
    let items: Vec<String> = items.iter().map(|s| s.as_ref().to_string()).collect();
    run_ui(&items)
}

// ---------------- 主循环 ----------------
fn run_ui(items: &[String]) -> io::Result<Option<String>> {
    let _guard = UiGuard::new(mouse_enabled())?;
    let mut stdout = io::stdout();

    let (mut w, mut h) = size()?;
    ensure(h >= 5, "终端高度至少需要 5 行")?;

    let mut panel_h = (h.saturating_sub(2)).min(12).max(5);
    let mut top_margin = compute_top_margin_bottom(h, panel_h);

    let input_pos = input_pos_from_env();
    let mut mode = Mode::Normal;

    let mut view = View::new(items.len());
    let mut st = State::new(view.page_count());

    // 双击
    let mut last_click_at: Option<Instant> = None;
    let mut last_click_abs: Option<usize> = None;

    // 搜索输入
    let mut query = String::new();
    let mut caret_visible = true;
    let mut last_blink = Instant::now();

    // 记录“上次高亮”的绝对索引，用于粘性焦点
    let mut last_abs_highlight: Option<usize>;

    redraw_main(
        &mut stdout, w, h, panel_h, top_margin, &st, &view, items, mode, &query, caret_visible,
        input_pos,
    )?;

    let mut idle_since = Instant::now();
    let mut seen_key = false;

    loop {
        if idle_since.elapsed() > WATCHDOG_TIMEOUT && !seen_key {
            return Ok(None);
        }

        if mode == Mode::Search && last_blink.elapsed() >= Duration::from_millis(CARET_BLINK_MS) {
            caret_visible = !caret_visible;
            last_blink = Instant::now();
            redraw_main(
                &mut stdout, w, h, panel_h, top_margin, &st, &view, items, mode, &query,
                caret_visible, input_pos,
            )?;
        }

        if event::poll(Duration::from_millis(250))? {
            match event::read()? {
                Event::Resize(w1, h1) => {
                    w = w1; h = h1;
                    panel_h = (h.saturating_sub(2)).min(12).max(5);
                    top_margin = compute_top_margin_bottom(h, panel_h);
                    st.clamp_cursor_on_resize(&view);
                    redraw_main(
                        &mut stdout, w, h, panel_h, top_margin, &st, &view, items, mode, &query,
                        caret_visible, input_pos,
                    )?;
                }
                Event::Key(k) => {
                    seen_key = true;
                    idle_since = Instant::now();

                    match mode {
                        Mode::Help => {
                            match k.code {
                                KeyCode::Char('q') | KeyCode::Esc => {
                                    mode = Mode::Normal;
                                    redraw_main(
                                        &mut stdout, w, h, panel_h, top_margin, &st, &view, items,
                                        mode, &query, caret_visible, input_pos,
                                    )?;
                                }
                                _ => {}
                            }
                        }
                        Mode::Search => {
                            match k.code {
                                KeyCode::Esc => {
                                    mode = Mode::Normal;
                                    query.clear();
                                    view.clear_filter(items.len());
                                    st.reset_pages(view.page_count());
                                    caret_visible = true;
                                    redraw_main(
                                        &mut stdout, w, h, panel_h, top_margin, &st, &view, items,
                                        mode, &query, caret_visible, input_pos,
                                    )?;
                                }
                                KeyCode::Enter | KeyCode::Tab => {
                                    // 智能回车
                                    let n = view.view_len();
                                    if n == 0 {
                                        beep(&mut stdout)?;
                                        continue;
                                    }
                                    let abs = if n == 1 {
                                        view.abs_index_from_page_cursor(1, 0).unwrap()
                                    } else {
                                        match view.abs_index_from_page_cursor(st.page, st.cursor) {
                                            Some(a) => a,
                                            None => {
                                                view.best_focus(items, &query)
                                                    .and_then(|(p,c)| view.abs_index_from_page_cursor(p,c))
                                                    .unwrap_or_else(|| 0)
                                            }
                                        }
                                    };
                                    return Ok(items.get(abs).cloned());
                                }
                                KeyCode::Backspace => {
                                    last_abs_highlight =
                                        view.abs_index_from_page_cursor(st.page, st.cursor);
                                    query.pop();
                                    reposition_after_filter(items, &mut view, &mut st, &query, last_abs_highlight);
                                    caret_visible = true;
                                    last_blink = Instant::now();
                                    redraw_main(
                                        &mut stdout, w, h, panel_h, top_margin, &st, &view, items,
                                        mode, &query, caret_visible, input_pos,
                                    )?;
                                }
                                // 方向键与 Ctrl+N/P 移动/翻页
                                KeyCode::Left => {
                                    st.page_left(&view);
                                    redraw_main(&mut stdout, w, h, panel_h, top_margin, &st, &view, items, mode, &query, caret_visible, input_pos)?;
                                }
                                KeyCode::Right => {
                                    st.page_right(&view);
                                    redraw_main(&mut stdout, w, h, panel_h, top_margin, &st, &view, items, mode, &query, caret_visible, input_pos)?;
                                }
                                KeyCode::Up => {
                                    st.move_up(&view);
                                    redraw_main(&mut stdout, w, h, panel_h, top_margin, &st, &view, items, mode, &query, caret_visible, input_pos)?;
                                }
                                KeyCode::Down => {
                                    st.move_down(&view);
                                    redraw_main(&mut stdout, w, h, panel_h, top_margin, &st, &view, items, mode, &query, caret_visible, input_pos)?;
                                }
                                KeyCode::Char('n') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                                    st.move_down(&view);
                                    redraw_main(&mut stdout, w, h, panel_h, top_margin, &st, &view, items, mode, &query, caret_visible, input_pos)?;
                                }
                                KeyCode::Char('p') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                                    st.move_up(&view);
                                    redraw_main(&mut stdout, w, h, panel_h, top_margin, &st, &view, items, mode, &query, caret_visible, input_pos)?;
                                }
                                KeyCode::Home => {
                                    st.page = 1; st.cursor = 0;
                                    redraw_main(&mut stdout, w, h, panel_h, top_margin, &st, &view, items, mode, &query, caret_visible, input_pos)?;
                                }
                                KeyCode::End => {
                                    st.page = view.page_count();
                                    st.cursor = view.page_len(st.page).saturating_sub(1);
                                    redraw_main(&mut stdout, w, h, panel_h, top_margin, &st, &view, items, mode, &query, caret_visible, input_pos)?;
                                }
                                KeyCode::Char(c) => {
                                    // 字符都加入查询（包含 j/k/p/n/q/数字）
                                    if !c.is_control() {
                                        last_abs_highlight =
                                            view.abs_index_from_page_cursor(st.page, st.cursor);
                                        query.push(c);
                                        reposition_after_filter(items, &mut view, &mut st, &query, last_abs_highlight);
                                        caret_visible = true;
                                        last_blink = Instant::now();
                                        redraw_main(
                                            &mut stdout, w, h, panel_h, top_margin, &st, &view,
                                            items, mode, &query, caret_visible, input_pos,
                                        )?;
                                    }
                                }
                                _ => {}
                            }
                        }
                        Mode::Normal => {
                            if let KeyCode::Char('h') = k.code {
                                mode = Mode::Help;
                                redraw_help(&mut stdout, w, h)?;
                                continue;
                            }
                            if let KeyCode::Char('i') = k.code {
                                mode = Mode::Search;
                                query.clear();
                                view.apply_filter(items, &query);
                                st.reset_pages(view.page_count());
                                caret_visible = true;
                                last_blink = Instant::now();
                                redraw_main(
                                    &mut stdout, w, h, panel_h, top_margin, &st, &view, items,
                                    mode, &query, caret_visible, input_pos,
                                )?;
                                continue;
                            }

                            let input = map_key_normal(k);
                            match apply_input_normal(&mut st, input, &view) {
                                Step::None => {}
                                Step::Quit => return Ok(None),
                                Step::SelectAbs(abs) => {
                                    return Ok(items.get(abs).cloned());
                                }
                            }
                            redraw_main(
                                &mut stdout, w, h, panel_h, top_margin, &st, &view, items, mode,
                                &query, caret_visible, input_pos,
                            )?;
                        }
                    }
                }
                Event::Mouse(me) if mouse_enabled() => {
                    if mode == Mode::Help { continue; }
                    seen_key = true;
                    idle_since = Instant::now();

                    let header_extra = if mode == Mode::Search && input_pos == InputPos::Top { 1 } else { 0 };
                    if let Some(action) =
                        handle_mouse(me, top_margin, panel_h, header_extra, &st, &view)
                    {
                        match action {
                            MouseAction::MoveToCursor(new_cursor) => {
                                st.cursor = new_cursor.min(view.page_len(st.page).saturating_sub(1));

                                if let Some(abs) =
                                    view.abs_index_from_page_cursor(st.page, st.cursor)
                                {
                                    let now = Instant::now();
                                    let is_double = last_click_abs == Some(abs)
                                        && last_click_at
                                            .map(|t| now.duration_since(t)
                                                <= Duration::from_millis(DOUBLE_CLICK_MS))
                                            .unwrap_or(false);
                                    if is_double {
                                        return Ok(items.get(abs).cloned());
                                    } else {
                                        last_click_abs = Some(abs);
                                        last_click_at = Some(now);
                                    }
                                }
                                redraw_main(&mut stdout, w, h, panel_h, top_margin, &st, &view, items, mode, &query, caret_visible, input_pos)?;
                            }
                            MouseAction::ScrollUp => {
                                st.move_up(&view);
                                redraw_main(&mut stdout, w, h, panel_h, top_margin, &st, &view, items, mode, &query, caret_visible, input_pos)?;
                            }
                            MouseAction::ScrollDown => {
                                st.move_down(&view);
                                redraw_main(&mut stdout, w, h, panel_h, top_margin, &st, &view, items, mode, &query, caret_visible, input_pos)?;
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

// ---------------- 视图 & 状态 ----------------
#[derive(Clone, Debug)]
struct View {
    total_len: usize,
    filtered: Option<Vec<usize>>,
}
impl View {
    fn new(total_len: usize) -> Self {
        Self { total_len, filtered: None }
    }
    fn clear_filter(&mut self, total_len: usize) {
        self.total_len = total_len;
        self.filtered = None;
    }
    fn view_len(&self) -> usize {
        self.filtered.as_ref().map(|v| v.len()).unwrap_or(self.total_len)
    }
    fn page_count(&self) -> usize {
        ((self.view_len() + PER_PAGE - 1) / PER_PAGE).max(1)
    }
    fn page_len(&self, page: usize) -> usize {
        let start = (page - 1) * PER_PAGE;
        let end = (start + PER_PAGE).min(self.view_len());
        end - start
    }
    fn get_abs_indices_on_page(&self, page: usize) -> Vec<usize> {
        let start = (page - 1) * PER_PAGE;
        let end = (start + PER_PAGE).min(self.view_len());
        if let Some(map) = &self.filtered {
            map[start..end].to_vec()
        } else {
            (start..end).collect()
        }
    }
    fn abs_index_from_page_cursor(&self, page: usize, cursor: usize) -> Option<usize> {
        let start = (page - 1) * PER_PAGE;
        let idx = start + cursor;
        if idx >= self.view_len() { return None; }
        if let Some(map) = &self.filtered { Some(map[idx]) } else { Some(idx) }
    }
    fn apply_filter(&mut self, items: &[String], q: &str) {
        self.total_len = items.len();
        let q = q.trim();
        if q.is_empty() {
            self.filtered = None;
            return;
        }
        let q_lower = q.to_lowercase();
        let mut out = Vec::with_capacity(items.len());
        for (i, s) in items.iter().enumerate() {
            if s.to_lowercase().contains(&q_lower) {
                out.push(i);
            }
        }
        self.filtered = Some(out);
    }

    /// 返回某个“绝对索引”在过滤视图中的（全局序号 -> 页/光标）
    fn pos_of_abs(&self, abs: usize) -> Option<(usize, usize)> {
        let idx = if let Some(map) = &self.filtered {
            map.iter().position(|&a| a == abs)?
        } else if abs < self.total_len {
            abs
        } else {
            return None;
        };
        let page = idx / PER_PAGE + 1;
        let cursor = idx % PER_PAGE;
        Some((page, cursor))
    }

    /// 基于查询，在当前视图中找一个“最佳焦点”（精确匹配>前缀匹配>0号）
    fn best_focus(&self, items: &[String], q: &str) -> Option<(usize, usize)> {
        if self.view_len() == 0 { return None; }
        let ql = q.to_lowercase();

        // 遍历过滤后的全局序号
        let iter: Box<dyn Iterator<Item = (usize, usize)>> = if let Some(map) = &self.filtered {
            Box::new(map.iter().enumerate().map(|(i, &abs)| (i, abs)))
        } else {
            Box::new((0..self.total_len).enumerate().map(|(i, abs)| (i, abs)))
        };

        let mut exact: Option<usize> = None;
        let mut prefix: Option<usize> = None;

        for (i, abs) in iter {
            let s = &items[abs];
            let sl = s.to_lowercase();
            if exact.is_none() && sl == ql { exact = Some(i); }
            if prefix.is_none() && sl.starts_with(&ql) { prefix = Some(i); }
            if exact.is_some() && prefix.is_some() { break; }
        }

        let pick = exact.or(prefix).unwrap_or(0);
        let page = pick / PER_PAGE + 1;
        let cursor = pick % PER_PAGE;
        Some((page, cursor))
    }
}

#[derive(Clone, Debug)]
struct State {
    pages: usize,
    page: usize,   // 1-based
    cursor: usize, // 0..PER_PAGE-1
}
impl State {
    fn new(pages: usize) -> Self {
        Self { pages, page: 1, cursor: 0 }
    }
    fn reset_pages(&mut self, pages: usize) {
        self.pages = pages.max(1);
        self.page = self.page.min(self.pages).max(1);
        self.cursor = self.cursor.min(PER_PAGE - 1);
    }
    fn clamp_cursor_on_resize(&mut self, view: &View) {
        if self.cursor >= view.page_len(self.page) {
            self.cursor = view.page_len(self.page).saturating_sub(1);
        }
    }
    fn move_up(&mut self, view: &View) {
        if self.cursor > 0 {
            self.cursor -= 1;
        } else if self.page > 1 {
            self.page -= 1;
            self.cursor = view.page_len(self.page).saturating_sub(1);
        }
    }
    fn move_down(&mut self, view: &View) {
        if self.cursor + 1 < view.page_len(self.page) {
            self.cursor += 1;
        } else if self.page < self.pages {
            self.page += 1;
            self.cursor = 0;
        }
    }
    fn page_left(&mut self, view: &View) {
        if self.page > 1 {
            self.page -= 1;
            self.cursor = self.cursor.min(view.page_len(self.page).saturating_sub(1));
        }
    }
    fn page_right(&mut self, view: &View) {
        if self.page < self.pages {
            self.page += 1;
            self.cursor = self.cursor.min(view.page_len(self.page).saturating_sub(1));
        }
    }
}

// 过滤后决定光标所在（粘性焦点 + 最佳焦点）
fn reposition_after_filter(
    items: &[String],
    view: &mut View,
    st: &mut State,
    q: &str,
    anchor_abs: Option<usize>,
) {
    view.apply_filter(items, q);
    st.reset_pages(view.page_count());

    // 优先：粘性焦点（原高亮仍存在）
    if let Some(abs) = anchor_abs {
        if let Some((p, c)) = view.pos_of_abs(abs) {
            st.page = p;
            st.cursor = c;
            return;
        }
    }
    // 其次：最佳焦点（精确>前缀>第 0 个）
    if let Some((p, c)) = view.best_focus(items, q) {
        st.page = p;
        st.cursor = c;
    } else {
        st.page = 1;
        st.cursor = 0;
    }
}

// ---------------- 键盘映射（Normal 模式） ----------------
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Step {
    None,
    Quit,
    SelectAbs(usize),
}
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Input {
    Up, Down, Left, Right,
    Digit(char),
    Backspace, Enter, Esc, CtrlC,
    Char(char),
}

fn map_key_normal(k: crossterm::event::KeyEvent) -> Input {
    if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c') {
        return Input::CtrlC;
    }
    match k.code {
        KeyCode::Esc => Input::Esc,
        KeyCode::Enter => Input::Enter,
        KeyCode::Up | KeyCode::Char('k') => Input::Up,
        KeyCode::Down | KeyCode::Char('j') => Input::Down,
        KeyCode::Left | KeyCode::Char('p') => Input::Left,
        KeyCode::Right | KeyCode::Char('n') => Input::Right,
        KeyCode::Backspace => Input::Backspace,
        KeyCode::Char(c) if c.is_ascii_digit() => Input::Digit(c),
        KeyCode::Char(c) => Input::Char(c),
        _ => Input::Backspace,
    }
}
fn apply_input_normal(st: &mut State, input: Input, view: &View) -> Step {
    match input {
        Input::Char('q') => return Step::Quit,
        Input::Enter => {
            if let Some(abs) = view.abs_index_from_page_cursor(st.page, st.cursor) {
                return Step::SelectAbs(abs);
            }
            return Step::None;
        }
        Input::Up => st.move_up(view),
        Input::Down => st.move_down(view),
        Input::Left => st.page_left(view),
        Input::Right => st.page_right(view),
        Input::Digit(c) => {
            let idx = (c as u8 - b'0') as usize;
            if idx < view.page_len(st.page) {
                st.cursor = idx;
            }
        }
        _ => {}
    }
    Step::None
}

// ---------------- 鼠标 ----------------
#[derive(Copy, Clone, Debug)]
enum MouseAction {
    MoveToCursor(usize),
    ScrollUp,
    ScrollDown,
}
fn handle_mouse(
    me: MouseEvent,
    top_margin: u16,
    panel_h: u16,
    header_extra: u16,
    st: &State,
    view: &View,
) -> Option<MouseAction> {
    let content_top = top_margin + 1 + header_extra;
    let inner_rows = (panel_h - 2 - header_extra) as u16;
    let content_bottom = content_top + inner_rows.saturating_sub(1);

    match me.kind {
        MouseEventKind::ScrollUp => return Some(MouseAction::ScrollUp),
        MouseEventKind::ScrollDown => return Some(MouseAction::ScrollDown),
        MouseEventKind::Down(MouseButton::Right) => return None,
        MouseEventKind::Down(MouseButton::Left) => {
            let row = me.row;
            if row >= content_top && row <= content_bottom {
                let idx_in_page = (row - content_top) as usize;
                let page_len = view.page_len(st.page);
                if idx_in_page < page_len {
                    return Some(MouseAction::MoveToCursor(idx_in_page));
                }
            }
        }
        _ => {}
    }
    None
}

// ---------------- 绘制 ----------------
fn compute_top_margin_bottom(h: u16, panel_h: u16) -> u16 {
    h.saturating_sub(panel_h).saturating_sub(1)
}
fn redraw_main(
    stdout: &mut Stdout,
    w: u16, _h: u16, panel_h: u16, top_margin: u16,
    st: &State, view: &View, items: &[String], mode: Mode,
    query: &str, caret_visible: bool, input_pos: InputPos,
) -> io::Result<()> {
    stdout.queue(Clear(ClearType::All))?.queue(MoveTo(0, 0))?;
    let inner_width = w.saturating_sub(2) as usize;

    // 顶栏
    stdout.queue(MoveTo(0, top_margin))?.queue(Print("╭"))?;
    if color_enabled() {
        stdout.queue(SetForegroundColor(Color::Cyan))?.queue(SetAttribute(Attribute::Bold))?;
    }
    let title = match mode {
        Mode::Search => format!(
            " cdh • 搜索 {}/{} 条 • 第 {}/{} 页 ",
            view.view_len(), items.len(), st.page, view.page_count()
        ),
        _ => format!(" cdh • 第 {}/{} 页 • 共 {} 条 ", st.page, view.page_count(), view.view_len()),
    };
    let title_line = if mode == Mode::Search && input_pos == InputPos::Title {
        let caret = if caret_visible { "▌" } else { " " };
        let prompt = format!("  搜索: {}{}", query, caret);
        pad(&(title + &prompt), inner_width, '─')
    } else {
        pad(&title, inner_width, '─')
    };
    stdout.queue(Print(title_line))?;
    if color_enabled() { stdout.queue(ResetColor)?.queue(SetAttribute(Attribute::Reset))?; }
    stdout.queue(Print("╮"))?;

    // 顶部输入（可选）
    let mut header_extra_lines: u16 = 0;
    if mode == Mode::Search && input_pos == InputPos::Top {
        header_extra_lines = 1;
        let caret = if caret_visible { "▌" } else { " " };
        let prompt = format!(" 搜索: {}{}", query, caret);
        let pad_width = inner_width.saturating_sub(display_width(&prompt));
        stdout.queue(MoveTo(0, top_margin + 1))?.queue(Print("│"))?;
        if color_enabled() { stdout.queue(SetForegroundColor(Color::Yellow))?.queue(SetAttribute(Attribute::Bold))?; }
        stdout.queue(Print(prompt))?;
        if pad_width > 0 { stdout.queue(Print(" ".repeat(pad_width)))?; }
        if color_enabled() { stdout.queue(ResetColor)?.queue(SetAttribute(Attribute::Reset))?; }
        stdout.queue(Print("│"))?;
    }

    // 内容
    let abs_indices = view.get_abs_indices_on_page(st.page);
    let content_lines = (panel_h - 2 - header_extra_lines) as usize;
    let content_start_row = top_margin + 1 + header_extra_lines;

    for i in 0..content_lines {
        let row = content_start_row + i as u16;
        stdout.queue(MoveTo(0, row))?.queue(Print("│"))?;

        let txt = if i < abs_indices.len() {
            format!(" {} ) {}", i, items[abs_indices[i]])
        } else {
            format!(" {} ) ", i)
        };

        let txt_width = display_width(&txt);
        let pad_width = inner_width.saturating_sub(txt_width);

        if i == st.cursor && i < abs_indices.len() {
            if color_enabled() {
                stdout
                    .queue(SetBackgroundColor(Color::DarkBlue))?
                    .queue(SetForegroundColor(Color::White))?
                    .queue(SetAttribute(Attribute::Bold))?;
                stdout.queue(Print(&txt))?;
                if pad_width > 0 { stdout.queue(Print(" ".repeat(pad_width)))?; }
                stdout.queue(ResetColor)?.queue(SetAttribute(Attribute::Reset))?;
            } else {
                stdout
                    .queue(SetAttribute(Attribute::Reverse))?
                    .queue(Print(&txt))?
                    .queue(Print(" ".repeat(pad_width)))?
                    .queue(SetAttribute(Attribute::Reset))?;
            }
        } else {
            stdout.queue(Print(&txt))?;
            if pad_width > 0 { stdout.queue(Print(" ".repeat(pad_width)))?; }
        }
        stdout.queue(Print("│"))?;
    }

    // 底栏
    let bottom_row = top_margin + panel_h - 1;
    stdout.queue(MoveTo(0, bottom_row))?.queue(Print("╰"))?;
    match mode {
        Mode::Search => match input_pos {
            InputPos::Bottom => {
                let caret = if caret_visible { "▌" } else { " " };
                let prompt = format!(" 搜索: {}{}  · Esc 返回 · Enter/Tab 选 ", query, caret);
                let pad_width = inner_width.saturating_sub(display_width(&prompt));
                if color_enabled() { stdout.queue(SetForegroundColor(Color::Yellow))?.queue(SetAttribute(Attribute::Bold))?; }
                stdout.queue(Print(prompt))?;
                if pad_width > 0 { stdout.queue(Print(" ".repeat(pad_width)))?; }
                if color_enabled() { stdout.queue(ResetColor)?; }
            }
            _ => {
                let tip = " Esc 返回 · Enter/Tab 选 ";
                let pad_width = inner_width.saturating_sub(display_width(tip));
                if color_enabled() { stdout.queue(SetForegroundColor(Color::DarkGrey))?.queue(SetAttribute(Attribute::Bold))?; }
                stdout.queue(Print(tip))?;
                if pad_width > 0 { stdout.queue(Print(" ".repeat(pad_width)))?; }
                if color_enabled() { stdout.queue(ResetColor)?; }
            }
        },
        _ => {
            let prompt = " Enter 选 · q 退出 · h 帮助 · i 搜索 ";
            let pad_width = inner_width.saturating_sub(display_width(prompt));
            if color_enabled() { stdout.queue(SetForegroundColor(Color::DarkGrey))?.queue(SetAttribute(Attribute::Bold))?; }
            stdout.queue(Print(prompt))?;
            if pad_width > 0 { stdout.queue(Print(" ".repeat(pad_width)))?; }
            if color_enabled() { stdout.queue(ResetColor)?; }
        }
    }
    stdout.queue(Print("╯"))?;

    // 浮层输入
    if mode == Mode::Search && input_pos == InputPos::Overlay {
        draw_overlay_input(stdout, w, top_margin, panel_h, query, caret_visible)?;
    }

    stdout.flush()?;
    Ok(())
}

fn draw_overlay_input(
    stdout: &mut Stdout,
    w: u16,
    top_margin: u16,
    panel_h: u16,
    q: &str,
    caret_visible: bool,
) -> io::Result<()> {
    let caret = if caret_visible { "▌" } else { " " };
    let text = format!(" 搜索: {}{}", q, caret);
    let width = (display_width(&text) + 4).min(w as usize - 4);
    let bw = width as u16;

    let left = w.saturating_sub(bw).saturating_sub(2);
    let mut top = top_margin.saturating_sub(3);
    if top < 1 {
        top = top_margin + panel_h + 1;
    }

    stdout
        .queue(MoveTo(left, top))?
        .queue(Print("┌"))?
        .queue(Print("─".repeat(width - 2)))?
        .queue(Print("┐"))?;
    stdout
        .queue(MoveTo(left, top + 1))?
        .queue(Print("│"))?
        .queue(Print(pad(&text, width - 2, ' ')))?
        .queue(Print("│"))?;
    stdout
        .queue(MoveTo(left, top + 2))?
        .queue(Print("└"))?
        .queue(Print("─".repeat(width - 2)))?
        .queue(Print("┘"))?;
    Ok(())
}

fn redraw_help(stdout: &mut Stdout, w: u16, h: u16) -> io::Result<()> {
    stdout.queue(Clear(ClearType::All))?;

    let lines = [
        "帮助",
        "",
        "主界面：",
        "  ↑/↓ 或 k/j       移动光标（越界翻页）",
        "  ←/→ 或 p/n       翻页",
        "  0..9             数字直达本页索引",
        "  Enter            选中并退出",
        "  q                退出程序",
        "  i                进入搜索模式",
        "  h                打开帮助（q/ESC 关闭）",
        "",
        "搜索模式：",
        "  输入任意字符（含 j/k/p/n/q/数字）进行过滤",
        "  ↑/↓/←/→         移动与翻页（支持 Ctrl+N / Ctrl+P）",
        "  Enter/Tab       选中（单结果直接选中；无结果 Beep）",
        "  Esc             返回主界面",
        "",
        "鼠标：左键单击移动，双击选中（300ms），滚轮滚动",
    ];

    let width = lines.iter().map(|s| display_width(s)).max().unwrap_or(10) + 4;
    let width = width.min(w as usize - 4);
    let height = lines.len() as u16 + 2;
    let box_w = width as u16;
    let box_h = height;
    let left = (w.saturating_sub(box_w)) / 2;
    let top = (h.saturating_sub(box_h)) / 2;

    stdout
        .queue(MoveTo(left, top))?
        .queue(Print("┌"))?
        .queue(Print("─".repeat(width - 2)))?
        .queue(Print("┐"))?;
    for i in 1..(box_h - 1) {
        stdout
            .queue(MoveTo(left, top + i))?
            .queue(Print("│"))?
            .queue(Print(" ".repeat(width - 2)))?
            .queue(Print("│"))?;
    }
    stdout
        .queue(MoveTo(left, top + box_h - 1))?
        .queue(Print("└"))?
        .queue(Print("─".repeat(width - 2)))?
        .queue(Print("┘"))?;
    for (i, line) in lines.iter().enumerate() {
        stdout.queue(MoveTo(left + 2, top + 1 + i as u16))?.queue(Print(*line))?;
    }
    stdout.flush()?;
    Ok(())
}

// ---------------- 文本与小工具 ----------------
fn display_width(s: &str) -> usize { UnicodeWidthStr::width(s) }
fn pad(s: &str, width: usize, fill: char) -> String {
    let w = display_width(s);
    if w >= width { trim_mid(s, width) } else { format!("{s}{}", fill.to_string().repeat(width - w)) }
}
fn trim_mid(s: &str, width: usize) -> String {
    if width < 3 { return "…".repeat(width); }
    let left = (width - 1) / 2;
    let right = width - 1 - left;
    let mut l = String::new(); let mut w = 0;
    for ch in s.chars() { let cw = display_width(&ch.to_string()); if w+cw>left { break } w+=cw; l.push(ch); }
    let mut r = String::new(); w = 0;
    for ch in s.chars().rev() { let cw = display_width(&ch.to_string()); if w+cw>right { break } w+=cw; r.insert(0,ch); }
    format!("{l}…{r}")
}
fn ensure(cond: bool, msg: &str) -> io::Result<()> {
    if !cond { Err(io::Error::new(io::ErrorKind::Other, msg.to_string())) } else { Ok(()) }
}
fn beep(stdout: &mut Stdout) -> io::Result<()> {
    stdout.queue(Print("\x07"))?.flush()?;
    Ok(())
}
