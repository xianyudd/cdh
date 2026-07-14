//! Keyboard-first interactive directory picker.
//!
//! The picker keeps ranking and filesystem work outside the rendering path:
//! filtering happens on input, preview I/O runs on a dedicated worker, and
//! drawing only formats the current page of already prepared data.

#[path = "picker_i18n.rs"]
mod i18n;
#[path = "tui_settings.rs"]
mod settings;

use std::cell::Cell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::env;
use std::fs;
use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossterm::{
    cursor::{Hide, Show},
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
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
    Frame, Terminal,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::recommend::Recommendation;
use crate::{history, AppContext, Paths};
#[cfg(test)]
use i18n::resolve_language;
use i18n::{detect_locale_language, Language, TextKey};
use settings::{
    LanguagePreference, SettingKey, SettingsLoad, UiEnvironment, UiPreferences, UiSettings,
};

const MIN_HEIGHT: u16 = 8;
const DOUBLE_CLICK_MS: u128 = 300;
const PREVIEW_ENTRY_LIMIT: usize = 16;
const PREVIEW_CACHE_LIMIT: usize = 50;
const PREVIEW_DEBOUNCE: Duration = Duration::from_millis(100);
const PREVIEW_SIDE_MIN_WIDTH: u16 = 108;
const PREVIEW_BOTTOM_MIN_WIDTH: u16 = 70;
const PREVIEW_BOTTOM_MIN_HEIGHT: u16 = 18;
const GIT_DIRTY_TIMEOUT: Duration = Duration::from_millis(300);
const EVENT_POLL_FALLBACK: Duration = Duration::from_millis(100);

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

fn start_preview_worker(language: Language) -> PreviewWorker {
    let (request_tx, request_rx) = mpsc::channel::<PreviewRequest>();
    let (response_tx, response_rx) = mpsc::channel::<PreviewResponse>();

    thread::spawn(move || {
        while let Ok(request) = request_rx.recv() {
            let outcome = load_preview(&request.path, language);
            let response = PreviewResponse {
                path: request.path,
                generation: request.generation,
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

fn load_preview(path: &str, language: Language) -> PreviewOutcome {
    let path = Path::new(path);
    if !path.is_dir() {
        return PreviewOutcome::Missing;
    }

    match read_preview_entries(path) {
        Ok((entries, has_more_entries)) => PreviewOutcome::Data(PreviewData {
            git: read_git_info(path),
            entries,
            has_more_entries,
        }),
        Err(error) => PreviewOutcome::Error(preview_error_message(&error, language)),
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
        entries.push(PreviewEntry {
            name: entry.file_name().to_string_lossy().into_owned(),
            is_dir: entry.file_type()?.is_dir(),
        });
    }

    entries.sort_by(|left, right| {
        right
            .is_dir
            .cmp(&left.is_dir)
            .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
            .then_with(|| left.name.cmp(&right.name))
    });
    Ok((entries, has_more_entries))
}

fn read_git_info(path: &Path) -> Option<GitInfo> {
    let (repo_root, git_dir) = find_git_repo(path)?;
    let head = fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let branch = parse_git_head_branch(&head)?;
    Some(GitInfo {
        branch,
        dirty: read_git_dirty(&repo_root, GIT_DIRTY_TIMEOUT),
    })
}

/// Locate a repository without starting a `git` process. A `.git` directory is
/// the normal case; a `.git` file covers worktrees and submodules.
fn find_git_repo(start: &Path) -> Option<(PathBuf, PathBuf)> {
    for ancestor in start.ancestors() {
        let marker = ancestor.join(".git");
        if marker.is_dir() {
            return Some((ancestor.to_path_buf(), marker));
        }
        if marker.is_file() {
            let content = fs::read_to_string(&marker).ok()?;
            let target = content.trim().strip_prefix("gitdir: ")?;
            let target = Path::new(target);
            let git_dir = if target.is_absolute() {
                target.to_path_buf()
            } else {
                marker.parent()?.join(target)
            };
            if git_dir.is_dir() {
                return Some((ancestor.to_path_buf(), git_dir));
            }
        }
    }
    None
}

fn parse_git_head_branch(head: &str) -> Option<String> {
    let head = head.trim();
    if let Some(branch) = head.strip_prefix("ref: refs/heads/") {
        return Some(branch.to_string());
    }
    (!head.is_empty()).then(|| "detached".to_string())
}

fn read_git_dirty(repo_root: &Path, timeout: Duration) -> Option<bool> {
    let repo_root = repo_root.to_path_buf();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let dirty = Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(repo_root)
            .output()
            .ok()
            .and_then(|output| output.status.success().then_some(!output.stdout.is_empty()));
        let _ = tx.send(dirty);
    });
    rx.recv_timeout(timeout).ok().flatten()
}

fn preview_error_message(error: &io::Error, language: Language) -> String {
    match error.kind() {
        io::ErrorKind::PermissionDenied => language.text(TextKey::PermissionDenied).to_string(),
        io::ErrorKind::NotFound => language.text(TextKey::DirectoryMissing).to_string(),
        _ => error.to_string(),
    }
}

struct Theme {
    on: bool,
}

impl Theme {
    fn new(on: bool) -> Self {
        Self { on }
    }

    fn color(&self, red: u8, green: u8, blue: u8) -> Color {
        if self.on {
            Color::Rgb(red, green, blue)
        } else {
            Color::Reset
        }
    }

    fn border(&self) -> Style {
        Style::default().fg(self.color(0x51, 0x5f, 0x7d))
    }

    fn title(&self) -> Style {
        Style::default()
            .fg(self.color(0xe8, 0xee, 0xff))
            .add_modifier(Modifier::BOLD)
    }

    fn primary(&self) -> Style {
        Style::default().fg(self.color(0xd8, 0xe1, 0xf5))
    }

    fn dim(&self) -> Style {
        Style::default().fg(self.dim_color())
    }

    fn dim_color(&self) -> Color {
        self.color(0x7d, 0x89, 0xa6)
    }

    fn accent(&self) -> Style {
        Style::default().fg(self.color(0xa8, 0xb8, 0xff))
    }

    fn key_hint(&self) -> Style {
        self.accent().add_modifier(Modifier::BOLD)
    }

    fn match_color(&self) -> Color {
        self.color(0xc3, 0xe8, 0x8d)
    }

    fn warning_color(&self) -> Color {
        self.color(0xff, 0xcb, 0x6b)
    }

    fn warning(&self) -> Style {
        Style::default().fg(self.warning_color())
    }

    fn success_color(&self) -> Color {
        self.color(0x98, 0xc3, 0x79)
    }

    fn selected(&self) -> Style {
        if self.on {
            Style::default()
                .fg(self.color(0xf7, 0xf9, 0xff))
                .bg(self.color(0x35, 0x45, 0x6a))
        } else {
            Style::default().add_modifier(Modifier::REVERSED)
        }
    }

    fn selected_marker(&self) -> Style {
        let selected = self.selected().add_modifier(Modifier::BOLD);
        if self.on {
            selected.fg(self.match_color())
        } else {
            selected
        }
    }

    fn matched(&self, base: Style) -> Style {
        base.fg(self.match_color())
            .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
    }
}

/// Select one directory in an interactive terminal. Non-interactive callers
/// retain the established contract and receive the top recommendation.
pub fn pick(items: &[Recommendation]) -> io::Result<Option<String>> {
    if items.is_empty() {
        return Ok(None);
    }
    if !io::stderr().is_terminal() || !io::stdin().is_terminal() {
        return Ok(items.first().map(|item| item.path.clone()));
    }
    run_ui(items, None)
}

pub fn pick_with_history(ctx: &AppContext, items: &[Recommendation]) -> io::Result<Option<String>> {
    if items.is_empty() {
        return Ok(None);
    }
    if !io::stderr().is_terminal() || !io::stdin().is_terminal() {
        return Ok(items.first().map(|item| item.path.clone()));
    }
    run_ui(items, Some(ctx))
}

struct TermGuard {
    mouse: bool,
}

trait MouseCaptureControl {
    fn mouse_capture_enabled(&self) -> bool;
    fn set_mouse_capture(&mut self, enabled: bool) -> io::Result<()>;
}

impl TermGuard {
    fn enter(mouse: bool) -> io::Result<Self> {
        enable_raw_mode()?;
        let mut stderr = io::stderr();
        if let Err(error) = execute!(stderr, EnterAlternateScreen) {
            let _ = disable_raw_mode();
            return Err(error);
        }
        if let Err(error) = execute!(stderr, Hide) {
            let _ = execute!(stderr, Show, LeaveAlternateScreen);
            let _ = disable_raw_mode();
            return Err(error);
        }
        if mouse {
            if let Err(error) = execute!(stderr, EnableMouseCapture) {
                let _ = execute!(stderr, Show, LeaveAlternateScreen);
                let _ = disable_raw_mode();
                return Err(error);
            }
        }
        Ok(Self { mouse })
    }
}

impl MouseCaptureControl for TermGuard {
    fn mouse_capture_enabled(&self) -> bool {
        self.mouse
    }

    fn set_mouse_capture(&mut self, enabled: bool) -> io::Result<()> {
        if enabled == self.mouse {
            return Ok(());
        }

        let mut stderr = io::stderr();
        if enabled {
            execute!(stderr, EnableMouseCapture)?;
        } else {
            execute!(stderr, DisableMouseCapture)?;
        }
        self.mouse = enabled;
        Ok(())
    }
}

#[derive(Debug)]
enum MouseSettingError {
    TerminalTransition {
        requested: bool,
        error: io::Error,
    },
    Persistence {
        requested: bool,
        error: io::Error,
        rollback_error: Option<io::Error>,
        actual: bool,
    },
}

fn apply_mouse_setting<C: MouseCaptureControl>(
    settings: &mut UiSettings,
    candidate: UiPreferences,
    mouse: &mut C,
) -> Result<(), MouseSettingError> {
    let previous = mouse.mouse_capture_enabled();
    let requested = candidate.mouse;
    mouse
        .set_mouse_capture(requested)
        .map_err(|error| MouseSettingError::TerminalTransition { requested, error })?;

    if let Err(error) = settings.persist(candidate) {
        let rollback_error = mouse.set_mouse_capture(previous).err();
        return Err(MouseSettingError::Persistence {
            requested,
            error,
            rollback_error,
            actual: mouse.mouse_capture_enabled(),
        });
    }

    Ok(())
}

fn mouse_event_enabled<C: MouseCaptureControl>(mouse: &C, mode: Mode) -> bool {
    mouse.mouse_capture_enabled() && mode == Mode::Normal
}

impl Drop for TermGuard {
    fn drop(&mut self) {
        let mut stderr = io::stderr();
        if self.mouse {
            let _ = execute!(stderr, DisableMouseCapture);
        }
        let _ = execute!(stderr, Show, LeaveAlternateScreen);
        let _ = disable_raw_mode();
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PathDisplay {
    text: String,
    chars: Vec<char>,
    raw_ranges: Vec<std::ops::Range<usize>>,
    terminal_component: std::ops::Range<usize>,
}

impl PathDisplay {
    fn from_path(path: &str, home: Option<&str>) -> Self {
        let home = home
            .map(|value| value.trim_end_matches('/'))
            .filter(|value| !value.is_empty() && *value != "/");
        let home_char_count = home.map(|value| value.chars().count());
        let abbreviated_suffix = home.and_then(|home| {
            if path == home {
                Some("")
            } else {
                path.strip_prefix(home)
                    .filter(|suffix| suffix.starts_with('/'))
            }
        });

        let (text, raw_ranges) =
            if let (Some(suffix), Some(home_char_count)) = (abbreviated_suffix, home_char_count) {
                let mut text = String::from("~");
                let mut raw_ranges = Vec::with_capacity(suffix.chars().count() + 1);
                raw_ranges.push(0..home_char_count);
                for (offset, character) in suffix.chars().enumerate() {
                    text.push(character);
                    let raw_index = home_char_count + offset;
                    raw_ranges.push(raw_index..raw_index + 1);
                }
                (text, raw_ranges)
            } else {
                let chars = path.chars().collect::<Vec<_>>();
                let raw_ranges = (0..chars.len()).map(|index| index..index + 1).collect();
                (path.to_string(), raw_ranges)
            };
        let chars = text.chars().collect::<Vec<_>>();
        let terminal_component = terminal_component_range(&chars);
        debug_assert_eq!(chars.len(), raw_ranges.len());
        Self {
            text,
            chars,
            raw_ranges,
            terminal_component,
        }
    }

    fn display_highlight_indices(&self, raw_highlights: &[u32]) -> HashSet<usize> {
        let raw_highlights = raw_highlights
            .iter()
            .map(|index| *index as usize)
            .collect::<HashSet<_>>();
        self.raw_ranges
            .iter()
            .enumerate()
            .filter_map(|(display_index, raw_range)| {
                raw_range
                    .clone()
                    .any(|raw_index| raw_highlights.contains(&raw_index))
                    .then_some(display_index)
            })
            .collect()
    }
}

fn terminal_component_range(chars: &[char]) -> std::ops::Range<usize> {
    if chars.is_empty() {
        return 0..0;
    }
    let mut end = chars.len();
    while end > 1 && chars[end - 1] == '/' {
        end -= 1;
    }
    let start = chars[..end]
        .iter()
        .rposition(|character| *character == '/')
        .map_or(0, |index| (index + 1).min(end));
    if start == end {
        0..end
    } else {
        start..end
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PathPiece {
    Character(usize),
    Ellipsis,
}

fn visible_path_pieces(path: &PathDisplay, max_width: usize) -> Vec<PathPiece> {
    let full_width = UnicodeWidthStr::width(path.text.as_str());
    if full_width <= max_width {
        return (0..path.chars.len()).map(PathPiece::Character).collect();
    }
    if max_width == 0 {
        return Vec::new();
    }
    if max_width == 1 {
        return vec![PathPiece::Ellipsis];
    }

    let budget = max_width - 1;
    let terminal_width = path.chars[path.terminal_component.clone()]
        .iter()
        .map(|character| UnicodeWidthChar::width(*character).unwrap_or(0))
        .sum::<usize>();
    let right_budget = (budget / 2).max(terminal_width.min(budget));
    let left_budget = budget.saturating_sub(right_budget);
    let mut front = take_char_indices_front(&path.chars, left_budget);
    let back = take_char_indices_back(&path.chars, right_budget);
    if let Some(back_start) = back.first() {
        front.retain(|index| index < back_start);
    }

    front
        .into_iter()
        .map(PathPiece::Character)
        .chain(std::iter::once(PathPiece::Ellipsis))
        .chain(back.into_iter().map(PathPiece::Character))
        .collect()
}

#[cfg(test)]
fn visible_path_text(path: &PathDisplay, max_width: usize) -> String {
    visible_path_pieces(path, max_width)
        .into_iter()
        .map(|piece| match piece {
            PathPiece::Character(index) => path.chars[index],
            PathPiece::Ellipsis => '…',
        })
        .collect()
}

fn take_char_indices_front(chars: &[char], max_width: usize) -> Vec<usize> {
    let mut width = 0;
    let mut indices = Vec::new();
    for (index, character) in chars.iter().enumerate() {
        let character_width = UnicodeWidthChar::width(*character).unwrap_or(0);
        if width + character_width > max_width {
            break;
        }
        width += character_width;
        indices.push(index);
    }
    indices
}

fn take_char_indices_back(chars: &[char], max_width: usize) -> Vec<usize> {
    let mut width = 0;
    let mut indices = Vec::new();
    for (index, character) in chars.iter().enumerate().rev() {
        let character_width = UnicodeWidthChar::width(*character).unwrap_or(0);
        if width + character_width > max_width {
            break;
        }
        width += character_width;
        indices.push(index);
    }
    indices.reverse();
    indices
}

struct Candidate {
    raw: String,
    display: PathDisplay,
    name: String,
    score: f32,
    exists: bool,
    last_visit: Option<i64>,
}

#[cfg(test)]
fn build_candidates(items: &[Recommendation]) -> Vec<Candidate> {
    build_candidates_with_visits(items, &HashMap::new())
}

fn build_candidates_with_visits(
    items: &[Recommendation],
    last_visits: &HashMap<String, i64>,
) -> Vec<Candidate> {
    let home = env::var("HOME").ok().filter(|home| !home.is_empty());
    items
        .iter()
        .map(|item| Candidate {
            raw: item.path.clone(),
            name: directory_name(&item.path),
            display: PathDisplay::from_path(&item.path, home.as_deref()),
            score: item.score.clamp(0.0, 1.0) as f32,
            exists: item.exists,
            last_visit: last_visits.get(&item.path).copied(),
        })
        .collect()
}

fn directory_name(path: &str) -> String {
    Path::new(path)
        .file_name()
        .filter(|name| !name.is_empty())
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string())
}

fn load_last_visits(ctx: Option<&AppContext>) -> HashMap<String, i64> {
    let Some(ctx) = ctx else {
        return HashMap::new();
    };
    let mut visits: HashMap<String, i64> = HashMap::new();
    if let Ok(entries) = history::load_raw(ctx) {
        for entry in entries {
            let path = entry.path.to_string_lossy().into_owned();
            visits
                .entry(path)
                .and_modify(|time| *time = (*time).max(entry.ts_secs))
                .or_insert(entry.ts_secs);
        }
    }
    visits
}

struct Match {
    idx: usize,
    highlights: Vec<u32>,
}

struct Filter {
    matcher: Matcher,
    buffer: Vec<char>,
}

impl Filter {
    fn new() -> Self {
        Self {
            matcher: Matcher::new(Config::DEFAULT.match_paths()),
            buffer: Vec::new(),
        }
    }

    /// An empty query preserves recommendation order. Fuzzy matches rank by
    /// matcher quality, then the existing recommendation score; stale paths
    /// remain after valid paths so they can be cleaned up without competing
    /// with jump targets.
    fn run(&mut self, candidates: &[Candidate], query: &str) -> Vec<Match> {
        let query = query.trim();
        if query.is_empty() {
            return (0..candidates.len())
                .map(|idx| Match {
                    idx,
                    highlights: Vec::new(),
                })
                .collect();
        }

        let pattern = Pattern::parse(query, CaseMatching::Ignore, Normalization::Smart);
        let mut valid = Vec::new();
        let mut missing = Vec::new();

        for (idx, candidate) in candidates.iter().enumerate() {
            let mut highlight_buffer = Vec::new();
            let haystack = Utf32Str::new(&candidate.raw, &mut self.buffer);
            let Some(score) = pattern.score(haystack, &mut self.matcher) else {
                continue;
            };

            let mut highlights = Vec::new();
            let match_haystack = Utf32Str::new(&candidate.raw, &mut highlight_buffer);
            let _ = self.matcher.fuzzy_indices(
                match_haystack,
                Utf32Str::new(query, &mut Vec::new()),
                &mut highlights,
            );
            highlights.sort_unstable();
            highlights.dedup();

            if candidate.exists {
                valid.push((score, idx, highlights));
            } else {
                missing.push((idx, highlights));
            }
        }

        valid.sort_by(|left, right| {
            right.0.cmp(&left.0).then_with(|| {
                candidates[right.1]
                    .score
                    .partial_cmp(&candidates[left.1].score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
        });

        valid
            .into_iter()
            .map(|(_, idx, highlights)| Match { idx, highlights })
            .chain(
                missing
                    .into_iter()
                    .map(|(idx, highlights)| Match { idx, highlights }),
            )
            .collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PageWindow {
    start: usize,
    end: usize,
    page: usize,
    page_count: usize,
    page_size: usize,
}

impl PageWindow {
    fn new(total: usize, selected: usize, page_size: usize) -> Self {
        let page_size = page_size.max(1);
        if total == 0 {
            return Self {
                start: 0,
                end: 0,
                page: 0,
                page_count: 0,
                page_size,
            };
        }

        let selected = selected.min(total - 1);
        let page_count = total.div_ceil(page_size);
        let page = selected / page_size + 1;
        let start = (page - 1) * page_size;
        Self {
            start,
            end: (start + page_size).min(total),
            page,
            page_count,
            page_size,
        }
    }

    fn summary(self, total: usize, language: Language) -> String {
        language.page_summary(self.start, self.end, total, self.page, self.page_count)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Normal,
    Help,
    Settings { selected: usize },
    ConfirmDelete { candidate_idx: usize },
}

struct App {
    settings: UiSettings,
    language: Language,
    locale_language: Language,
    color_enabled: bool,
    pending_mouse_candidate: Option<UiPreferences>,
    candidates: Vec<Candidate>,
    filter: Filter,
    query: String,
    /// Unicode scalar-value offset in `query`, never a UTF-8 byte offset.
    query_cursor: usize,
    filtered_results: Vec<Match>,
    selected_index: usize,
    current_page: usize,
    page_size: usize,
    total_pages: usize,
    mode: Mode,
    notice: Option<String>,
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
    last_list_area: Cell<Rect>,
    last_list_start: Cell<usize>,
}

impl App {
    fn new(candidates: Vec<Candidate>, loaded: SettingsLoad, locale_language: Language) -> Self {
        let effective = loaded.settings.effective();
        let language = resolve_language_preference(effective.language, locale_language);
        let preview_visible = effective.preview;
        let preview_worker = preview_visible.then(|| start_preview_worker(language));
        Self::with_settings(candidates, loaded, locale_language, preview_worker)
    }

    #[cfg(test)]
    fn with_preview_worker(
        candidates: Vec<Candidate>,
        preview_worker: Option<PreviewWorker>,
        preview_visible: bool,
    ) -> Self {
        Self::with_preview_worker_language(
            candidates,
            preview_worker,
            preview_visible,
            Language::ZhCn,
        )
    }

    #[cfg(test)]
    fn with_preview_worker_language(
        candidates: Vec<Candidate>,
        preview_worker: Option<PreviewWorker>,
        preview_visible: bool,
        language: Language,
    ) -> Self {
        let language_preference = match language {
            Language::ZhCn => LanguagePreference::ZhCn,
            Language::En => LanguagePreference::En,
        };
        let settings = UiSettings::for_test(UiPreferences {
            language: language_preference,
            preview: preview_visible,
            ..UiPreferences::default()
        });
        Self::with_settings(
            candidates,
            SettingsLoad {
                settings,
                warning: None,
            },
            language,
            preview_worker,
        )
    }

    fn with_settings(
        candidates: Vec<Candidate>,
        loaded: SettingsLoad,
        locale_language: Language,
        preview_worker: Option<PreviewWorker>,
    ) -> Self {
        let effective = loaded.settings.effective();
        let language = resolve_language_preference(effective.language, locale_language);
        let mut filter = Filter::new();
        let filtered_results = filter.run(&candidates, "");
        let mut app = Self {
            settings: loaded.settings,
            language,
            locale_language,
            color_enabled: effective.color,
            pending_mouse_candidate: None,
            candidates,
            filter,
            query: String::new(),
            query_cursor: 0,
            filtered_results,
            selected_index: 0,
            current_page: 0,
            page_size: 1,
            total_pages: 0,
            mode: Mode::Normal,
            notice: None,
            preview_visible: effective.preview,
            preview_worker,
            preview_cache: HashMap::new(),
            preview_cache_order: VecDeque::new(),
            preview_generation: 0,
            preview_pending: None,
            preview_loading: None,
            preview_current: None,
            preview_selected_path: None,
            last_click: None,
            last_list_area: Cell::new(Rect::new(0, 0, 0, 0)),
            last_list_start: Cell::new(0),
        };
        app.notice = loaded.warning.map(|warning| {
            format!(
                "{}{warning}",
                app.language.text(TextKey::SettingsLoadFailedPrefix)
            )
        });
        app.sync_pagination();
        app
    }

    fn page(&self) -> PageWindow {
        let total = self.filtered_results.len();
        if total == 0 {
            return PageWindow::new(0, 0, self.page_size);
        }
        let start = self.current_page.saturating_sub(1) * self.page_size;
        PageWindow {
            start,
            end: (start + self.page_size).min(total),
            page: self.current_page,
            page_count: self.total_pages,
            page_size: self.page_size,
        }
    }

    fn sync_pagination(&mut self) {
        if self.filtered_results.is_empty() {
            self.selected_index = 0;
            self.current_page = 0;
            self.total_pages = 0;
            return;
        }

        self.selected_index = self.selected_index.min(self.filtered_results.len() - 1);
        let page = PageWindow::new(
            self.filtered_results.len(),
            self.selected_index,
            self.page_size,
        );
        self.current_page = page.page;
        self.total_pages = page.page_count;
    }

    fn set_page_size(&mut self, page_size: usize) -> bool {
        let page_size = page_size.max(1);
        if self.page_size == page_size {
            return false;
        }
        let selected_path = self.selected_raw();
        self.page_size = page_size;
        self.restore_selected_path(selected_path.as_deref());
        true
    }

    fn restore_selected_path(&mut self, path: Option<&str>) {
        if !self.filtered_results.is_empty() {
            if let Some(path) = path {
                if let Some(position) = self
                    .filtered_results
                    .iter()
                    .position(|matched| self.candidates[matched.idx].raw == path)
                {
                    self.selected_index = position;
                }
            }
            self.selected_index = self
                .selected_index
                .min(self.filtered_results.len().saturating_sub(1));
        }
        self.sync_pagination();
    }

    fn recompute_after_query_change(&mut self) {
        self.clamp_query_cursor();
        self.filtered_results = self.filter.run(&self.candidates, &self.query);
        self.selected_index = 0;
        self.sync_pagination();
        self.notice = None;
        self.invalidate_preview_selection();
    }

    fn query_char_count(&self) -> usize {
        self.query.chars().count()
    }

    fn clamp_query_cursor(&mut self) {
        self.query_cursor = self.query_cursor.min(self.query_char_count());
    }

    fn query_byte_index(&self, char_index: usize) -> usize {
        self.query
            .char_indices()
            .nth(char_index)
            .map(|(byte_index, _)| byte_index)
            .unwrap_or(self.query.len())
    }

    fn move_query_cursor(&mut self, delta: isize) -> bool {
        self.clamp_query_cursor();
        let next = if delta < 0 {
            self.query_cursor.saturating_sub((-delta) as usize)
        } else {
            self.query_cursor
                .saturating_add(delta as usize)
                .min(self.query_char_count())
        };
        if next == self.query_cursor {
            return false;
        }
        self.query_cursor = next;
        true
    }

    fn insert_query_char(&mut self, character: char) {
        self.clamp_query_cursor();
        let byte_index = self.query_byte_index(self.query_cursor);
        self.query.insert(byte_index, character);
        self.query_cursor += 1;
        self.recompute_after_query_change();
    }

    fn backspace_query(&mut self) -> bool {
        self.clamp_query_cursor();
        if self.query_cursor == 0 {
            return false;
        }
        let end = self.query_byte_index(self.query_cursor);
        let start = self.query_byte_index(self.query_cursor - 1);
        self.query.replace_range(start..end, "");
        self.query_cursor -= 1;
        self.recompute_after_query_change();
        true
    }

    fn delete_query_char(&mut self) -> bool {
        self.clamp_query_cursor();
        if self.query_cursor >= self.query_char_count() {
            return false;
        }
        let start = self.query_byte_index(self.query_cursor);
        let end = self.query_byte_index(self.query_cursor + 1);
        self.query.replace_range(start..end, "");
        self.recompute_after_query_change();
        true
    }

    fn clear_query(&mut self) -> bool {
        if self.query.is_empty() {
            self.query_cursor = 0;
            return false;
        }
        self.query.clear();
        self.query_cursor = 0;
        self.recompute_after_query_change();
        true
    }

    fn selected_candidate_idx(&self) -> Option<usize> {
        self.filtered_results
            .get(self.selected_index)
            .map(|matched| matched.idx)
    }

    fn selected_candidate(&self) -> Option<&Candidate> {
        self.selected_candidate_idx()
            .map(|idx| &self.candidates[idx])
    }

    fn selected_raw(&self) -> Option<String> {
        self.selected_candidate()
            .map(|candidate| candidate.raw.clone())
    }

    fn set_selected(&mut self, selected: usize) -> bool {
        if self.filtered_results.is_empty() {
            self.sync_pagination();
            return false;
        }
        let selected = selected.min(self.filtered_results.len() - 1);
        if self.selected_index == selected {
            return false;
        }
        self.selected_index = selected;
        self.sync_pagination();
        self.notice = None;
        self.invalidate_preview_selection();
        true
    }

    fn move_by(&mut self, delta: isize) -> bool {
        if delta < 0 {
            self.set_selected(self.selected_index.saturating_sub((-delta) as usize))
        } else {
            self.set_selected(self.selected_index.saturating_add(delta as usize))
        }
    }

    fn move_page(&mut self, delta: isize) -> bool {
        if self.filtered_results.is_empty() {
            return false;
        }
        let page = self.page();
        let current = self.current_page.saturating_sub(1) as isize;
        let target_page =
            (current + delta).clamp(0, page.page_count.saturating_sub(1) as isize) as usize;
        let row_in_page = self.selected_index.saturating_sub(page.start);
        self.set_selected(target_page * page.page_size + row_in_page)
    }

    fn move_home(&mut self) -> bool {
        self.set_selected(0)
    }

    fn move_end(&mut self) -> bool {
        self.set_selected(self.filtered_results.len().saturating_sub(1))
    }

    fn remove_candidate(&mut self, idx: usize) {
        if idx >= self.candidates.len() {
            self.notice = Some(self.language.text(TextKey::RecordMissing).to_string());
            return;
        }
        let selected = self.selected_index;
        self.candidates.remove(idx);
        self.filtered_results = self.filter.run(&self.candidates, &self.query);
        self.selected_index = selected.min(self.filtered_results.len().saturating_sub(1));
        self.sync_pagination();
        self.notice = Some(self.language.text(TextKey::HistoryDeleted).to_string());
        self.invalidate_preview_selection();
    }

    fn toggle_preview(&mut self) {
        self.preview_visible = !self.preview_visible;
        if self.preview_visible && self.preview_worker.is_none() {
            self.preview_worker = Some(start_preview_worker(self.language));
        }
        self.notice = None;
        self.invalidate_preview_selection();
    }

    fn edit_setting(&mut self, key: SettingKey, direction: isize) {
        let Some(candidate) = self.settings.candidate(key, direction) else {
            self.notice = Some(self.language.text(TextKey::SettingsLocked).to_string());
            return;
        };
        if key == SettingKey::Mouse {
            self.pending_mouse_candidate = Some(candidate);
            return;
        }

        let previous = self.settings.effective();
        match self.settings.persist(candidate) {
            Ok(()) => {
                let effective = self.settings.effective();
                self.apply_persisted_runtime(key, previous, effective);
                self.notice = Some(self.language.text(TextKey::SettingsSaved).to_string());
            }
            Err(error) => {
                self.notice = Some(format!(
                    "{}{error}",
                    self.language.text(TextKey::SettingsSaveFailedPrefix)
                ));
            }
        }
    }

    fn apply_persisted_runtime(
        &mut self,
        key: SettingKey,
        previous: UiPreferences,
        effective: UiPreferences,
    ) {
        match key {
            SettingKey::Language if previous.language != effective.language => {
                self.language =
                    resolve_language_preference(effective.language, self.locale_language);
                self.invalidate_language_preview();
            }
            SettingKey::Preview => {
                self.preview_visible = effective.preview;
                if self.preview_visible {
                    if self.preview_worker.is_none() {
                        self.preview_worker = Some(start_preview_worker(self.language));
                    }
                } else {
                    self.preview_worker = None;
                }
                self.invalidate_preview_selection();
            }
            SettingKey::Color => self.color_enabled = effective.color,
            SettingKey::Language | SettingKey::Mouse => {}
        }
    }

    fn invalidate_language_preview(&mut self) {
        self.preview_generation = self.preview_generation.saturating_add(1);
        self.preview_cache.clear();
        self.preview_cache_order.clear();
        self.invalidate_preview_selection();
        self.preview_worker = self
            .preview_visible
            .then(|| start_preview_worker(self.language));
    }

    fn apply_pending_mouse_setting<C: MouseCaptureControl>(&mut self, mouse: &mut C) {
        let Some(candidate) = self.pending_mouse_candidate.take() else {
            return;
        };
        match apply_mouse_setting(&mut self.settings, candidate, mouse) {
            Ok(()) => {
                let key = if candidate.mouse {
                    TextKey::MouseEnabled
                } else {
                    TextKey::MouseDisabled
                };
                self.notice = Some(self.language.text(key).to_string());
            }
            Err(MouseSettingError::TerminalTransition { requested, error }) => {
                self.notice = Some(format!(
                    "{}{} ({})",
                    self.language.text(TextKey::MouseTerminalFailedPrefix),
                    error,
                    mouse_state_label(self.language, requested)
                ));
            }
            Err(MouseSettingError::Persistence {
                requested,
                error,
                rollback_error,
                actual,
            }) => {
                let mut notice = format!(
                    "{}{} (requested {}, actual {})",
                    self.language.text(TextKey::MousePersistenceFailedPrefix),
                    error,
                    mouse_state_label(self.language, requested),
                    mouse_state_label(self.language, actual)
                );
                if let Some(rollback_error) = rollback_error {
                    notice.push_str(self.language.text(TextKey::MouseRollbackFailedPrefix));
                    notice.push_str(&rollback_error.to_string());
                }
                self.notice = Some(notice);
            }
        }
    }

    fn invalidate_preview_selection(&mut self) {
        self.preview_selected_path = None;
        self.preview_pending = None;
        self.preview_loading = None;
        self.preview_current = None;
    }

    /// Polls the worker and starts any debounced request. It returns whether
    /// visible state changed so the main loop can redraw without a frame clock.
    fn update_preview(&mut self, now: Instant) -> bool {
        if !self.preview_visible {
            return false;
        }
        let mut changed = self.poll_preview_results();
        changed |= self.track_preview_selection(now);
        changed |= self.maybe_send_preview_request(now);
        changed
    }

    fn preview_wait_timeout(&self, now: Instant) -> Duration {
        let Some((_, changed_at)) = &self.preview_pending else {
            return EVENT_POLL_FALLBACK;
        };
        PREVIEW_DEBOUNCE
            .checked_sub(now.saturating_duration_since(*changed_at))
            .unwrap_or_default()
            .min(EVENT_POLL_FALLBACK)
    }

    fn track_preview_selection(&mut self, now: Instant) -> bool {
        let selected = self
            .selected_candidate()
            .map(|candidate| (candidate.raw.clone(), candidate.exists));
        let selected_path = selected.as_ref().map(|(path, _)| path.clone());
        if self.preview_selected_path == selected_path {
            return false;
        }

        self.preview_selected_path = selected_path.clone();
        self.preview_pending = None;
        self.preview_loading = None;
        self.preview_current = None;

        let Some((path, exists)) = selected else {
            return true;
        };
        if !exists {
            self.preview_current = Some((path, PreviewOutcome::Missing));
            return true;
        }
        if let Some(cached) = self.preview_cache.get(&path).cloned() {
            self.preview_current = Some((path, cached));
        } else {
            self.preview_pending = Some((path, now));
        }
        true
    }

    fn maybe_send_preview_request(&mut self, now: Instant) -> bool {
        let Some((path, changed_at)) = self.preview_pending.clone() else {
            return false;
        };
        if now.saturating_duration_since(changed_at) < PREVIEW_DEBOUNCE {
            return false;
        }

        self.preview_pending = None;
        if self.preview_cache.contains_key(&path) {
            return false;
        }
        let Some(worker) = &self.preview_worker else {
            self.preview_current = Some((
                path,
                PreviewOutcome::Error(self.language.text(TextKey::PreviewUnavailable).to_string()),
            ));
            return true;
        };

        self.preview_generation = self.preview_generation.saturating_add(1);
        let generation = self.preview_generation;
        match worker.requests.send(PreviewRequest {
            path: path.clone(),
            generation,
        }) {
            Ok(()) => {
                self.preview_loading = Some(path);
                true
            }
            Err(_) => {
                self.preview_worker = None;
                self.preview_current = Some((
                    path,
                    PreviewOutcome::Error(
                        self.language.text(TextKey::PreviewUnavailable).to_string(),
                    ),
                ));
                true
            }
        }
    }

    fn poll_preview_results(&mut self) -> bool {
        let mut responses = Vec::new();
        let mut disconnected = false;
        if let Some(worker) = &self.preview_worker {
            loop {
                match worker.responses.try_recv() {
                    Ok(response) => responses.push(response),
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }
        }

        let mut changed = false;
        if disconnected {
            self.preview_worker = None;
            changed = true;
        }
        for response in responses {
            changed |= self.accept_preview_response(response);
        }
        changed
    }

    fn accept_preview_response(&mut self, response: PreviewResponse) -> bool {
        if response.generation != self.preview_generation
            || self.preview_selected_path.as_deref() != Some(response.path.as_str())
        {
            return false;
        }
        self.insert_preview_cache(response.path.clone(), response.outcome.clone());
        self.preview_loading = None;
        self.preview_current = Some((response.path, response.outcome));
        true
    }

    fn insert_preview_cache(&mut self, path: String, outcome: PreviewOutcome) {
        if !self.preview_cache.contains_key(&path) {
            self.preview_cache_order.push_back(path.clone());
        }
        self.preview_cache.insert(path, outcome);
        while self.preview_cache_order.len() > PREVIEW_CACHE_LIMIT {
            if let Some(oldest) = self.preview_cache_order.pop_front() {
                self.preview_cache.remove(&oldest);
            }
        }
    }
}

fn resolve_language_preference(
    preference: LanguagePreference,
    locale_language: Language,
) -> Language {
    match preference {
        LanguagePreference::Auto => locale_language,
        LanguagePreference::ZhCn => Language::ZhCn,
        LanguagePreference::En => Language::En,
    }
}

fn mouse_state_label(language: Language, enabled: bool) -> &'static str {
    match (language, enabled) {
        (Language::ZhCn, true) => "启用",
        (Language::ZhCn, false) => "禁用",
        (Language::En, true) => "enabled",
        (Language::En, false) => "disabled",
    }
}

fn run_ui(items: &[Recommendation], ctx: Option<&AppContext>) -> io::Result<Option<String>> {
    let last_visits = load_last_visits(ctx);
    let config_dir = ctx
        .map(|context| context.paths.config_dir.clone())
        .unwrap_or_else(|| Paths::from_env().config_dir);
    let loaded = UiSettings::load(config_dir.join("tui.toml"), UiEnvironment::from_process());
    let initial_mouse = loaded.settings.effective().mouse;
    let locale_language = detect_locale_language();
    let mut app = App::new(
        build_candidates_with_visits(items, &last_visits),
        loaded,
        locale_language,
    );
    let mut guard = TermGuard::enter(initial_mouse)?;
    let backend = CrosstermBackend::new(io::stderr());
    let mut terminal = Terminal::new(backend)?;
    let mut dirty = true;

    loop {
        let terminal_size = terminal.size()?;
        let terminal_area = Rect::new(0, 0, terminal_size.width, terminal_size.height);
        if app.set_page_size(page_size_for(terminal_area, app.preview_visible)) {
            dirty = true;
        }
        if dirty {
            terminal.draw(|frame| {
                let theme = Theme::new(app.color_enabled);
                draw(frame, &app, &theme);
            })?;
            dirty = false;
        }

        let now = Instant::now();
        if app.update_preview(now) {
            dirty = true;
            continue;
        }

        if !event::poll(app.preview_wait_timeout(now))? {
            continue;
        }

        match event::read()? {
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                let result = handle_key(&mut app, key.code, key.modifiers, ctx);
                app.apply_pending_mouse_setting(&mut guard);
                if let Some(result) = result {
                    return Ok(result);
                }
                dirty = true;
            }
            Event::Resize(_, _) => {
                dirty = true;
            }
            Event::Mouse(mouse_event) if mouse_event_enabled(&guard, app.mode) => {
                if let Some(result) = handle_mouse(&mut app, mouse_event) {
                    return Ok(result);
                }
                dirty = true;
            }
            _ => {}
        }
    }
}

fn handle_key(
    app: &mut App,
    code: KeyCode,
    modifiers: KeyModifiers,
    ctx: Option<&AppContext>,
) -> Option<Option<String>> {
    if modifiers.contains(KeyModifiers::CONTROL)
        && matches!(code, KeyCode::Char('c') | KeyCode::Char('g'))
    {
        return Some(None);
    }
    match app.mode {
        Mode::Normal => handle_key_normal(app, code, modifiers),
        Mode::Help => handle_key_help(app, code),
        Mode::Settings { selected } => handle_key_settings(app, code, selected),
        Mode::ConfirmDelete { candidate_idx } => {
            handle_key_confirm_delete(app, code, modifiers, ctx, candidate_idx)
        }
    }
}

fn handle_key_help(app: &mut App, code: KeyCode) -> Option<Option<String>> {
    app.mode = if code == KeyCode::F(2) {
        Mode::Settings { selected: 0 }
    } else {
        Mode::Normal
    };
    None
}

fn handle_key_settings(app: &mut App, code: KeyCode, selected: usize) -> Option<Option<String>> {
    const ROWS: [SettingKey; 4] = [
        SettingKey::Language,
        SettingKey::Preview,
        SettingKey::Color,
        SettingKey::Mouse,
    ];
    match code {
        KeyCode::F(2) | KeyCode::Esc => app.mode = Mode::Normal,
        KeyCode::Up => {
            app.mode = Mode::Settings {
                selected: selected.saturating_sub(1),
            };
        }
        KeyCode::Down => {
            app.mode = Mode::Settings {
                selected: selected.saturating_add(1).min(ROWS.len() - 1),
            };
        }
        KeyCode::Left => app.edit_setting(ROWS[selected.min(ROWS.len() - 1)], -1),
        KeyCode::Right | KeyCode::Enter | KeyCode::Char(' ') => {
            app.edit_setting(ROWS[selected.min(ROWS.len() - 1)], 1);
        }
        _ => {}
    }
    None
}

fn handle_key_confirm_delete(
    app: &mut App,
    code: KeyCode,
    modifiers: KeyModifiers,
    ctx: Option<&AppContext>,
    candidate_idx: usize,
) -> Option<Option<String>> {
    app.mode = Mode::Normal;
    if code != KeyCode::Char('d') || !modifiers.contains(KeyModifiers::CONTROL) {
        return None;
    }

    let Some(ctx) = ctx else {
        app.notice = Some(
            app.language
                .text(TextKey::HistoryWriteUnavailable)
                .to_string(),
        );
        return None;
    };
    let Some(path) = app
        .candidates
        .get(candidate_idx)
        .map(|candidate| candidate.raw.clone())
    else {
        app.notice = Some(app.language.text(TextKey::RecordMissing).to_string());
        return None;
    };
    match history::remove_path(ctx, &path) {
        Ok(()) => app.remove_candidate(candidate_idx),
        Err(error) => {
            app.notice = Some(format!(
                "{}{error}",
                app.language.text(TextKey::DeleteFailedPrefix)
            ));
        }
    }
    None
}

fn handle_key_normal(
    app: &mut App,
    code: KeyCode,
    modifiers: KeyModifiers,
) -> Option<Option<String>> {
    let ctrl = modifiers.contains(KeyModifiers::CONTROL);
    match code {
        KeyCode::Char('u') if ctrl => {
            app.clear_query();
        }
        KeyCode::Char('d') if ctrl => {
            if let Some(candidate_idx) = app.selected_candidate_idx() {
                app.mode = Mode::ConfirmDelete { candidate_idx };
            } else {
                app.notice = Some(app.language.text(TextKey::NoDeletableHistory).to_string());
            }
        }
        KeyCode::Enter => match app.selected_candidate() {
            Some(candidate) if candidate.exists => return Some(Some(candidate.raw.clone())),
            Some(_) => {
                app.notice = Some(app.language.text(TextKey::MissingDeleteHint).to_string());
            }
            None => app.notice = Some(app.language.text(TextKey::NoJumpTarget).to_string()),
        },
        KeyCode::Tab => app.toggle_preview(),
        KeyCode::F(1) | KeyCode::Char('?') | KeyCode::Char('？') => app.mode = Mode::Help,
        KeyCode::F(2) => app.mode = Mode::Settings { selected: 0 },
        KeyCode::Esc => {
            if app.preview_visible {
                app.toggle_preview();
            } else if !app.query.is_empty() {
                app.clear_query();
            } else {
                return Some(None);
            }
        }
        KeyCode::Up if ctrl => {
            app.move_page(-1);
        }
        KeyCode::Down if ctrl => {
            app.move_page(1);
        }
        KeyCode::Up => {
            app.move_by(-1);
        }
        KeyCode::Char('p') if ctrl => {
            app.move_by(-1);
        }
        KeyCode::Down => {
            app.move_by(1);
        }
        KeyCode::Char('n') if ctrl => {
            app.move_by(1);
        }
        KeyCode::PageUp => {
            app.move_page(-1);
        }
        KeyCode::PageDown => {
            app.move_page(1);
        }
        KeyCode::Left => {
            app.move_query_cursor(-1);
        }
        KeyCode::Right => {
            app.move_query_cursor(1);
        }
        KeyCode::Home => {
            app.move_home();
        }
        KeyCode::End => {
            app.move_end();
        }
        KeyCode::Backspace => {
            app.backspace_query();
        }
        KeyCode::Delete => {
            app.delete_query_char();
        }
        KeyCode::Char(character) if !ctrl && !character.is_control() => {
            app.insert_query_char(character);
        }
        _ => {}
    }
    None
}

fn handle_mouse(app: &mut App, event: event::MouseEvent) -> Option<Option<String>> {
    match event.kind {
        MouseEventKind::ScrollUp => {
            app.move_by(-1);
        }
        MouseEventKind::ScrollDown => {
            app.move_by(1);
        }
        MouseEventKind::Down(MouseButton::Left) => {
            let list_area = app.last_list_area.get();
            if list_area.height == 0
                || event.row < list_area.y
                || event.row >= list_area.y + list_area.height
                || event.column < list_area.x
                || event.column >= list_area.x + list_area.width
            {
                return None;
            }

            let selected = app.last_list_start.get() + (event.row - list_area.y) as usize;
            if selected >= app.filtered_results.len() {
                return None;
            }
            let now = Instant::now();
            let double_click = app
                .last_click
                .map(|(row, then)| {
                    row == selected
                        && now.saturating_duration_since(then).as_millis() <= DOUBLE_CLICK_MS
                })
                .unwrap_or(false);
            app.set_selected(selected);
            app.last_click = Some((selected, now));
            if double_click {
                match app.selected_candidate() {
                    Some(candidate) if candidate.exists => {
                        return Some(Some(candidate.raw.clone()))
                    }
                    Some(_) => {
                        app.notice =
                            Some(app.language.text(TextKey::MissingDeleteHint).to_string());
                    }
                    None => {}
                }
            }
        }
        _ => {}
    }
    None
}

#[derive(Clone, Copy)]
enum PreviewPlacement {
    Side,
    Bottom,
}

struct ScreenLayout {
    header: Rect,
    input: Rect,
    top_divider: Rect,
    content: Rect,
    bottom_divider: Rect,
    footer: Rect,
    list: Rect,
    preview: Option<(Rect, PreviewPlacement)>,
    preview_unavailable: bool,
}

fn screen_layout(full: Rect, preview_visible: bool) -> Option<ScreenLayout> {
    if full.width < 3 || full.height < MIN_HEIGHT {
        return None;
    }
    let inner = Rect::new(
        full.x + 1,
        full.y + 1,
        full.width.saturating_sub(2),
        full.height.saturating_sub(2),
    );
    let sections = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .split(inner);
    let content = sections[3];
    let (list, preview, preview_unavailable) = if preview_visible
        && full.width >= PREVIEW_SIDE_MIN_WIDTH
    {
        let columns = Layout::horizontal([Constraint::Percentage(58), Constraint::Percentage(42)])
            .split(content);
        (
            columns[0],
            Some((columns[1], PreviewPlacement::Side)),
            false,
        )
    } else if preview_visible
        && full.width >= PREVIEW_BOTTOM_MIN_WIDTH
        && full.height >= PREVIEW_BOTTOM_MIN_HEIGHT
        && content.height >= 5
    {
        let rows = Layout::vertical([
            Constraint::Percentage(52),
            Constraint::Length(1),
            Constraint::Min(1),
        ])
        .split(content);
        (rows[0], Some((rows[2], PreviewPlacement::Bottom)), false)
    } else {
        (content, None, preview_visible)
    };

    Some(ScreenLayout {
        header: sections[0],
        input: sections[1],
        top_divider: sections[2],
        content,
        bottom_divider: sections[4],
        footer: sections[5],
        list,
        preview,
        preview_unavailable,
    })
}

fn page_size_for(full: Rect, preview_visible: bool) -> usize {
    screen_layout(full, preview_visible)
        .map(|layout| (layout.list.height as usize).max(1))
        .unwrap_or(1)
}

fn draw(frame: &mut Frame, app: &App, theme: &Theme) {
    let full = frame.area();
    if let Some(layout) = screen_layout(full, app.preview_visible) {
        frame.render_widget(Clear, full);
        frame.render_widget(
            Block::default()
                .borders(Borders::ALL)
                .border_style(theme.border()),
            full,
        );
        render_header(frame, app, theme, layout.header);
        render_input(frame, app, theme, layout.input);
        render_divider(frame, theme, layout.top_divider);
        frame.render_widget(Clear, layout.content);
        render_list(frame, app, theme, layout.list);
        if let Some((preview_area, placement)) = layout.preview {
            render_preview(frame, app, theme, preview_area, placement);
        }
        render_divider(frame, theme, layout.bottom_divider);
        render_footer(frame, app, theme, layout.footer, layout.preview_unavailable);
    } else {
        frame.render_widget(Clear, full);
        frame.render_widget(
            Paragraph::new(app.language.text(TextKey::TerminalTooSmall)).style(theme.dim()),
            full,
        );
    }

    match app.mode {
        Mode::Normal => {}
        Mode::Help => render_help(frame, app.language, theme, full),
        Mode::Settings { selected } => render_settings(frame, app, theme, full, selected),
        Mode::ConfirmDelete { candidate_idx } => {
            render_confirm_delete(frame, app, theme, full, candidate_idx)
        }
    }
}

fn render_header(frame: &mut Frame, app: &App, theme: &Theme, area: Rect) {
    let title = "cdh";
    let summary = app.page().summary(app.filtered_results.len(), app.language);
    let title_width = UnicodeWidthStr::width(title);
    let summary_width = UnicodeWidthStr::width(summary.as_str());
    let width = area.width as usize;
    let mut spans = vec![Span::styled(title, theme.title())];
    if title_width + summary_width < width {
        spans.push(Span::raw(" ".repeat(width - title_width - summary_width)));
        spans.push(Span::styled(summary, theme.dim()));
    } else if width > title_width + 1 {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            trim_middle(&summary, width - title_width - 1),
            theme.dim(),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct QueryViewport {
    before: String,
    after: String,
    left_hidden: bool,
    right_hidden: bool,
}

impl QueryViewport {
    fn display_width(&self) -> usize {
        UnicodeWidthStr::width(self.before.as_str())
            + UnicodeWidthStr::width(self.after.as_str())
            + usize::from(self.left_hidden)
            + usize::from(self.right_hidden)
    }
}

fn query_viewport(query: &str, cursor_index: usize, max_width: usize) -> QueryViewport {
    let cursor_index = cursor_index.min(query.chars().count());
    let (before, after) = split_at_char_index(query, cursor_index);
    if UnicodeWidthStr::width(query) <= max_width {
        return QueryViewport {
            before: before.to_string(),
            after: after.to_string(),
            left_hidden: false,
            right_hidden: false,
        };
    }
    if max_width == 0 {
        return QueryViewport {
            before: String::new(),
            after: String::new(),
            left_hidden: false,
            right_hidden: false,
        };
    }

    let mut indicator_slots = 0;
    loop {
        let content_width = max_width.saturating_sub(indicator_slots);
        let (visible_before, visible_after) = visible_query_sides(before, after, content_width);
        let left_hidden = visible_before != before;
        let right_hidden = visible_after != after;
        let required_slots = usize::from(left_hidden) + usize::from(right_hidden);
        if required_slots >= max_width {
            return QueryViewport {
                before: visible_before,
                after: visible_after,
                left_hidden: false,
                right_hidden: false,
            };
        }
        if required_slots == indicator_slots {
            return QueryViewport {
                before: visible_before,
                after: visible_after,
                left_hidden,
                right_hidden,
            };
        }
        indicator_slots = required_slots;
    }
}

fn visible_query_sides(before: &str, after: &str, max_width: usize) -> (String, String) {
    let before_width = UnicodeWidthStr::width(before);
    let after_width = UnicodeWidthStr::width(after);
    let preferred_before = max_width.saturating_mul(2) / 3;
    let mut before_budget = before_width.min(preferred_before);
    let mut after_budget = after_width.min(max_width.saturating_sub(before_budget));
    let mut remaining = max_width.saturating_sub(before_budget + after_budget);

    let extra_after = after_width.saturating_sub(after_budget).min(remaining);
    after_budget += extra_after;
    remaining -= extra_after;
    before_budget += before_width.saturating_sub(before_budget).min(remaining);

    (
        take_width_back(before, before_budget),
        take_width_front(after, after_budget),
    )
}

fn render_input(frame: &mut Frame, app: &App, theme: &Theme, area: Rect) {
    let prompt = "❯ ";
    let cursor = "▏";
    let width = area.width as usize;
    let available = width.saturating_sub(UnicodeWidthStr::width(prompt));
    let cursor_width = UnicodeWidthStr::width(cursor);
    let mut spans = vec![Span::styled(prompt, theme.accent())];
    if app.query.is_empty() {
        spans.push(Span::styled(cursor, theme.accent()));
        spans.push(Span::styled(
            trim_end(
                app.language.text(TextKey::SearchPlaceholder),
                available.saturating_sub(cursor_width),
            ),
            theme.dim(),
        ));
    } else {
        let cursor_index = app.query_cursor.min(app.query_char_count());
        let viewport = query_viewport(
            &app.query,
            cursor_index,
            available.saturating_sub(cursor_width),
        );
        debug_assert!(viewport.display_width() + cursor_width <= available || available == 0);
        if viewport.left_hidden {
            spans.push(Span::styled("…", theme.border()));
        }
        spans.push(Span::styled(viewport.before, theme.primary()));
        spans.push(Span::styled(cursor, theme.accent()));
        spans.push(Span::styled(viewport.after, theme.primary()));
        if viewport.right_hidden {
            spans.push(Span::styled("…", theme.border()));
        }
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_divider(frame: &mut Frame, theme: &Theme, area: Rect) {
    frame.render_widget(
        Paragraph::new(Span::styled(
            "─".repeat(area.width as usize),
            theme.border(),
        )),
        area,
    );
}

fn empty_state_line(query: &str, language: Language, theme: &Theme) -> Line<'static> {
    if query.is_empty() {
        return Line::from(Span::styled(
            language.text(TextKey::EmptyHistory),
            theme.dim(),
        ));
    }
    Line::from(vec![
        Span::styled(language.text(TextKey::NoMatches), theme.primary()),
        Span::styled(" · ", theme.border()),
        Span::styled("Ctrl+U", theme.key_hint()),
        Span::styled(language.text(TextKey::ClearSearch), theme.dim()),
    ])
}

fn render_list(frame: &mut Frame, app: &App, theme: &Theme, area: Rect) {
    let page = app.page();
    let row_capacity = area.height as usize;
    let list_area = Rect::new(area.x, area.y, area.width, row_capacity as u16);
    app.last_list_start.set(page.start);
    app.last_list_area.set(Rect::new(
        list_area.x,
        list_area.y,
        list_area.width,
        page.end.saturating_sub(page.start) as u16,
    ));

    if app.filtered_results.is_empty() {
        if list_area.height == 0 {
            return;
        }
        let empty_area = Rect::new(
            list_area.x,
            list_area.y + list_area.height.saturating_sub(1) / 2,
            list_area.width,
            1,
        );
        frame.render_widget(
            Paragraph::new(empty_state_line(&app.query, app.language, theme))
                .alignment(Alignment::Center),
            empty_area,
        );
        return;
    }

    let mut lines = Vec::with_capacity(page.end - page.start);
    for (offset, matched) in app.filtered_results[page.start..page.end]
        .iter()
        .enumerate()
    {
        let index = page.start + offset;
        lines.push(list_row_line(
            &app.candidates[matched.idx],
            &matched.highlights,
            ListRowOptions {
                index,
                total: app.filtered_results.len(),
                selected: index == app.selected_index,
                width: area.width as usize,
                language: app.language,
            },
            theme,
        ));
    }
    frame.render_widget(Paragraph::new(lines), list_area);
}

#[derive(Clone, Copy)]
struct ListRowOptions {
    index: usize,
    total: usize,
    selected: bool,
    width: usize,
    language: Language,
}

fn list_row_line(
    candidate: &Candidate,
    highlights: &[u32],
    options: ListRowOptions,
    theme: &Theme,
) -> Line<'static> {
    let ListRowOptions {
        index,
        total,
        selected,
        width,
        language,
    } = options;
    let index_width = decimal_width(total.max(1)).max(2);
    let row_style = if selected {
        theme.selected()
    } else if candidate.exists {
        theme.primary()
    } else {
        theme.dim().add_modifier(Modifier::CROSSED_OUT)
    };
    let (path_style, terminal_style) = if candidate.exists {
        if selected {
            (
                theme.selected().fg(theme.color(0xbd, 0xc8, 0xe2)),
                theme
                    .selected()
                    .fg(theme.color(0xf7, 0xf9, 0xff))
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            (theme.dim(), theme.primary().add_modifier(Modifier::BOLD))
        }
    } else if selected {
        (
            theme
                .selected()
                .fg(theme.dim_color())
                .add_modifier(Modifier::CROSSED_OUT),
            theme
                .selected()
                .fg(theme.warning_color())
                .add_modifier(Modifier::BOLD | Modifier::CROSSED_OUT),
        )
    } else {
        (
            theme.dim().add_modifier(Modifier::CROSSED_OUT),
            theme
                .warning()
                .add_modifier(Modifier::BOLD | Modifier::CROSSED_OUT),
        )
    };

    let marker = if selected { "›" } else { " " };
    let index_label = format!("{:>index_width$}  ", index + 1);
    let prefix_width =
        UnicodeWidthStr::width(marker) + UnicodeWidthStr::width(index_label.as_str());
    let status_label = language.text(TextKey::MissingStatus);
    let status_width = UnicodeWidthStr::width(status_label) + 2;
    let show_status = !candidate.exists && width >= prefix_width + status_width + 4;
    let status = show_status.then_some(status_label);
    let status_width = status.map_or(0, |_| status_width);
    let available = width.saturating_sub(prefix_width + status_width);

    let marker_style = if selected {
        theme.selected_marker()
    } else {
        row_style
    };
    let mut spans = vec![
        Span::styled(marker, marker_style),
        Span::styled(index_label, row_style),
    ];
    spans.extend(list_path_spans(
        &candidate.display,
        highlights,
        available,
        path_style,
        terminal_style,
        candidate.exists,
        theme,
    ));
    pad_spans_to_width(&mut spans, prefix_width + available, row_style);

    if let Some(status) = status {
        spans.push(Span::styled("  ", row_style));
        let status_style = if selected {
            theme.selected().fg(theme.warning_color())
        } else {
            theme.warning()
        };
        spans.push(Span::styled(status, status_style));
    }
    pad_spans_to_width(&mut spans, width, row_style);
    Line::from(spans)
}

fn list_path_spans(
    path: &PathDisplay,
    raw_highlights: &[u32],
    max_width: usize,
    path_style: Style,
    terminal_style: Style,
    allow_highlights: bool,
    theme: &Theme,
) -> Vec<Span<'static>> {
    let highlighted = allow_highlights.then(|| path.display_highlight_indices(raw_highlights));
    visible_path_pieces(path, max_width)
        .into_iter()
        .map(|piece| match piece {
            PathPiece::Ellipsis => Span::styled("…", path_style),
            PathPiece::Character(index) => {
                let base = if path.terminal_component.contains(&index) {
                    terminal_style
                } else {
                    path_style
                };
                let style = if highlighted
                    .as_ref()
                    .is_some_and(|indices| indices.contains(&index))
                {
                    theme.matched(base)
                } else {
                    base
                };
                Span::styled(path.chars[index].to_string(), style)
            }
        })
        .collect()
}

fn pad_spans_to_width(spans: &mut Vec<Span<'static>>, width: usize, style: Style) {
    let used: usize = spans
        .iter()
        .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
        .sum();
    if used < width {
        spans.push(Span::styled(" ".repeat(width - used), style));
    }
}

fn decimal_width(value: usize) -> usize {
    value.to_string().len()
}

fn render_preview(
    frame: &mut Frame,
    app: &App,
    theme: &Theme,
    area: Rect,
    placement: PreviewPlacement,
) {
    let borders = match placement {
        PreviewPlacement::Side => Borders::LEFT,
        PreviewPlacement::Bottom => Borders::TOP,
    };
    let block = Block::default()
        .borders(borders)
        .border_style(theme.border());
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let Some(candidate) = app.selected_candidate() else {
        frame.render_widget(
            Paragraph::new(Span::styled(
                app.language.text(TextKey::NoSelection),
                theme.dim(),
            )),
            inner,
        );
        return;
    };
    let width = inner.width as usize;
    let mut lines = vec![
        Line::from(Span::styled(
            trim_end(&candidate.name, width),
            theme.title(),
        )),
        Line::from(Span::styled(
            trim_middle(&candidate.raw, width),
            theme.dim(),
        )),
        Line::raw(""),
    ];

    match preview_outcome_for_selected(app, candidate) {
        PreviewPanelOutcome::Loading => {
            lines.push(Line::from(Span::styled(
                app.language.text(TextKey::Loading),
                theme.dim(),
            )));
        }
        PreviewPanelOutcome::Missing | PreviewPanelOutcome::Outcome(PreviewOutcome::Missing) => {
            lines.push(Line::from(Span::styled(
                app.language.text(TextKey::DirectoryMissing),
                theme.warning(),
            )));
        }
        PreviewPanelOutcome::Outcome(PreviewOutcome::Error(message)) => {
            let prefix = app.language.text(TextKey::CannotReadPrefix);
            lines.push(Line::from(vec![
                Span::styled(prefix, theme.dim()),
                Span::styled(
                    trim_end(
                        message,
                        width.saturating_sub(UnicodeWidthStr::width(prefix)),
                    ),
                    theme.warning(),
                ),
            ]));
        }
        PreviewPanelOutcome::Outcome(PreviewOutcome::Data(data)) => {
            if let Some(git) = &data.git {
                lines.push(git_line(git, app.language, theme, width));
            }
            lines.push(Line::from(vec![
                Span::styled(app.language.text(TextKey::LastVisitPrefix), theme.dim()),
                Span::styled(
                    relative_time(candidate.last_visit, app.language),
                    theme.primary(),
                ),
            ]));
            lines.push(Line::raw(""));
            if data.entries.is_empty() {
                lines.push(Line::from(Span::styled(
                    app.language.text(TextKey::EmptyDirectory),
                    theme.dim(),
                )));
            } else {
                for entry in &data.entries {
                    let icon = if entry.is_dir { "▸ " } else { "· " };
                    lines.push(Line::from(vec![
                        Span::styled(icon, theme.dim()),
                        Span::styled(
                            trim_middle(&entry.name, width.saturating_sub(2)),
                            theme.primary(),
                        ),
                    ]));
                }
                if data.has_more_entries {
                    lines.push(Line::from(Span::styled(
                        app.language.text(TextKey::MoreEntries),
                        theme.dim(),
                    )));
                }
            }
        }
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

fn git_line(git: &GitInfo, language: Language, theme: &Theme, width: usize) -> Line<'static> {
    let (status, color) = match git.dirty {
        Some(true) => (language.text(TextKey::GitModified), theme.warning_color()),
        Some(false) => (language.text(TextKey::GitClean), theme.success_color()),
        None => ("", theme.dim_color()),
    };
    let branch_width = width.saturating_sub(8 + UnicodeWidthStr::width(status));
    let mut spans = vec![
        Span::styled("Git: ", theme.dim()),
        Span::styled("● ", Style::default().fg(color)),
        Span::styled(trim_end(&git.branch, branch_width), theme.primary()),
    ];
    if !status.is_empty() {
        spans.push(Span::styled(" · ", theme.dim()));
        spans.push(Span::styled(status, Style::default().fg(color)));
    }
    Line::from(spans)
}

enum PreviewPanelOutcome<'a> {
    Loading,
    Missing,
    Outcome(&'a PreviewOutcome),
}

fn preview_outcome_for_selected<'a>(
    app: &'a App,
    candidate: &Candidate,
) -> PreviewPanelOutcome<'a> {
    if !candidate.exists {
        return PreviewPanelOutcome::Missing;
    }
    if app.preview_loading.as_deref() == Some(candidate.raw.as_str())
        || app
            .preview_pending
            .as_ref()
            .map(|(path, _)| path == &candidate.raw)
            .unwrap_or(false)
    {
        return PreviewPanelOutcome::Loading;
    }
    if let Some((path, outcome)) = &app.preview_current {
        if path == &candidate.raw {
            return PreviewPanelOutcome::Outcome(outcome);
        }
    }
    PreviewPanelOutcome::Loading
}

fn relative_time(timestamp: Option<i64>, language: Language) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    relative_time_at(timestamp, now, language)
}

fn relative_time_at(timestamp: Option<i64>, now: i64, language: Language) -> String {
    language.relative_time(timestamp, now)
}

fn render_footer(
    frame: &mut Frame,
    app: &App,
    theme: &Theme,
    area: Rect,
    preview_unavailable: bool,
) {
    let line = if let Some(notice) = &app.notice {
        Line::from(Span::styled(
            trim_end(notice, area.width as usize),
            theme.primary(),
        ))
    } else if preview_unavailable {
        Line::from(Span::styled(
            trim_end(
                app.language.text(TextKey::PreviewSpaceInsufficient),
                area.width as usize,
            ),
            theme.warning(),
        ))
    } else {
        let hint = fit_footer(
            app.language.text(TextKey::FooterPrimary),
            app.language.text(TextKey::FooterCompact),
            app.language.text(TextKey::FooterShort),
            area.width as usize,
        );
        footer_hint_line(&trim_end(&hint, area.width as usize), theme)
    };
    frame.render_widget(Paragraph::new(line), area);
}

fn footer_hint_line(text: &str, theme: &Theme) -> Line<'static> {
    let mut spans = Vec::new();
    for (index, segment) in text.split(" · ").enumerate() {
        if index > 0 {
            spans.push(Span::styled(" · ", theme.border()));
        }
        if let Some((key, description)) = segment.split_once(' ') {
            spans.push(Span::styled(key.to_string(), theme.key_hint()));
            spans.push(Span::styled(" ", theme.dim()));
            spans.push(Span::styled(description.to_string(), theme.dim()));
        } else {
            spans.push(Span::styled(segment.to_string(), theme.dim()));
        }
    }
    Line::from(spans)
}

fn fit_footer(full: &str, compact: &str, short: &str, width: usize) -> String {
    if UnicodeWidthStr::width(full) <= width {
        full.to_string()
    } else if UnicodeWidthStr::width(compact) <= width {
        compact.to_string()
    } else {
        short.to_string()
    }
}

fn render_help(frame: &mut Frame, language: Language, theme: &Theme, full: Rect) {
    let lines = help_lines(language, theme);
    let width = 76u16.min(full.width.saturating_sub(4));
    let height = (lines.len() as u16 + 2).min(full.height);
    let area = centered(full, width, height);
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.border());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(Paragraph::new(lines), inner);
}

fn help_lines(language: Language, theme: &Theme) -> Vec<Line<'static>> {
    vec![
        Line::from(Span::styled(
            language.text(TextKey::HelpTitle),
            theme.title(),
        )),
        help_section(language.text(TextKey::Movement), theme),
        help_row("↑ / Ctrl+P", language.text(TextKey::PreviousItem), theme),
        help_row("↓ / Ctrl+N", language.text(TextKey::NextItem), theme),
        help_section(language.text(TextKey::Paging), theme),
        help_row("Ctrl+↑ / PgUp", language.text(TextKey::PreviousPage), theme),
        help_row("Ctrl+↓ / PgDn", language.text(TextKey::NextPage), theme),
        help_row("Home", language.text(TextKey::FirstItem), theme),
        help_row("End", language.text(TextKey::LastItem), theme),
        help_section(language.text(TextKey::Search), theme),
        help_row("← / →", language.text(TextKey::MoveCursor), theme),
        help_row(
            "Backspace",
            language.text(TextKey::DeleteBeforeCursor),
            theme,
        ),
        help_row("Delete", language.text(TextKey::DeleteAtCursor), theme),
        help_row(
            "Ctrl+U",
            language.text(TextKey::ClearSearchDescription),
            theme,
        ),
        help_section(language.text(TextKey::Actions), theme),
        help_row("Enter", language.text(TextKey::JumpToDirectory), theme),
        help_row("Tab", language.text(TextKey::TogglePreview), theme),
        help_row("Ctrl+D", language.text(TextKey::DeleteHistoryEntry), theme),
        help_row("F1 / ? / ？", language.text(TextKey::OpenHelp), theme),
        help_row("F2", language.text(TextKey::OpenSettings), theme),
        help_row(
            "↑↓  ←→  Enter/Space  Esc",
            language.text(TextKey::SettingsControls),
            theme,
        ),
        help_row("Esc", language.text(TextKey::EscapeDescription), theme),
    ]
}

fn render_settings(frame: &mut Frame, app: &App, theme: &Theme, full: Rect, selected: usize) {
    let width = 72u16.min(full.width.saturating_sub(2));
    let height = 9u16.min(full.height);
    if width < 2 || height < 2 {
        return;
    }

    let area = centered(full, width, height);
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.border());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.is_empty() {
        return;
    }

    render_settings_line(
        frame,
        inner,
        0,
        app.language.text(TextKey::SettingsTitle),
        theme.title(),
    );

    let row_start = u16::from(inner.height >= 7);
    for (index, key) in [
        SettingKey::Language,
        SettingKey::Preview,
        SettingKey::Color,
        SettingKey::Mouse,
    ]
    .into_iter()
    .enumerate()
    {
        let offset = row_start + 1 + index as u16;
        if offset >= inner.height {
            break;
        }
        let style = if index == selected.min(3) {
            theme.selected()
        } else {
            theme.primary()
        };
        let text = settings_row_text(app, key, inner.width as usize);
        render_settings_line(frame, inner, offset, &text, style);
    }

    if inner.height > 1 {
        let footer = trim_end(
            app.language.text(TextKey::SettingsFooter),
            inner.width as usize,
        );
        render_settings_line(frame, inner, inner.height - 1, &footer, theme.dim());
    }
}

fn render_settings_line(frame: &mut Frame, inner: Rect, offset: u16, text: &str, style: Style) {
    if offset >= inner.height {
        return;
    }
    let area = Rect::new(inner.x, inner.y + offset, inner.width, 1);
    frame.render_widget(
        Paragraph::new(trim_end(text, inner.width as usize)).style(style),
        area,
    );
}

fn settings_row_text(app: &App, key: SettingKey, width: usize) -> String {
    let effective = app.settings.effective();
    let (label, value) = match key {
        SettingKey::Language => (
            app.language.text(TextKey::SettingLanguage),
            match effective.language {
                LanguagePreference::Auto => app.language.text(TextKey::LanguageAuto),
                LanguagePreference::ZhCn => app.language.text(TextKey::LanguageSimplifiedChinese),
                LanguagePreference::En => app.language.text(TextKey::LanguageEnglish),
            },
        ),
        SettingKey::Preview => (
            app.language.text(TextKey::SettingPreviewStartup),
            setting_boolean_text(app.language, effective.preview),
        ),
        SettingKey::Color => (
            app.language.text(TextKey::SettingColor),
            setting_boolean_text(app.language, effective.color),
        ),
        SettingKey::Mouse => (
            app.language.text(TextKey::SettingMouseCapture),
            setting_boolean_text(app.language, effective.mouse),
        ),
    };
    let marker = app
        .settings
        .is_locked(key)
        .then(|| app.language.text(TextKey::EnvironmentControlled));
    let right = marker
        .map(|marker| format!("{value} · {marker}"))
        .unwrap_or_else(|| value.to_string());
    let occupied = UnicodeWidthStr::width(label) + UnicodeWidthStr::width(right.as_str());
    if occupied < width {
        format!("{label}{}{right}", " ".repeat(width - occupied))
    } else {
        trim_end(&format!("{label}  {right}"), width)
    }
}

fn setting_boolean_text(language: Language, value: bool) -> &'static str {
    language.text(if value {
        TextKey::SettingOn
    } else {
        TextKey::SettingOff
    })
}

fn help_section(title: &str, theme: &Theme) -> Line<'static> {
    Line::from(Span::styled(title.to_string(), theme.accent()))
}

fn render_confirm_delete(
    frame: &mut Frame,
    app: &App,
    theme: &Theme,
    full: Rect,
    candidate_idx: usize,
) {
    let width = 56u16.min(full.width.saturating_sub(4));
    let path = app
        .candidates
        .get(candidate_idx)
        .map(|candidate| candidate.display.text.clone())
        .unwrap_or_else(|| app.language.text(TextKey::UnknownDirectory).to_string());
    let message = confirm_delete_message(&path, width.saturating_sub(2) as usize, app.language);
    let lines = vec![
        Line::from(Span::styled(
            app.language.text(TextKey::ConfirmDeleteTitle),
            theme.title(),
        )),
        Line::raw(""),
        Line::from(Span::styled(message, theme.primary())),
        Line::raw(""),
        Line::from(Span::styled(
            app.language.text(TextKey::ConfirmDeleteAgain),
            theme.dim(),
        )),
    ];
    let height = (lines.len() as u16 + 2).min(full.height.saturating_sub(2));
    let area = centered(full, width, height);
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.border());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(Paragraph::new(lines), inner);
}

fn confirm_delete_message(path: &str, max_width: usize, language: Language) -> String {
    let prefix = language.text(TextKey::ConfirmDeletePrefix);
    let suffix = language.text(TextKey::ConfirmDeleteSuffix);
    let fixed_width = UnicodeWidthStr::width(prefix) + UnicodeWidthStr::width(suffix);
    if max_width <= fixed_width {
        return trim_end(language.text(TextKey::ConfirmDeleteShort), max_width);
    }
    let path_width = max_width - fixed_width;
    format!("{prefix}{}{suffix}", trim_middle(path, path_width))
}

fn help_row(key: &str, description: &str, theme: &Theme) -> Line<'static> {
    const KEY_WIDTH: usize = 21;
    let padding = KEY_WIDTH.saturating_sub(UnicodeWidthStr::width(key));
    Line::from(vec![
        Span::styled(format!("{key}{}", " ".repeat(padding)), theme.accent()),
        Span::styled(description.to_string(), theme.primary()),
    ])
}

fn centered(full: Rect, width: u16, height: u16) -> Rect {
    Rect::new(
        full.x + (full.width.saturating_sub(width)) / 2,
        full.y + (full.height.saturating_sub(height)) / 2,
        width,
        height,
    )
}

fn split_at_char_index(text: &str, char_index: usize) -> (&str, &str) {
    let byte_index = text
        .char_indices()
        .nth(char_index)
        .map(|(byte_index, _)| byte_index)
        .unwrap_or(text.len());
    text.split_at(byte_index)
}

/// Truncate at the end using terminal display width, never UTF-8 byte length.
fn trim_end(text: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    if UnicodeWidthStr::width(text) <= max_width {
        return text.to_string();
    }
    if max_width == 1 {
        return "…".to_string();
    }
    let mut result = take_width_front(text, max_width - 1);
    result.push('…');
    result
}

/// Middle truncation keeps a useful path prefix and basename while preserving
/// CJK and emoji display widths.
fn trim_middle(text: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    if UnicodeWidthStr::width(text) <= max_width {
        return text.to_string();
    }
    if max_width == 1 {
        return "…".to_string();
    }
    let budget = max_width - 1;
    let left_budget = budget - budget / 2;
    let right_budget = budget - left_budget;
    format!(
        "{}…{}",
        take_width_front(text, left_budget),
        take_width_back(text, right_budget)
    )
}

fn take_width_front(text: &str, max_width: usize) -> String {
    let mut width = 0;
    let mut result = String::new();
    for character in text.chars() {
        let character_width = UnicodeWidthChar::width(character).unwrap_or(0);
        if width + character_width > max_width {
            break;
        }
        width += character_width;
        result.push(character);
    }
    result
}

fn take_width_back(text: &str, max_width: usize) -> String {
    let mut width = 0;
    let mut reverse = String::new();
    for character in text.chars().rev() {
        let character_width = UnicodeWidthChar::width(character).unwrap_or(0);
        if width + character_width > max_width {
            break;
        }
        width += character_width;
        reverse.push(character);
    }
    reverse.chars().rev().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EffectiveConfig, Paths};
    use ratatui::{backend::TestBackend, buffer::Buffer};
    use settings::{LanguagePreference, SettingKey, UiEnvironment, UiSettings};
    use std::collections::VecDeque;
    use std::fs;
    use std::io;
    use std::path::PathBuf;
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[derive(Debug)]
    struct FakeMouseCaptureControl {
        actual: bool,
        transitions: Vec<bool>,
        results: VecDeque<io::Result<()>>,
    }

    impl FakeMouseCaptureControl {
        fn succeeding(actual: bool) -> Self {
            Self {
                actual,
                transitions: Vec::new(),
                results: VecDeque::new(),
            }
        }

        fn with_results(actual: bool, results: Vec<io::Result<()>>) -> Self {
            Self {
                actual,
                transitions: Vec::new(),
                results: results.into(),
            }
        }
    }

    impl MouseCaptureControl for FakeMouseCaptureControl {
        fn mouse_capture_enabled(&self) -> bool {
            self.actual
        }

        fn set_mouse_capture(&mut self, enabled: bool) -> io::Result<()> {
            self.transitions.push(enabled);
            if let Some(result) = self.results.pop_front() {
                result?;
            }
            self.actual = enabled;
            Ok(())
        }
    }

    fn mouse_test_root(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("cdh_settings_mouse_{name}_{unique}"))
    }

    fn mouse_test_settings(name: &str, contents: Option<&str>) -> (PathBuf, UiSettings) {
        let root = mouse_test_root(name);
        let path = root.join("tui.toml");
        if let Some(contents) = contents {
            fs::create_dir_all(&root).unwrap();
            fs::write(&path, contents).unwrap();
        }
        let settings = UiSettings::load(path, UiEnvironment::default()).settings;
        (root, settings)
    }

    #[test]
    fn settings_mouse_successful_enable_updates_actual_and_persisted_state() {
        let (root, mut settings) = mouse_test_settings(
            "enable",
            Some("language = \"auto\"\npreview = false\ncolor = true\nmouse = false\n"),
        );
        let candidate = settings.candidate(SettingKey::Mouse, 1).unwrap();
        let mut control = FakeMouseCaptureControl::succeeding(false);

        apply_mouse_setting(&mut settings, candidate, &mut control).unwrap();

        assert!(control.mouse_capture_enabled());
        assert!(settings.saved().mouse);
        assert!(fs::read_to_string(root.join("tui.toml"))
            .unwrap()
            .contains("mouse = true"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn settings_mouse_successful_disable_updates_actual_and_persisted_state() {
        let (root, mut settings) = mouse_test_settings("disable", None);
        let candidate = settings.candidate(SettingKey::Mouse, 1).unwrap();
        let mut control = FakeMouseCaptureControl::succeeding(true);

        apply_mouse_setting(&mut settings, candidate, &mut control).unwrap();

        assert!(!control.mouse_capture_enabled());
        assert!(!settings.saved().mouse);
        assert!(fs::read_to_string(root.join("tui.toml"))
            .unwrap()
            .contains("mouse = false"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn settings_mouse_terminal_failure_changes_neither_actual_nor_settings() {
        let (root, mut settings) = mouse_test_settings("transition-failure", None);
        let candidate = settings.candidate(SettingKey::Mouse, 1).unwrap();
        let original = settings.saved();
        let mut control = FakeMouseCaptureControl::with_results(
            true,
            vec![Err(io::Error::other("transition failed"))],
        );

        let error = apply_mouse_setting(&mut settings, candidate, &mut control).unwrap_err();

        assert!(matches!(
            error,
            MouseSettingError::TerminalTransition {
                requested: false,
                ..
            }
        ));
        assert!(control.mouse_capture_enabled());
        assert_eq!(settings.saved(), original);
        assert!(!root.join("tui.toml").exists());
    }

    #[test]
    fn settings_mouse_persistence_failure_rolls_terminal_back() {
        let root = mouse_test_root("persistence-rollback");
        let blocker = root.join("blocker");
        fs::create_dir_all(&root).unwrap();
        fs::write(&blocker, "not a directory").unwrap();
        let mut settings =
            UiSettings::load(blocker.join("tui.toml"), UiEnvironment::default()).settings;
        let candidate = settings.candidate(SettingKey::Mouse, 1).unwrap();
        let original = settings.saved();
        let mut control = FakeMouseCaptureControl::succeeding(true);

        let error = apply_mouse_setting(&mut settings, candidate, &mut control).unwrap_err();

        assert!(matches!(
            error,
            MouseSettingError::Persistence {
                requested: false,
                rollback_error: None,
                actual: true,
                ..
            }
        ));
        assert_eq!(control.transitions, vec![false, true]);
        assert!(control.mouse_capture_enabled());
        assert_eq!(settings.saved(), original);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn settings_mouse_persistence_and_rollback_failure_tracks_actual_state() {
        let root = mouse_test_root("double-failure");
        let blocker = root.join("blocker");
        fs::create_dir_all(&root).unwrap();
        fs::write(&blocker, "not a directory").unwrap();
        let mut settings =
            UiSettings::load(blocker.join("tui.toml"), UiEnvironment::default()).settings;
        let candidate = settings.candidate(SettingKey::Mouse, 1).unwrap();
        let original = settings.saved();
        let mut control = FakeMouseCaptureControl::with_results(
            true,
            vec![Ok(()), Err(io::Error::other("rollback failed"))],
        );

        let error = apply_mouse_setting(&mut settings, candidate, &mut control).unwrap_err();

        assert!(matches!(
            error,
            MouseSettingError::Persistence {
                requested: false,
                rollback_error: Some(_),
                actual: false,
                ..
            }
        ));
        assert_eq!(control.transitions, vec![false, true]);
        assert!(!control.mouse_capture_enabled());
        assert_eq!(settings.saved(), original);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn settings_mouse_event_gating_uses_current_actual_state() {
        let mut control = FakeMouseCaptureControl::succeeding(false);
        assert!(!mouse_event_enabled(&control, Mode::Normal));

        control.set_mouse_capture(true).unwrap();
        assert!(mouse_event_enabled(&control, Mode::Normal));
        assert!(!mouse_event_enabled(&control, Mode::Help));

        control.set_mouse_capture(false).unwrap();
        assert!(!mouse_event_enabled(&control, Mode::Normal));
    }

    fn recs(paths: &[(&str, f64)]) -> Vec<Recommendation> {
        recs_with_exists(
            &paths
                .iter()
                .map(|(path, score)| (*path, *score, true))
                .collect::<Vec<_>>(),
        )
    }

    fn recs_with_exists(paths: &[(&str, f64, bool)]) -> Vec<Recommendation> {
        paths
            .iter()
            .map(|(path, score, exists)| Recommendation {
                path: (*path).to_string(),
                score: *score,
                breakdown: crate::recommend::ScoreBreakdown {
                    frecency_norm: *score,
                    recency_norm: *score,
                    context_norm: *score,
                    uniq_norm: *score,
                },
                exists: *exists,
            })
            .collect()
    }

    fn app_with_paths(paths: &[(&str, f64)]) -> App {
        App::with_preview_worker(build_candidates(&recs(paths)), None, false)
    }

    fn settings_mode_app(
        name: &str,
        contents: Option<&str>,
        environment: UiEnvironment,
        locale_language: Language,
    ) -> (PathBuf, App) {
        let root = mouse_test_root(name);
        let path = root.join("tui.toml");
        if let Some(contents) = contents {
            fs::create_dir_all(&root).unwrap();
            fs::write(&path, contents).unwrap();
        }
        let loaded = UiSettings::load(path, environment);
        let app = App::with_settings(
            build_candidates(&recs(&[("/a", 0.9)])),
            loaded,
            locale_language,
            None,
        );
        (root, app)
    }

    fn settings_mode_select(app: &mut App, selected: usize) {
        app.mode = Mode::Settings { selected };
    }

    fn settings_panel_buffer(app: &App, width: u16, height: u16, color: bool) -> Buffer {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| draw(frame, app, &Theme::new(color)))
            .unwrap();
        terminal.backend().buffer().clone()
    }

    fn settings_panel_text(buffer: &Buffer) -> String {
        let area = buffer.area;
        (area.y..area.y + area.height)
            .map(|y| settings_panel_buffer_row(buffer, y))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn settings_panel_buffer_row(buffer: &Buffer, y: u16) -> String {
        let area = buffer.area;
        let mut text = String::new();
        let mut previous_was_wide = false;
        for x in area.x..area.x + area.width {
            let symbol = buffer[(x, y)].symbol();
            if previous_was_wide && symbol == " " {
                previous_was_wide = false;
                continue;
            }
            previous_was_wide = UnicodeWidthStr::width(symbol) > 1;
            text.push_str(symbol);
        }
        text
    }

    fn settings_panel_row(buffer: &Buffer, needle: &str) -> u16 {
        let area = buffer.area;
        (area.y..area.y + area.height)
            .find(|y| settings_panel_buffer_row(buffer, *y).contains(needle))
            .unwrap_or_else(|| panic!("missing rendered row containing {needle:?}"))
    }

    #[test]
    fn settings_panel_bilingual_labels_values_and_hints() {
        for (language, required) in [
            (
                Language::En,
                [
                    "Settings",
                    "Language",
                    "Auto",
                    "Simplified Chinese",
                    "English",
                    "Preview on startup",
                    "Off",
                    "Color",
                    "On",
                    "Mouse capture",
                    "Up/Down select",
                    "Left/Right change",
                    "Enter/Space change",
                    "Esc done",
                ],
            ),
            (
                Language::ZhCn,
                [
                    "设置",
                    "语言",
                    "自动",
                    "简体中文",
                    "英语",
                    "启动时预览",
                    "关",
                    "颜色",
                    "开",
                    "鼠标捕获",
                    "上下选择",
                    "左右更改",
                    "回车/空格更改",
                    "Esc 完成",
                ],
            ),
        ] {
            let mut text = String::new();
            let mut roots = Vec::new();
            for (name, contents) in [
                ("panel-copy-auto", None),
                (
                    "panel-copy-zh",
                    Some("language = \"zh-CN\"\npreview = false\ncolor = true\nmouse = true\n"),
                ),
                (
                    "panel-copy-en",
                    Some("language = \"en\"\npreview = false\ncolor = true\nmouse = true\n"),
                ),
            ] {
                let (root, mut app) =
                    settings_mode_app(name, contents, UiEnvironment::default(), language);
                app.language = language;
                settings_mode_select(&mut app, 0);
                text.push_str(&settings_panel_text(&settings_panel_buffer(
                    &app, 80, 24, true,
                )));
                roots.push(root);
            }
            for expected in required {
                assert!(
                    text.contains(expected),
                    "missing {expected:?} in {language:?}"
                );
            }
            for root in roots {
                let _ = fs::remove_dir_all(root);
            }
        }
    }

    #[test]
    fn settings_panel_locked_row_has_environment_read_only_marker() {
        let environment = UiEnvironment {
            preview: Some(true),
            ..UiEnvironment::default()
        };
        let (root, mut app) = settings_mode_app("panel-lock", None, environment, Language::En);
        settings_mode_select(&mut app, 1);

        let text = settings_panel_text(&settings_panel_buffer(&app, 80, 24, true));

        assert!(text.contains("Preview on startup"));
        assert!(text.contains("Environment controlled/read-only"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn settings_panel_selected_row_uses_one_continuous_background() {
        let (root, mut app) = settings_mode_app(
            "panel-selected",
            None,
            UiEnvironment::default(),
            Language::En,
        );
        settings_mode_select(&mut app, 1);
        let theme = Theme::new(true);
        let buffer = settings_panel_buffer(&app, 80, 24, true);
        let y = settings_panel_row(&buffer, "Preview on startup");
        let selected_background = theme.selected().bg.unwrap();
        let selected_x = (0..buffer.area.width)
            .filter(|x| buffer[(*x, y)].bg == selected_background)
            .collect::<Vec<_>>();

        assert!(selected_x.len() >= 40);
        assert!(selected_x.windows(2).all(|pair| pair[1] == pair[0] + 1));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn settings_panel_colorless_selected_row_is_continuously_reversed() {
        let (root, mut app) = settings_mode_app(
            "panel-colorless",
            None,
            UiEnvironment::default(),
            Language::En,
        );
        settings_mode_select(&mut app, 2);
        let buffer = settings_panel_buffer(&app, 80, 24, false);
        let y = settings_panel_row(&buffer, "Color");
        let reversed_x = (0..buffer.area.width)
            .filter(|x| buffer[(*x, y)].modifier.contains(Modifier::REVERSED))
            .collect::<Vec<_>>();

        assert!(reversed_x.len() >= 40);
        assert!(reversed_x.windows(2).all(|pair| pair[1] == pair[0] + 1));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn settings_panel_narrow_and_tiny_terminals_render_safely() {
        let (root, mut app) = settings_mode_app(
            "panel-small",
            None,
            UiEnvironment::default(),
            Language::ZhCn,
        );
        settings_mode_select(&mut app, 3);

        for (width, height) in [(24, 12), (8, 8), (3, 3), (1, 1)] {
            let buffer = settings_panel_buffer(&app, width, height, true);
            assert_eq!(buffer.area.width, width);
            assert_eq!(buffer.area.height, height);
        }
        let narrow = settings_panel_text(&settings_panel_buffer(&app, 24, 12, true));
        assert!(narrow.contains("设置"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn settings_panel_f2_copy_is_distinct_in_footer_and_help() {
        for language in [Language::ZhCn, Language::En] {
            let footer = language.text(TextKey::FooterPrimary);
            assert!(footer.contains("F1"));
            assert!(footer.contains("F2"));

            let help = help_lines(language, &Theme::new(false));
            let help_text = help.iter().map(line_text).collect::<String>();
            assert!(help_text.contains("F1 / ? / ？"));
            assert!(help_text.contains("F2"));
            let (settings_entry, settings_controls) = match language {
                Language::ZhCn => ("打开设置", "选择 · 更改 · 更改 · 完成"),
                Language::En => ("Open settings", "Select · Change · Change · Done"),
            };
            assert!(help_text.contains(settings_entry));
            assert!(help_text.contains(settings_controls));
        }
    }

    #[test]
    fn settings_mode_f2_opens_and_f2_or_escape_closes_without_editing_query() {
        let (root, mut app) =
            settings_mode_app("open-close", None, UiEnvironment::default(), Language::En);
        app.query = "keep".to_string();
        app.query_cursor = 4;

        handle_key(&mut app, KeyCode::F(2), KeyModifiers::NONE, None);
        assert_eq!(app.mode, Mode::Settings { selected: 0 });
        handle_key(&mut app, KeyCode::Char('x'), KeyModifiers::NONE, None);
        assert_eq!(app.query, "keep");
        handle_key(&mut app, KeyCode::F(2), KeyModifiers::NONE, None);
        assert_eq!(app.mode, Mode::Normal);

        app.mode = Mode::Help;
        handle_key(&mut app, KeyCode::F(2), KeyModifiers::NONE, None);
        assert_eq!(app.mode, Mode::Settings { selected: 0 });
        handle_key(&mut app, KeyCode::Esc, KeyModifiers::NONE, None);
        assert_eq!(app.mode, Mode::Normal);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn settings_mode_ctrl_c_and_ctrl_g_exit_from_every_mode() {
        for mode in [
            Mode::Normal,
            Mode::Help,
            Mode::Settings { selected: 2 },
            Mode::ConfirmDelete { candidate_idx: 0 },
        ] {
            for code in [KeyCode::Char('c'), KeyCode::Char('g')] {
                let mut app = app_with_paths(&[("/workspace", 1.0)]);
                app.mode = mode;
                assert_eq!(
                    handle_key(&mut app, code, KeyModifiers::CONTROL, None),
                    Some(None),
                    "{code:?} should exit from {mode:?}"
                );
            }
        }
    }

    #[test]
    fn settings_mode_up_down_selects_exactly_four_rows_and_clamps() {
        let (root, mut app) =
            settings_mode_app("selection", None, UiEnvironment::default(), Language::En);
        settings_mode_select(&mut app, 0);

        handle_key(&mut app, KeyCode::Up, KeyModifiers::NONE, None);
        assert_eq!(app.mode, Mode::Settings { selected: 0 });
        for selected in 1..=3 {
            handle_key(&mut app, KeyCode::Down, KeyModifiers::NONE, None);
            assert_eq!(app.mode, Mode::Settings { selected });
        }
        handle_key(&mut app, KeyCode::Down, KeyModifiers::NONE, None);
        assert_eq!(app.mode, Mode::Settings { selected: 3 });
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn settings_mode_edit_keys_cycle_language_and_toggle_boolean_values() {
        let (root, mut app) =
            settings_mode_app("edit-keys", None, UiEnvironment::default(), Language::En);
        settings_mode_select(&mut app, 0);

        handle_key(&mut app, KeyCode::Right, KeyModifiers::NONE, None);
        assert_eq!(app.settings.saved().language, LanguagePreference::ZhCn);
        handle_key(&mut app, KeyCode::Enter, KeyModifiers::NONE, None);
        assert_eq!(app.settings.saved().language, LanguagePreference::En);
        handle_key(&mut app, KeyCode::Char(' '), KeyModifiers::NONE, None);
        assert_eq!(app.settings.saved().language, LanguagePreference::Auto);
        handle_key(&mut app, KeyCode::Left, KeyModifiers::NONE, None);
        assert_eq!(app.settings.saved().language, LanguagePreference::En);

        for selected in 1..=3 {
            settings_mode_select(&mut app, selected);
            let before = app.settings.saved();
            handle_key(&mut app, KeyCode::Enter, KeyModifiers::NONE, None);
            let after = if selected == 3 {
                app.pending_mouse_candidate.unwrap()
            } else {
                app.settings.saved()
            };
            assert_ne!(before, after);
        }
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn settings_mode_unlocked_edit_persists_before_updating_runtime() {
        let (root, mut app) =
            settings_mode_app("persist", None, UiEnvironment::default(), Language::En);
        settings_mode_select(&mut app, 2);
        assert!(app.settings.effective().color);

        handle_key(&mut app, KeyCode::Right, KeyModifiers::NONE, None);

        assert!(!app.settings.effective().color);
        assert!(!app.color_enabled);
        assert!(fs::read_to_string(root.join("tui.toml"))
            .unwrap()
            .contains("color = false"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn settings_mode_locked_edit_is_rejected_without_disk_change() {
        let environment = UiEnvironment {
            preview: Some(true),
            ..UiEnvironment::default()
        };
        let (root, mut app) = settings_mode_app("locked", None, environment, Language::ZhCn);
        settings_mode_select(&mut app, 1);

        handle_key(&mut app, KeyCode::Enter, KeyModifiers::NONE, None);

        assert!(app.settings.effective().preview);
        assert!(!root.join("tui.toml").exists());
        assert_eq!(app.notice.as_deref(), Some("此设置由环境变量锁定"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn settings_mode_write_failure_preserves_runtime_and_saved_value() {
        let root = mouse_test_root("write-failure");
        fs::create_dir_all(&root).unwrap();
        let blocker = root.join("blocker");
        fs::write(&blocker, "not a directory").unwrap();
        let loaded = UiSettings::load(blocker.join("tui.toml"), UiEnvironment::default());
        let mut app = App::with_settings(
            build_candidates(&recs(&[("/a", 0.9)])),
            loaded,
            Language::En,
            None,
        );
        settings_mode_select(&mut app, 2);

        handle_key(&mut app, KeyCode::Enter, KeyModifiers::NONE, None);

        assert!(app.settings.saved().color);
        assert!(app.settings.effective().color);
        assert!(app.color_enabled);
        assert!(app
            .notice
            .as_deref()
            .unwrap()
            .starts_with("Failed to save settings: "));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn settings_mode_persisted_defaults_and_environment_overrides_seed_runtime() {
        let (persisted_root, persisted_app) = settings_mode_app(
            "startup-persisted",
            Some("language = \"en\"\npreview = true\ncolor = false\nmouse = false\n"),
            UiEnvironment::default(),
            Language::ZhCn,
        );
        assert_eq!(persisted_app.language, Language::En);
        assert!(persisted_app.preview_visible);
        assert!(!persisted_app.color_enabled);
        assert!(!persisted_app.settings.effective().mouse);
        let _ = fs::remove_dir_all(persisted_root);

        let environment = UiEnvironment {
            language: Some(LanguagePreference::En),
            preview: Some(false),
            color: Some(true),
            mouse: Some(false),
        };
        let (root, app) = settings_mode_app(
            "startup",
            Some("language = \"zh-CN\"\npreview = true\ncolor = false\nmouse = true\n"),
            environment,
            Language::ZhCn,
        );

        assert_eq!(app.language, Language::En);
        assert!(!app.preview_visible);
        assert!(app.color_enabled);
        assert!(!app.settings.effective().mouse);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn settings_mode_preview_edit_persists_but_tab_remains_session_only() {
        let (root, mut app) =
            settings_mode_app("preview", None, UiEnvironment::default(), Language::En);
        settings_mode_select(&mut app, 1);

        handle_key(&mut app, KeyCode::Enter, KeyModifiers::NONE, None);
        assert!(app.preview_visible);
        assert!(app.settings.saved().preview);
        let persisted = fs::read_to_string(root.join("tui.toml")).unwrap();

        app.mode = Mode::Normal;
        handle_key(&mut app, KeyCode::Tab, KeyModifiers::NONE, None);
        assert!(!app.preview_visible);
        assert_eq!(
            fs::read_to_string(root.join("tui.toml")).unwrap(),
            persisted
        );
        assert!(app.settings.saved().preview);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn settings_mode_load_warning_becomes_localized_notice() {
        let (root, app) = settings_mode_app(
            "load-warning",
            Some("language = ["),
            UiEnvironment::default(),
            Language::ZhCn,
        );
        assert!(app
            .notice
            .as_deref()
            .unwrap()
            .starts_with("TUI 设置加载失败: "));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn settings_mouse_staged_candidate_applies_and_reports_runtime_results() {
        let (root, mut app) = settings_mode_app(
            "mouse-runtime",
            None,
            UiEnvironment::default(),
            Language::En,
        );
        settings_mode_select(&mut app, 3);
        let mut control = FakeMouseCaptureControl::succeeding(true);

        handle_key(&mut app, KeyCode::Enter, KeyModifiers::NONE, None);
        assert!(app.pending_mouse_candidate.is_some());
        assert!(app.settings.saved().mouse);
        app.apply_pending_mouse_setting(&mut control);

        assert!(app.pending_mouse_candidate.is_none());
        assert!(!control.mouse_capture_enabled());
        assert!(!app.settings.saved().mouse);
        assert_eq!(app.notice.as_deref(), Some("Mouse disabled"));
        assert!(fs::read_to_string(root.join("tui.toml"))
            .unwrap()
            .contains("mouse = false"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn settings_mouse_terminal_and_persistence_failures_are_localized_and_gate_actual_state() {
        let (root, mut app) = settings_mode_app(
            "mouse-terminal",
            None,
            UiEnvironment::default(),
            Language::En,
        );
        settings_mode_select(&mut app, 3);
        handle_key(&mut app, KeyCode::Enter, KeyModifiers::NONE, None);
        let mut terminal_failure = FakeMouseCaptureControl::with_results(
            true,
            vec![Err(io::Error::other("terminal failed"))],
        );
        app.apply_pending_mouse_setting(&mut terminal_failure);
        assert!(mouse_event_enabled(&terminal_failure, Mode::Normal));
        assert!(app
            .notice
            .as_deref()
            .unwrap()
            .starts_with("Failed to change terminal mouse capture: "));
        let _ = fs::remove_dir_all(root);

        let root = mouse_test_root("mouse-persist-rollback-runtime");
        fs::create_dir_all(&root).unwrap();
        let blocker = root.join("blocker");
        fs::write(&blocker, "not a directory").unwrap();
        let loaded = UiSettings::load(blocker.join("tui.toml"), UiEnvironment::default());
        let mut app = App::with_settings(
            build_candidates(&recs(&[("/a", 0.9)])),
            loaded,
            Language::ZhCn,
            None,
        );
        settings_mode_select(&mut app, 3);
        handle_key(&mut app, KeyCode::Enter, KeyModifiers::NONE, None);
        let mut successful_rollback = FakeMouseCaptureControl::succeeding(true);
        app.apply_pending_mouse_setting(&mut successful_rollback);
        assert!(mouse_event_enabled(&successful_rollback, Mode::Normal));
        assert!(app
            .notice
            .as_deref()
            .unwrap()
            .starts_with("保存鼠标设置失败: "));
        fs::remove_dir_all(root).unwrap();

        let root = mouse_test_root("mouse-persist-runtime");
        fs::create_dir_all(&root).unwrap();
        let blocker = root.join("blocker");
        fs::write(&blocker, "not a directory").unwrap();
        let loaded = UiSettings::load(blocker.join("tui.toml"), UiEnvironment::default());
        let mut app = App::with_settings(
            build_candidates(&recs(&[("/a", 0.9)])),
            loaded,
            Language::En,
            None,
        );
        settings_mode_select(&mut app, 3);
        handle_key(&mut app, KeyCode::Enter, KeyModifiers::NONE, None);
        let mut rollback_failure = FakeMouseCaptureControl::with_results(
            true,
            vec![Ok(()), Err(io::Error::other("rollback failed"))],
        );
        app.apply_pending_mouse_setting(&mut rollback_failure);
        assert!(!mouse_event_enabled(&rollback_failure, Mode::Normal));
        assert!(app.settings.saved().mouse);
        assert!(app.notice.as_deref().unwrap().contains("rollback failed"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn settings_language_change_invalidates_old_preview() {
        let (request_tx, request_rx) = mpsc::channel::<PreviewRequest>();
        let (_response_tx, response_rx) = mpsc::channel::<PreviewResponse>();
        let worker = PreviewWorker {
            requests: request_tx,
            responses: response_rx,
        };
        let root = mouse_test_root("language-preview");
        let path = root.join("tui.toml");
        fs::create_dir_all(&root).unwrap();
        fs::write(
            &path,
            "language = \"auto\"\npreview = true\ncolor = true\nmouse = true\n",
        )
        .unwrap();
        let loaded = UiSettings::load(path, UiEnvironment::default());
        let mut app = App::with_settings(
            build_candidates(&recs(&[("/a", 0.9)])),
            loaded,
            Language::En,
            Some(worker),
        );
        app.preview_generation = 7;
        app.preview_selected_path = Some("/a".to_string());
        app.preview_pending = Some(("/a".to_string(), Instant::now()));
        app.preview_loading = Some("/a".to_string());
        app.preview_current = Some(("/a".to_string(), preview_data(&["old"])));
        app.insert_preview_cache("/a".to_string(), preview_data(&["cached"]));
        settings_mode_select(&mut app, 0);

        handle_key(&mut app, KeyCode::Right, KeyModifiers::NONE, None);

        assert_eq!(app.language, Language::ZhCn);
        assert_eq!(app.preview_generation, 8);
        assert!(app.preview_cache.is_empty());
        assert!(app.preview_cache_order.is_empty());
        assert!(app.preview_selected_path.is_none());
        assert!(app.preview_pending.is_none());
        assert!(app.preview_loading.is_none());
        assert!(app.preview_current.is_none());
        assert!(app.preview_worker.is_some());
        assert!(matches!(
            request_rx.try_recv(),
            Err(mpsc::TryRecvError::Disconnected)
        ));
        assert!(!app.accept_preview_response(PreviewResponse {
            path: "/a".to_string(),
            generation: 7,
            outcome: preview_data(&["stale-language"]),
        }));
        assert!(app.preview_current.is_none());
        let _ = fs::remove_dir_all(root);
    }

    fn line_text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    fn row_options(index: usize, total: usize, selected: bool, width: usize) -> ListRowOptions {
        ListRowOptions {
            index,
            total,
            selected,
            width,
            language: Language::ZhCn,
        }
    }

    fn test_ctx(name: &str) -> (PathBuf, AppContext) {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("cdh_picker_test_{name}_{unique}"));
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
    fn language_resolution_honors_override_locale_and_fallbacks() {
        assert_eq!(
            resolve_language(Some("en-US"), &[Some("zh_CN.UTF-8")]),
            Language::En
        );
        assert_eq!(
            resolve_language(Some("zh-CN"), &[Some("en_US.UTF-8")]),
            Language::ZhCn
        );
        assert_eq!(
            resolve_language(None, &[Some("en_GB.UTF-8"), Some("zh_CN")]),
            Language::En
        );
        assert_eq!(
            resolve_language(Some("unsupported"), &[Some("zh_CN.UTF-8")]),
            Language::ZhCn
        );
        assert_eq!(resolve_language(None, &[Some("fr_FR.UTF-8")]), Language::En);
        assert_eq!(resolve_language(None, &[]), Language::ZhCn);
    }

    #[test]
    fn ui_catalog_exposes_complete_chinese_and_english_core_copy() {
        assert_eq!(
            Language::ZhCn.text(TextKey::SearchPlaceholder),
            "输入路径片段…"
        );
        assert_eq!(
            Language::En.text(TextKey::SearchPlaceholder),
            "Search paths…"
        );
        assert_eq!(Language::ZhCn.text(TextKey::MissingStatus), "失效");
        assert_eq!(Language::En.text(TextKey::MissingStatus), "missing");
        assert_eq!(
            Language::En.text(TextKey::FooterPrimary),
            "↑↓ Select · Ctrl+↑↓ Page · Enter Jump · Tab Preview · F1 Help · F2 Settings · Esc Exit"
        );
    }

    #[test]
    fn english_dynamic_copy_covers_page_time_and_delete_confirmation() {
        let page = PageWindow::new(136, 14, 10);
        assert_eq!(page.summary(136, Language::En), "11–20 / 136 · Page 2/14");
        assert_eq!(relative_time_at(Some(0), 90, Language::En), "1 minute ago");
        assert_eq!(
            relative_time_at(Some(0), 7_200, Language::En),
            "2 hours ago"
        );

        let message = confirm_delete_message("~/archive/old-project", 50, Language::En);
        assert_eq!(message, "Delete history entry “~/archive/old-project”?");
    }

    #[test]
    fn english_help_contains_no_chinese_copy() {
        let theme = Theme { on: false };
        let lines = help_lines(Language::En, &theme);
        let text = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>();
        for required in [
            "Keyboard shortcuts",
            "Movement",
            "Paging",
            "Search",
            "Actions",
        ] {
            assert!(text.contains(required));
        }
        assert!(!text
            .chars()
            .any(|character| ('\u{4e00}'..='\u{9fff}').contains(&character)));
        assert!(lines
            .iter()
            .all(|line| UnicodeWidthStr::width(line_text(line).as_str()) <= 56));
    }

    #[test]
    fn english_footer_and_missing_notice_fit_the_existing_tui_flow() {
        let full = Language::En.text(TextKey::FooterPrimary);
        let compact = Language::En.text(TextKey::FooterCompact);
        let short = Language::En.text(TextKey::FooterShort);
        assert_eq!(fit_footer(full, compact, short, 80), compact);

        let mut app = App::with_preview_worker_language(
            build_candidates(&recs_with_exists(&[("/gone", 0.9, false)])),
            None,
            false,
            Language::En,
        );
        handle_key(&mut app, KeyCode::Enter, KeyModifiers::NONE, None);
        assert_eq!(
            app.notice.as_deref(),
            Some("Directory is missing; press Ctrl+D to delete its history entry")
        );

        app.query = "not-found".to_string();
        app.query_cursor = app.query.chars().count();
        app.recompute_after_query_change();
        assert_eq!(
            line_text(&empty_state_line(
                &app.query,
                app.language,
                &Theme { on: false }
            )),
            "No matching directories · Ctrl+U Clear search"
        );
    }

    #[test]
    fn english_preview_worker_disconnect_uses_the_session_language() {
        let (request_tx, request_rx) = mpsc::channel::<PreviewRequest>();
        drop(request_rx);
        let (_response_tx, response_rx) = mpsc::channel::<PreviewResponse>();
        let worker = PreviewWorker {
            requests: request_tx,
            responses: response_rx,
        };
        let now = Instant::now();
        let mut app = App::with_preview_worker_language(
            build_candidates(&recs(&[("/a", 0.9)])),
            Some(worker),
            true,
            Language::En,
        );
        app.update_preview(now);
        app.update_preview(now + PREVIEW_DEBOUNCE + Duration::from_millis(1));
        assert_eq!(
            app.preview_current,
            Some((
                "/a".to_string(),
                PreviewOutcome::Error("Preview unavailable".to_string())
            ))
        );
    }

    #[test]
    fn page_window_handles_zero_results_without_division() {
        let page = PageWindow::new(0, 0, 10);
        assert_eq!(page.start, 0);
        assert_eq!(page.end, 0);
        assert_eq!(page.page_count, 0);
        assert_eq!(page.summary(0, Language::ZhCn), "0 / 0 · 第 0/0 页");
    }

    #[test]
    fn page_window_reports_range_and_incomplete_last_page() {
        let page = PageWindow::new(136, 14, 10);
        assert_eq!(page.start, 10);
        assert_eq!(page.end, 20);
        assert_eq!(
            page.summary(136, Language::ZhCn),
            "11–20 / 136 · 第 2/14 页"
        );

        let last = PageWindow::new(136, 135, 10);
        assert_eq!(
            (last.start, last.end, last.page, last.page_count),
            (130, 136, 14, 14)
        );
    }

    #[test]
    fn pagination_state_tracks_205_filtered_results() {
        let paths = (0..205)
            .map(|index| (format!("/workspace/project-{index}"), 0.5))
            .collect::<Vec<_>>();
        let records = paths
            .iter()
            .map(|(path, score)| (path.as_str(), *score))
            .collect::<Vec<_>>();
        let mut app = app_with_paths(&records);
        app.set_page_size(24);

        assert_eq!(app.filtered_results.len(), 205);
        assert_eq!(app.selected_index, 0);
        assert_eq!(app.current_page, 1);
        assert_eq!(app.total_pages, 9);
        assert_eq!(
            app.page().summary(205, Language::ZhCn),
            "1–24 / 205 · 第 1/9 页"
        );

        app.set_selected(24);
        assert_eq!(app.current_page, 2);
        assert_eq!(
            app.page().summary(205, Language::ZhCn),
            "25–48 / 205 · 第 2/9 页"
        );

        let page = app.page();
        let slice = &app.filtered_results[page.start..page.end];
        assert_eq!(slice.len(), 24);
        assert_eq!(slice[0].idx, 24);
        assert_eq!(slice[23].idx, 47);
    }

    #[test]
    fn page_size_uses_the_actual_list_area_after_resize() {
        let regular_area = Rect::new(0, 0, 80, 24);
        let short_area = Rect::new(0, 0, 80, 12);
        let regular = page_size_for(regular_area, false);
        let short = page_size_for(short_area, false);
        assert_eq!(
            regular,
            screen_layout(regular_area, false).unwrap().list.height as usize
        );
        assert_eq!(
            short,
            screen_layout(short_area, false).unwrap().list.height as usize
        );
        assert!(short < regular);
        assert!(short > 0);
    }

    #[test]
    fn empty_filter_resets_explicit_pagination_state() {
        let mut app = app_with_paths(&[("/workspace/api", 0.8)]);
        app.query = "does-not-match".to_string();
        app.recompute_after_query_change();
        assert!(app.filtered_results.is_empty());
        assert_eq!(app.selected_index, 0);
        assert_eq!(app.current_page, 0);
        assert_eq!(app.total_pages, 0);
        assert_eq!(app.page().summary(0, Language::ZhCn), "0 / 0 · 第 0/0 页");
    }

    #[test]
    fn empty_search_state_offers_a_visible_recovery_action() {
        let theme = Theme { on: true };
        let line = empty_state_line("does-not-match", Language::ZhCn, &theme);
        assert_eq!(line_text(&line), "未找到匹配目录 · Ctrl+U 清空搜索");
        let clear_key = line
            .spans
            .iter()
            .find(|span| span.content.as_ref() == "Ctrl+U")
            .unwrap();
        assert_eq!(clear_key.style.fg, theme.accent().fg);
        assert!(clear_key.style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn path_display_abbreviates_home_and_keeps_absolute_paths() {
        let home = "/home/jason";
        assert_eq!(PathDisplay::from_path(home, Some(home)).text, "~");
        assert_eq!(
            PathDisplay::from_path("/home/jason/workspace/project", Some(home)).text,
            "~/workspace/project"
        );
        assert_eq!(PathDisplay::from_path("/tmp", Some(home)).text, "/tmp");
        assert_eq!(PathDisplay::from_path("/", Some(home)).text, "/");
        assert_eq!(
            PathDisplay::from_path("/mnt/c/Users/Jason/Documents/Workspace", Some(home)).text,
            "/mnt/c/Users/Jason/Documents/Workspace"
        );
    }

    #[test]
    fn home_abbreviation_maps_raw_highlights_to_displayed_path() {
        let home = "/home/jason";
        let raw = "/home/jason/workspace/项目/🧭-tools";
        let display = PathDisplay::from_path(raw, Some(home));
        let home_highlights = (0..home.chars().count() as u32).collect::<Vec<_>>();
        let mapped_home = display.display_highlight_indices(&home_highlights);
        assert_eq!(mapped_home.len(), 1);
        assert!(mapped_home.contains(&0));

        let raw_project_index = raw[..raw.find('项').unwrap()].chars().count() as u32;
        let display_project_index = display.text[..display.text.find('项').unwrap()]
            .chars()
            .count();
        let mapped_project = display.display_highlight_indices(&[raw_project_index]);
        assert!(mapped_project.contains(&display_project_index));
    }

    #[test]
    fn filter_matches_original_path_after_home_abbreviation() {
        let raw = "/home/jason/workspace/api-client";
        let candidate = Candidate {
            raw: raw.to_string(),
            display: PathDisplay::from_path(raw, Some("/home/jason")),
            name: "api-client".to_string(),
            score: 1.0,
            exists: true,
            last_visit: None,
        };
        let mut filter = Filter::new();
        let matches = filter.run(std::slice::from_ref(&candidate), "/home/jason");
        assert_eq!(matches.len(), 1);
        assert!(candidate
            .display
            .display_highlight_indices(&matches[0].highlights)
            .contains(&0));
    }

    #[test]
    fn path_middle_truncation_preserves_path_start_and_terminal_component() {
        let display = PathDisplay::from_path(
            "/home/jason/workspace/repos/github.com/jasonwong1991/easy_proxies",
            Some("/home/jason"),
        );
        let visible = visible_path_text(&display, 32);
        assert!(UnicodeWidthStr::width(visible.as_str()) <= 32);
        assert!(visible.starts_with("~/workspace"));
        assert!(visible.contains('…'));
        assert!(visible.ends_with("easy_proxies"));
    }

    #[test]
    fn path_middle_truncation_is_safe_for_wide_unicode_and_tiny_widths() {
        let display = PathDisplay::from_path(
            "/home/jason/工作区/🧭-项目/非常非常长的目录",
            Some("/home/jason"),
        );
        for width in 0..=12 {
            let visible = visible_path_text(&display, width);
            assert!(UnicodeWidthStr::width(visible.as_str()) <= width);
            assert!(visible.is_char_boundary(visible.len()));
        }
    }

    #[test]
    fn page_up_down_moves_a_whole_page_at_boundaries() {
        let paths = (0..25)
            .map(|index| (format!("/p/{index}"), 0.5))
            .collect::<Vec<_>>();
        let records = paths
            .iter()
            .map(|(path, score)| (path.as_str(), *score))
            .collect::<Vec<_>>();
        let mut app = app_with_paths(&records);
        app.set_page_size(10);
        app.set_selected(8);
        assert!(app.move_page(1));
        assert_eq!(app.selected_index, 18);
        assert!(app.move_page(1));
        assert_eq!(app.selected_index, 24);
        assert!(!app.move_page(1));
        assert_eq!(app.selected_index, 24);
        assert!(app.move_page(-1));
        assert_eq!(app.selected_index, 14);
    }

    #[test]
    fn ctrl_arrows_and_page_keys_share_page_navigation() {
        let paths = (0..25)
            .map(|index| (format!("/p/{index}"), 0.5))
            .collect::<Vec<_>>();
        let records = paths
            .iter()
            .map(|(path, score)| (path.as_str(), *score))
            .collect::<Vec<_>>();
        let mut app = app_with_paths(&records);
        app.set_page_size(10);
        app.set_selected(8);

        assert_eq!(
            handle_key(
                &mut app,
                KeyCode::Down,
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
                None
            ),
            None
        );
        assert_eq!(app.selected_index, 18);
        assert_eq!(app.current_page, 2);

        assert_eq!(
            handle_key(&mut app, KeyCode::PageUp, KeyModifiers::NONE, None),
            None
        );
        assert_eq!(app.selected_index, 8);
        assert_eq!(app.current_page, 1);

        assert_eq!(
            handle_key(&mut app, KeyCode::PageDown, KeyModifiers::NONE, None),
            None
        );
        assert_eq!(app.selected_index, 18);

        assert_eq!(
            handle_key(&mut app, KeyCode::Up, KeyModifiers::CONTROL, None),
            None
        );
        assert_eq!(app.selected_index, 8);
    }

    #[test]
    fn page_keys_clamp_at_boundaries_and_on_short_last_page() {
        let paths = (0..25)
            .map(|index| (format!("/p/{index}"), 0.5))
            .collect::<Vec<_>>();
        let records = paths
            .iter()
            .map(|(path, score)| (path.as_str(), *score))
            .collect::<Vec<_>>();
        let mut app = app_with_paths(&records);
        app.set_page_size(10);

        handle_key(&mut app, KeyCode::Up, KeyModifiers::CONTROL, None);
        assert_eq!(app.selected_index, 0);

        app.set_selected(18);
        handle_key(&mut app, KeyCode::Down, KeyModifiers::CONTROL, None);
        assert_eq!(app.selected_index, 24);
        assert_eq!(app.current_page, 3);
        handle_key(&mut app, KeyCode::PageDown, KeyModifiers::NONE, None);
        assert_eq!(app.selected_index, 24);

        app.query = "not-found".to_string();
        app.query_cursor = app.query.chars().count();
        app.recompute_after_query_change();
        for (code, modifiers) in [
            (KeyCode::Up, KeyModifiers::CONTROL),
            (KeyCode::Down, KeyModifiers::CONTROL),
            (KeyCode::PageUp, KeyModifiers::NONE),
            (KeyCode::PageDown, KeyModifiers::NONE),
        ] {
            assert_eq!(handle_key(&mut app, code, modifiers, None), None);
        }
        assert_eq!(app.selected_index, 0);
        assert_eq!(app.current_page, 0);
    }

    #[test]
    fn ctrl_p_and_ctrl_n_cross_page_boundaries() {
        let mut app = app_with_paths(&[("/a", 0.9), ("/b", 0.8), ("/c", 0.7), ("/d", 0.6)]);
        app.set_page_size(2);
        app.set_selected(1);

        handle_key(&mut app, KeyCode::Char('n'), KeyModifiers::CONTROL, None);
        assert_eq!(app.selected_index, 2);
        assert_eq!(app.current_page, 2);
        handle_key(&mut app, KeyCode::Char('p'), KeyModifiers::CONTROL, None);
        assert_eq!(app.selected_index, 1);
        assert_eq!(app.current_page, 1);
    }

    #[test]
    fn left_right_move_the_query_cursor_without_paging() {
        let mut app = app_with_paths(&[("/a", 0.9), ("/b", 0.8), ("/c", 0.7)]);
        app.set_page_size(1);
        app.set_selected(1);
        app.query = "abc".to_string();
        app.query_cursor = 1;

        assert_eq!(
            handle_key(&mut app, KeyCode::Right, KeyModifiers::NONE, None),
            None
        );
        assert_eq!(app.query_cursor, 2);
        assert_eq!(app.selected_index, 1);
        assert_eq!(app.current_page, 2);

        assert_eq!(
            handle_key(&mut app, KeyCode::Left, KeyModifiers::ALT, None),
            None
        );
        assert_eq!(app.query_cursor, 1);
        assert_eq!(app.selected_index, 1);
    }

    #[test]
    fn query_editing_inserts_and_deletes_at_the_cursor() {
        let mut app = app_with_paths(&[("/abc", 0.9), ("/other", 0.8)]);
        app.query = "ac".to_string();
        app.query_cursor = 1;

        handle_key(&mut app, KeyCode::Char('b'), KeyModifiers::NONE, None);
        assert_eq!(app.query, "abc");
        assert_eq!(app.query_cursor, 2);
        assert_eq!(app.selected_index, 0);
        assert_eq!(app.current_page, 1);

        handle_key(&mut app, KeyCode::Backspace, KeyModifiers::NONE, None);
        assert_eq!(app.query, "ac");
        assert_eq!(app.query_cursor, 1);

        handle_key(&mut app, KeyCode::Delete, KeyModifiers::NONE, None);
        assert_eq!(app.query, "a");
        assert_eq!(app.query_cursor, 1);

        handle_key(&mut app, KeyCode::Char('u'), KeyModifiers::CONTROL, None);
        assert!(app.query.is_empty());
        assert_eq!(app.query_cursor, 0);
    }

    #[test]
    fn unicode_query_cursor_moves_and_deletes_by_characters() {
        let mut app = app_with_paths(&[("/中文🧭", 0.9)]);
        app.query = "中🧭文".to_string();
        app.query_cursor = 1;

        handle_key(&mut app, KeyCode::Right, KeyModifiers::NONE, None);
        assert_eq!(app.query_cursor, 2);
        handle_key(&mut app, KeyCode::Backspace, KeyModifiers::NONE, None);
        assert_eq!(app.query, "中文");
        assert_eq!(app.query_cursor, 1);

        app.query = "中🧭文".to_string();
        app.query_cursor = 1;
        handle_key(&mut app, KeyCode::Delete, KeyModifiers::NONE, None);
        assert_eq!(app.query, "中文");
        assert_eq!(app.query_cursor, 1);
    }

    #[test]
    fn query_cursor_stays_within_unicode_character_boundaries() {
        let mut app = app_with_paths(&[("/中文🧭", 0.9)]);
        app.query = "中🧭".to_string();
        app.query_cursor = 0;

        handle_key(&mut app, KeyCode::Left, KeyModifiers::NONE, None);
        assert_eq!(app.query_cursor, 0);
        handle_key(&mut app, KeyCode::Right, KeyModifiers::NONE, None);
        handle_key(&mut app, KeyCode::Right, KeyModifiers::NONE, None);
        handle_key(&mut app, KeyCode::Right, KeyModifiers::NONE, None);
        assert_eq!(app.query_cursor, 2);
        assert_eq!(split_at_char_index(&app.query, app.query_cursor).0, "中🧭");
    }

    #[test]
    fn long_query_viewport_follows_the_cursor_and_marks_hidden_edges() {
        let query = "0123456789abcdefghij";
        let viewport = query_viewport(query, 15, 8);
        assert!(viewport.left_hidden);
        assert!(viewport.right_hidden);
        assert!(viewport.before.ends_with('e'));
        assert!(viewport.after.starts_with('f'));
        assert!(viewport.display_width() <= 8);

        let at_end = query_viewport(query, query.chars().count(), 8);
        assert!(at_end.left_hidden);
        assert!(!at_end.right_hidden);
        assert!(at_end.before.ends_with('j'));
        assert!(at_end.after.is_empty());
    }

    #[test]
    fn query_viewport_preserves_unicode_boundaries_and_terminal_width() {
        let query = "项目/🧭/workspace/非常长";
        let cursor = "项目/🧭".chars().count();
        let viewport = query_viewport(query, cursor, 9);
        assert!(viewport.before.ends_with('🧭'));
        assert!(viewport.after.starts_with('/'));
        assert!(viewport.display_width() <= 9);
        assert!(viewport.before.is_char_boundary(viewport.before.len()));
        assert!(viewport.after.is_char_boundary(viewport.after.len()));

        for width in 0..=4 {
            for cursor in [0, query.chars().count() / 2, query.chars().count()] {
                assert!(query_viewport(query, cursor, width).display_width() <= width);
            }
        }
    }

    #[test]
    fn footer_uses_ctrl_arrows_without_advertising_left_right_paging() {
        let hint = Language::ZhCn.text(TextKey::FooterPrimary);
        assert_eq!(
            hint,
            "↑↓ 选择 · Ctrl+↑↓ 翻页 · Enter 跳转 · Tab 预览 · F1 帮助 · F2 设置 · Esc 退出"
        );
        assert!(!hint.contains("PgUp"));
        assert!(!hint.contains('←'));
        assert!(!hint.contains('→'));
    }

    #[test]
    fn footer_hint_visually_separates_keys_from_descriptions() {
        let theme = Theme { on: true };
        let hint = Language::ZhCn.text(TextKey::FooterPrimary);
        let line = footer_hint_line(hint, &theme);
        assert_eq!(line_text(&line), hint);

        let enter = line
            .spans
            .iter()
            .find(|span| span.content.as_ref() == "Enter")
            .unwrap();
        assert_eq!(enter.style.fg, theme.accent().fg);
        assert!(enter.style.add_modifier.contains(Modifier::BOLD));

        let separator = line
            .spans
            .iter()
            .find(|span| span.content.as_ref() == " · ")
            .unwrap();
        assert_eq!(separator.style.fg, theme.border().fg);
    }

    #[test]
    fn help_lists_both_page_key_sets_and_query_editing_controls() {
        let theme = Theme { on: false };
        let text = help_lines(Language::ZhCn, &theme)
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>();

        for required in [
            "移动",
            "分页",
            "搜索",
            "操作",
            "Ctrl+↑ / PgUp",
            "Ctrl+↓ / PgDn",
            "← / →",
            "Backspace",
            "Delete",
            "F1 / ? / ？",
        ] {
            assert!(text.contains(required), "missing help text: {required}");
        }
    }

    #[test]
    fn all_help_shortcuts_open_help_without_entering_search() {
        for (code, modifiers) in [
            (KeyCode::F(1), KeyModifiers::NONE),
            (KeyCode::Char('?'), KeyModifiers::SHIFT),
            (KeyCode::Char('？'), KeyModifiers::NONE),
        ] {
            let mut app = app_with_paths(&[("/workspace", 1.0)]);
            handle_key(&mut app, code, modifiers, None);
            assert_eq!(app.mode, Mode::Help);
            assert!(app.query.is_empty());
        }

        let mut app = app_with_paths(&[("/workspace", 1.0)]);
        handle_key(&mut app, KeyCode::Char('/'), KeyModifiers::NONE, None);
        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(app.query, "/");
    }

    #[test]
    fn up_down_cross_page_without_wrapping() {
        let mut app = app_with_paths(&[("/a", 0.9), ("/b", 0.8), ("/c", 0.7), ("/d", 0.6)]);
        app.set_page_size(2);
        app.set_selected(1);
        assert!(app.move_by(1));
        assert_eq!(app.selected_index, 2);
        assert_eq!(app.page().page, 2);
        assert!(app.move_by(-1));
        assert_eq!(app.selected_index, 1);
        assert!(app.move_by(-1));
        assert_eq!(app.selected_index, 0);
        assert!(!app.move_by(-1));
        assert_eq!(app.selected_index, 0);
    }

    #[test]
    fn home_end_select_first_and_last_result() {
        let mut app = app_with_paths(&[("/a", 0.9), ("/b", 0.8), ("/c", 0.7)]);
        app.move_end();
        assert_eq!(app.selected_index, 2);
        app.move_home();
        assert_eq!(app.selected_index, 0);
    }

    #[test]
    fn search_resets_to_first_page_and_first_result() {
        let mut app = app_with_paths(&[
            ("/work/api-one", 0.9),
            ("/work/api-two", 0.8),
            ("/work/other", 0.7),
        ]);
        app.set_page_size(1);
        app.set_selected(2);
        app.query = "api".to_string();
        app.recompute_after_query_change();
        assert_eq!(app.selected_index, 0);
        assert_eq!(app.page().page, 1);
        assert_eq!(app.filtered_results.len(), 2);
    }

    #[test]
    fn resize_page_size_keeps_current_directory_selected() {
        let mut app = app_with_paths(&[("/a", 0.9), ("/b", 0.8), ("/c", 0.7), ("/d", 0.6)]);
        app.set_selected(3);
        let selected = app.selected_raw();
        app.set_page_size(2);
        assert_eq!(app.selected_raw(), selected);
        assert_eq!(app.page().page, 2);
    }

    #[test]
    fn deleting_last_item_corrects_selection_and_page() {
        let mut app = app_with_paths(&[("/a", 0.9), ("/b", 0.8), ("/c", 0.7)]);
        app.set_page_size(2);
        app.set_selected(2);
        let idx = app.selected_candidate_idx().unwrap();
        app.remove_candidate(idx);
        assert_eq!(app.filtered_results.len(), 2);
        assert_eq!(app.selected_index, 1);
        assert_eq!(app.page().page, 1);

        let idx = app.selected_candidate_idx().unwrap();
        app.remove_candidate(idx);
        let idx = app.selected_candidate_idx().unwrap();
        app.remove_candidate(idx);
        assert!(app.filtered_results.is_empty());
        assert_eq!(app.selected_index, 0);
    }

    #[test]
    fn filter_keeps_missing_matches_after_existing_matches() {
        let candidates = build_candidates(&recs_with_exists(&[
            ("/missing/api", 0.9, false),
            ("/live/api", 0.1, true),
        ]));
        let mut filter = Filter::new();
        let matches = filter.run(&candidates, "api");
        assert_eq!(candidates[matches[0].idx].raw, "/live/api");
        assert_eq!(candidates[matches[1].idx].raw, "/missing/api");
    }

    #[test]
    fn missing_directory_enter_shows_explanation() {
        let mut app = App::with_preview_worker(
            build_candidates(&recs_with_exists(&[("/gone", 0.9, false)])),
            None,
            false,
        );
        assert_eq!(
            handle_key(&mut app, KeyCode::Enter, KeyModifiers::NONE, None),
            None
        );
        assert_eq!(
            app.notice.as_deref(),
            Some("目录已失效，按 Ctrl+D 删除历史记录")
        );
    }

    #[test]
    fn ctrl_d_confirmation_removes_history_and_candidate() {
        let (root, ctx) = test_ctx("delete");
        let stale = root.join("stale");
        let keep = root.join("keep");
        fs::create_dir_all(&keep).unwrap();
        fs::write(
            &ctx.paths.history_raw,
            format!("100\t{}\n101\t{}\n", stale.display(), keep.display()),
        )
        .unwrap();
        fs::write(
            &ctx.paths.history_uniq,
            format!("{}\n{}\n", stale.display(), keep.display()),
        )
        .unwrap();
        let mut app = App::with_preview_worker(
            build_candidates(&recs_with_exists(&[
                (keep.to_str().unwrap(), 0.8, true),
                (stale.to_str().unwrap(), 0.7, false),
            ])),
            None,
            false,
        );
        app.set_selected(1);

        handle_key(
            &mut app,
            KeyCode::Char('d'),
            KeyModifiers::CONTROL,
            Some(&ctx),
        );
        assert_eq!(app.mode, Mode::ConfirmDelete { candidate_idx: 1 });
        handle_key(
            &mut app,
            KeyCode::Char('d'),
            KeyModifiers::CONTROL,
            Some(&ctx),
        );
        assert_eq!(app.candidates.len(), 1);
        assert!(!fs::read_to_string(&ctx.paths.history_raw)
            .unwrap()
            .contains(stale.to_str().unwrap()));
        assert_eq!(
            fs::read_to_string(&ctx.paths.history_uniq).unwrap(),
            format!("{}\n", keep.display())
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn tab_toggles_preview_without_selecting_a_directory() {
        let mut app = App::with_preview_worker(build_candidates(&recs(&[("/a", 0.9)])), None, true);
        assert_eq!(
            handle_key(&mut app, KeyCode::Tab, KeyModifiers::NONE, None),
            None
        );
        assert!(!app.preview_visible);
        assert_eq!(
            handle_key(&mut app, KeyCode::Tab, KeyModifiers::NONE, None),
            None
        );
        assert!(app.preview_visible);
    }

    #[test]
    fn escape_closes_preview_before_clearing_query_or_exiting() {
        let mut app = App::with_preview_worker(build_candidates(&recs(&[("/a", 0.9)])), None, true);
        app.query = "a".to_string();
        assert_eq!(
            handle_key(&mut app, KeyCode::Esc, KeyModifiers::NONE, None),
            None
        );
        assert!(!app.preview_visible);
        assert_eq!(app.query, "a");
    }

    #[test]
    fn unicode_paths_truncate_by_display_width() {
        let value = "~/项目/emoji-🧭/非常非常长的目录";
        let middle = trim_middle(value, 12);
        let end = trim_end(value, 12);
        assert!(UnicodeWidthStr::width(middle.as_str()) <= 12);
        assert!(UnicodeWidthStr::width(end.as_str()) <= 12);
        assert!(middle.is_char_boundary(middle.len()));
        assert!(end.is_char_boundary(end.len()));
    }

    #[test]
    fn list_row_uses_one_full_path_field_and_emphasizes_its_terminal_directory() {
        let raw = "/home/jason/workspace/repos/easy_proxies";
        let candidate = Candidate {
            raw: raw.to_string(),
            display: PathDisplay::from_path(raw, Some("/home/jason")),
            name: "easy_proxies".to_string(),
            score: 1.0,
            exists: true,
            last_visit: None,
        };
        let theme = Theme { on: true };
        let line = list_row_line(&candidate, &[], row_options(1, 10, false, 80), &theme);
        let rendered = line_text(&line);

        assert!(rendered.contains("~/workspace/repos/easy_proxies"));
        let terminal = line
            .spans
            .iter()
            .rev()
            .find(|span| span.content.as_ref() == "s")
            .unwrap();
        assert!(terminal.style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn missing_list_row_keeps_status_at_the_right_in_warning_color() {
        let raw = "/archive/old-project";
        let candidate = Candidate {
            raw: raw.to_string(),
            display: PathDisplay::from_path(raw, None),
            name: "old-project".to_string(),
            score: 1.0,
            exists: false,
            last_visit: None,
        };
        let theme = Theme { on: true };
        let line = list_row_line(&candidate, &[], row_options(5, 10, false, 80), &theme);
        let rendered = line_text(&line);
        let status = line
            .spans
            .iter()
            .find(|span| span.content.as_ref() == "失效")
            .unwrap();

        assert!(rendered.contains("/archive/old-project"));
        assert!(rendered.trim_end().ends_with("失效"));
        assert_eq!(status.style.fg, theme.warning().fg);
    }

    #[test]
    fn list_row_retruncates_full_path_when_available_width_changes() {
        let raw = "/home/jason/workspace/repos/github.com/jasonwong1991/easy_proxies";
        let candidate = Candidate {
            raw: raw.to_string(),
            display: PathDisplay::from_path(raw, Some("/home/jason")),
            name: "easy_proxies".to_string(),
            score: 1.0,
            exists: true,
            last_visit: None,
        };
        let theme = Theme { on: true };
        let wide = list_row_line(&candidate, &[], row_options(0, 1, false, 80), &theme);
        let narrow = list_row_line(&candidate, &[], row_options(0, 1, false, 24), &theme);
        let wide_text = line_text(&wide);
        let narrow_text = line_text(&narrow);

        assert!(wide_text.contains("~/workspace/repos/github.com/jasonwong1991/easy_proxies"));
        assert!(narrow_text.contains('…'));
        assert!(narrow_text.contains("easy_proxies"));
        assert_eq!(UnicodeWidthStr::width(narrow_text.as_str()), 24);
    }

    #[test]
    fn delete_confirmation_keeps_its_closing_quote_when_path_is_long() {
        let message = confirm_delete_message(
            "/tmp/a-very-long-directory-name-that-must-be-truncated-before-rendering",
            32,
            Language::ZhCn,
        );
        assert!(UnicodeWidthStr::width(message.as_str()) <= 32);
        assert!(message.starts_with("删除历史记录 “"));
        assert!(message.ends_with("”？"));
    }

    #[test]
    fn search_match_in_selected_row_keeps_the_same_background() {
        let candidate = Candidate {
            raw: "/projects/api-client".to_string(),
            display: PathDisplay::from_path("/projects/api-client", None),
            name: "api-client".to_string(),
            score: 1.0,
            exists: true,
            last_visit: None,
        };
        let theme = Theme { on: true };
        let line = list_row_line(
            &candidate,
            &[10, 11, 12],
            row_options(0, 1, true, 80),
            &theme,
        );
        let selected_background = theme.selected().bg;
        let marker = line.spans.first().unwrap();
        assert_eq!(marker.content.as_ref(), "›");
        assert_eq!(marker.style.fg, theme.match_color().into());
        assert!(line
            .spans
            .iter()
            .all(|span| span.style.bg == selected_background));
    }

    #[test]
    fn colorless_selected_row_keeps_one_continuous_reverse_style() {
        let candidate = Candidate {
            raw: "/projects/api-client".to_string(),
            display: PathDisplay::from_path("/projects/api-client", None),
            name: "api-client".to_string(),
            score: 1.0,
            exists: true,
            last_visit: None,
        };
        let theme = Theme { on: false };
        let line = list_row_line(
            &candidate,
            &[10, 11, 12],
            row_options(0, 1, true, 40),
            &theme,
        );
        assert!(line
            .spans
            .iter()
            .all(|span| span.style.add_modifier.contains(Modifier::REVERSED)));
    }

    #[test]
    fn search_match_in_unselected_path_does_not_add_a_background() {
        let raw = "/home/jason/workspace/api-client";
        let candidate = Candidate {
            raw: raw.to_string(),
            display: PathDisplay::from_path(raw, Some("/home/jason")),
            name: "api-client".to_string(),
            score: 1.0,
            exists: true,
            last_visit: None,
        };
        let theme = Theme { on: true };
        let highlights = (raw[..raw.find("api-client").unwrap()].chars().count() as u32
            ..raw.chars().count() as u32)
            .collect::<Vec<_>>();
        let line = list_row_line(
            &candidate,
            &highlights,
            row_options(0, 1, false, 80),
            &theme,
        );
        let highlighted = line
            .spans
            .iter()
            .find(|span| span.style.fg == theme.match_color().into())
            .unwrap();

        assert!(highlighted.style.add_modifier.contains(Modifier::BOLD));
        assert!(highlighted.style.bg.is_none());
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
        assert!(!app.accept_preview_response(PreviewResponse {
            path: "/a".to_string(),
            generation: 1,
            outcome: preview_data(&["old"]),
        }));
        assert!(app.preview_current.is_none());
        assert!(app.preview_cache.is_empty());
    }

    #[test]
    fn preview_cache_hit_does_not_start_a_worker_request() {
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
    fn preview_worker_disconnect_is_reported_without_panicking() {
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
    fn preview_layout_uses_side_then_bottom_then_notice() {
        assert!(screen_layout(Rect::new(0, 0, 110, 24), true)
            .unwrap()
            .preview
            .is_some());
        assert!(screen_layout(Rect::new(0, 0, 80, 24), true)
            .unwrap()
            .preview
            .is_some());
        assert!(
            screen_layout(Rect::new(0, 0, 60, 24), true)
                .unwrap()
                .preview_unavailable
        );
    }

    #[test]
    fn read_git_info_reports_clean_and_modified_status() {
        if Command::new("git").arg("--version").output().is_err() {
            return;
        }
        let (root, _) = test_ctx("git_status");
        let repo = root.join("repo");
        fs::create_dir_all(&repo).unwrap();
        assert!(Command::new("git")
            .args(["init", "-q"])
            .current_dir(&repo)
            .status()
            .unwrap()
            .success());
        assert_eq!(read_git_info(&repo).unwrap().dirty, Some(false));
        fs::write(repo.join("note.txt"), "dirty").unwrap();
        assert_eq!(read_git_info(&repo).unwrap().dirty, Some(true));
        let _ = fs::remove_dir_all(root);
    }
}
