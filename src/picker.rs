//! Keyboard-first interactive directory picker.
//!
//! The picker keeps ranking and filesystem work outside the rendering path:
//! filtering happens on input, preview I/O runs on a dedicated worker, and
//! drawing only formats the current page of already prepared data.

#[path = "picker_cube.rs"]
mod cube;
#[path = "picker_git.rs"]
mod git;
#[path = "picker_i18n.rs"]
mod i18n;
#[path = "picker_overlays.rs"]
mod overlays;
#[path = "tui_settings.rs"]
mod settings;
#[cfg(test)]
#[path = "picker_tests.rs"]
mod tests;
#[path = "picker_theme.rs"]
mod theme;

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet, VecDeque};
use std::env;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::{self, IsTerminal};
use std::path::Path;
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
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Clear, Paragraph},
    Frame, Terminal,
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use regex::Regex;

use crate::discover;
use crate::excludes::{self, Excludes};
use crate::recommend::Recommendation;
use crate::{history, AppContext, Paths};
#[cfg(test)]
use i18n::resolve_language;
use i18n::{detect_locale_language, Language, TextKey};
use settings::{
    LanguagePreference, SettingKey, SettingLocks, SettingsLoad, UiEnvironment, UiPreferences,
    UiSettings,
};
use theme::{Theme, ThemeChoice};

const MIN_HEIGHT: u16 = 8;
const DOUBLE_CLICK_MS: u128 = 300;
const PREVIEW_ENTRY_LIMIT: usize = 16;
const PREVIEW_CACHE_LIMIT: usize = 50;
const PREVIEW_DEBOUNCE: Duration = Duration::from_millis(100);
const PREVIEW_SIDE_MIN_WIDTH: u16 = 108;
const PREVIEW_BOTTOM_MIN_WIDTH: u16 = 70;
const PREVIEW_BOTTOM_MIN_HEIGHT: u16 = 18;
const EVENT_POLL_FALLBACK: Duration = Duration::from_millis(100);
/// Cube columns plus one blank separator column, carved out of the content area.
const CORNER_3D_GUTTER: u16 = cube::WIDTH + 1;
/// Content width that must survive the gutter before the cube is allowed at
/// all. Ambient decoration never costs the list room it actually needs, so on
/// narrower terminals the cube simply does not appear.
const CORNER_3D_MIN_CONTENT: u16 = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
struct PreviewEntry {
    name: String,
    is_dir: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PreviewData {
    git: Option<git::GitInfo>,
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
            git: git::read_git_info(path),
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

fn preview_error_message(error: &io::Error, language: Language) -> String {
    match error.kind() {
        io::ErrorKind::PermissionDenied => language.text(TextKey::PermissionDenied).to_string(),
        io::ErrorKind::NotFound => language.text(TextKey::DirectoryMissing).to_string(),
        _ => error.to_string(),
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
    // Non-interactive callers keep the established contract: the top
    // recommendation, or nothing when history is empty.
    if !io::stderr().is_terminal() || !io::stdin().is_terminal() {
        return Ok(items.first().map(|item| item.path.clone()));
    }
    // Interactive: open the picker even with an empty history. The directory-tree
    // discovery layer -- and the $PWD bootstrap -- give it candidates to show.
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
        // Before any terminal mutation, so the hook is already in place if the
        // setup sequence below is itself what panics.
        install_terminal_panic_hook();
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

/// Undo `TermGuard::enter`'s screen changes: mouse reporting off, cursor back,
/// then back to the primary screen.
///
/// Split out of `Drop` so the panic hook can share it, and generic over the
/// writer so the ordering can be asserted in tests. Leaving the alternate screen
/// must be the *last* thing written -- the panic hook prints the panic message
/// afterwards, and anything printed before this point would land on the screen
/// that is about to be discarded.
fn restore_screen<W: io::Write>(writer: &mut W, mouse: bool) -> io::Result<()> {
    // Best-effort, not fail-fast: a failed mouse-disable must not stop the
    // alternate-screen exit. The panic hook always passes `true` here, so
    // propagating that error would leave the terminal in the exact broken
    // state this function exists to prevent.
    if mouse {
        let _ = execute!(writer, DisableMouseCapture);
    }
    execute!(writer, Show, LeaveAlternateScreen)
}

/// Restore the terminal before a panic kills the process.
///
/// Release builds set `panic = "abort"`, so nothing unwinds and
/// `TermGuard::drop` never runs. Debug builds unwind and `Drop` covers them,
/// which is why this failure never shows up locally -- only on users' machines.
/// Panic hooks still run before the abort, so this is the only place the restore
/// can happen.
///
/// Chains to the previous hook so the default message and `RUST_BACKTRACE`
/// handling stay intact. This also covers the preview and discovery worker
/// threads: a panic on either aborts the whole process, and the main thread's
/// guard never gets a chance to drop.
fn install_terminal_panic_hook() {
    static INSTALLED: std::sync::Once = std::sync::Once::new();
    INSTALLED.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            // Unconditionally disabling mouse capture: the hook cannot see the
            // guard's current flag, and telling a terminal to stop reporting
            // mouse events it never started reporting is a no-op.
            let _ = restore_screen(&mut io::stderr(), true);
            let _ = disable_raw_mode();
            previous(info);
        }));
    });
}

impl Drop for TermGuard {
    fn drop(&mut self) {
        let _ = restore_screen(&mut io::stderr(), self.mouse);
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

/// 候选来源。发现层的唯一结构改动：用来在同一模糊分档内让历史候选排在目录树
/// 候选之前（历史是用户真实去过的地方，权重更高），且用来禁用发现行的 Ctrl+D。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CandidateSource {
    History,
    Discovered,
}

struct Candidate {
    raw: String,
    score: f32,
    exists: bool,
    last_visit: Option<i64>,
    source: CandidateSource,
}

impl Candidate {
    /// Directory name (the last path segment), borrowed from `raw`.
    ///
    /// Deliberately not a stored field: it is read only by the preview header,
    /// i.e. for the single selected row, while storing it would cost a `String`
    /// per candidate (~55 bytes with header and allocator overhead, ~2.7 MB
    /// across a full 50k discovery pool). Same reasoning as `PathDisplay` below.
    fn name(&self) -> &str {
        directory_name_str(&self.raw)
    }

    /// Build the abbreviated display form on demand. `PathDisplay` is heavy
    /// (a `Range` per character for highlight mapping), so it is never stored on
    /// the candidate -- only the ~20 visible rows and the delete confirmation
    /// materialize it, which keeps a 50k-candidate pool well under budget.
    fn display(&self, home: Option<&str>) -> PathDisplay {
        PathDisplay::from_path(&self.raw, home)
    }
}

#[cfg(test)]
fn build_candidates(items: &[Recommendation]) -> Vec<Candidate> {
    build_candidates_with_visits(items, &HashMap::new())
}

fn build_candidates_with_visits(
    items: &[Recommendation],
    last_visits: &HashMap<String, i64>,
) -> Vec<Candidate> {
    items
        .iter()
        .map(|item| Candidate {
            raw: item.path.clone(),
            score: item.score.clamp(0.0, 1.0) as f32,
            exists: item.exists,
            last_visit: last_visits.get(&item.path).copied(),
            source: CandidateSource::History,
        })
        .collect()
}

/// Sort weight for the source tiebreak: smaller ranks first, so History (0)
/// precedes Discovered (1) whenever the fuzzy score ties.
fn source_rank(source: CandidateSource) -> u8 {
    match source {
        CandidateSource::History => 0,
        CandidateSource::Discovered => 1,
    }
}

/// Normalize a path for dedup: drop a trailing slash (but keep root "/").
fn normalize_path(path: &str) -> String {
    normalized_path_str(path).to_string()
}

/// `normalize_path` 的借用版本：只做尾斜杠归一，不分配。
fn normalized_path_str(path: &str) -> &str {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        "/"
    } else {
        trimmed
    }
}

/// 已入池路径的 64 位指纹，供 `App::known_paths` 去重。
///
/// 存指纹而不是路径本身：5 万条候选下，一份完整路径拷贝要 8–10 MB（平均路径 72
/// 字节，加 `String` 头与哈希表开销约 8–10 倍放大），指纹只要约 0.8 MB。
///
/// 碰撞代价很轻：某个「发现来的」目录被误判为已在池中、这一轮不出现在列表里。
/// 不是数据损坏，也不影响历史记录；该目录一旦被 cd 过就会经由历史进入候选池。
/// 5 万条下的碰撞概率约 `n²/2^65` ≈ 7e-11，可以忽略。
fn path_fingerprint(path: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    normalized_path_str(path).hash(&mut hasher);
    hasher.finish()
}

/// The parent directory of `path`, normalized. Root has no parent.
fn parent_dir(path: &str) -> Option<String> {
    Path::new(path)
        .parent()
        .map(|parent| normalize_path(&parent.to_string_lossy()))
        .filter(|parent| !parent.is_empty())
}

/// Last path segment, borrowed from `path`. `path` is already UTF-8, so the
/// segment is too and `to_str` cannot fail; paths without one (`/`) yield the
/// whole input, matching the previous owned implementation.
fn directory_name_str(path: &str) -> &str {
    Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or(path)
}

/// Build the discovery layer's ordering key: parent directory -> best history
/// score in that directory. A discovered candidate looks up its own parent, so
/// siblings of a hot history entry inherit that entry's heat, while directories
/// in corners history never touched map to 0. Keyed on the parent (not the path
/// itself) precisely so the level-1 siblings the scan surfaces get a signal.
fn build_score_map(items: &[Recommendation]) -> HashMap<String, f32> {
    let mut map: HashMap<String, f32> = HashMap::new();
    for item in items {
        let score = item.score.clamp(0.0, 1.0) as f32;
        if let Some(parent) = parent_dir(&item.path) {
            map.entry(parent)
                .and_modify(|existing| *existing = existing.max(score))
                .or_insert(score);
        }
    }
    map
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
}

struct Filter {
    matcher: Matcher,
    buffer: Vec<char>,
    hide_hidden: bool,
}

impl Filter {
    fn new() -> Self {
        Self {
            matcher: Matcher::new(Config::DEFAULT.match_paths()),
            buffer: Vec::new(),
            hide_hidden: false,
        }
    }

    fn toggle_hidden(&mut self) -> bool {
        self.hide_hidden = !self.hide_hidden;
        self.hide_hidden
    }

    fn accepts(&self, candidate: &Candidate) -> bool {
        !self.hide_hidden || !discover::path_has_hidden_component(&candidate.raw)
    }

    /// An empty query preserves recommendation order (history first, then the
    /// discovered tree). Fuzzy matches rank by matcher quality, then by source
    /// (History before Discovered -- a place the user actually visited outranks
    /// a merely-existing sibling at the same fuzzy score), then by the existing
    /// recommendation score; stale paths remain after valid paths so they can be
    /// cleaned up without competing with jump targets.
    ///
    /// Only the *ranking* score is computed here -- one `pattern.score` pass per
    /// candidate. Match highlight indices are deliberately NOT computed: a query
    /// can hit tens of thousands of candidates while only ~20 rows are ever on
    /// screen, so highlighting is deferred to the render path and run for visible
    /// rows only (see `compute_row_highlights`). Dropping the second per-match
    /// pass roughly halves filter cost on wide result sets.
    fn run(&mut self, candidates: &[Candidate], query: &str) -> Vec<Match> {
        let query = query.trim();
        if query.is_empty() {
            return candidates
                .iter()
                .enumerate()
                .filter(|(_, candidate)| self.accepts(candidate))
                .map(|(idx, _)| Match { idx })
                .collect();
        }

        let pattern = Pattern::parse(query, CaseMatching::Ignore, Normalization::Smart);
        let mut valid = Vec::new();
        let mut missing = Vec::new();

        for (idx, candidate) in candidates.iter().enumerate() {
            if !self.accepts(candidate) {
                continue;
            }
            let haystack = Utf32Str::new(&candidate.raw, &mut self.buffer);
            let Some(score) = pattern.score(haystack, &mut self.matcher) else {
                continue;
            };

            if candidate.exists {
                valid.push((score, idx));
            } else {
                missing.push(idx);
            }
        }

        valid.sort_by(|left, right| {
            right
                .0
                .cmp(&left.0)
                .then_with(|| {
                    source_rank(candidates[left.1].source)
                        .cmp(&source_rank(candidates[right.1].source))
                })
                .then_with(|| {
                    candidates[right.1]
                        .score
                        .partial_cmp(&candidates[left.1].score)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
        });

        valid
            .into_iter()
            .map(|(_, idx)| Match { idx })
            .chain(missing.into_iter().map(|idx| Match { idx }))
            .collect()
    }
}

/// Compute raw match-highlight indices for a single path, on demand. Run only
/// for on-screen rows (see `Filter::run`'s note), so the O(pool) filter never
/// pays for highlighting candidates the user will not see.
fn compute_row_highlights(matcher: &mut Matcher, raw: &str, query: &str) -> Vec<u32> {
    let query = query.trim();
    if query.is_empty() {
        return Vec::new();
    }
    let mut haystack_buffer = Vec::new();
    let mut highlights = Vec::new();
    let pattern = Pattern::parse(query, CaseMatching::Ignore, Normalization::Smart);
    let _ = pattern.indices(
        Utf32Str::new(raw, &mut haystack_buffer),
        matcher,
        &mut highlights,
    );
    highlights.sort_unstable();
    highlights.dedup();
    highlights
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

fn env_flag_enabled(name: &str, default: bool) -> bool {
    match env::var(name) {
        Ok(value) => env_truthy(&value, default),
        Err(_) => default,
    }
}

fn env_truthy(value: &str, default: bool) -> bool {
    let value = value.trim();
    if value.is_empty() {
        return default;
    }
    !(value.eq_ignore_ascii_case("0")
        || value.eq_ignore_ascii_case("false")
        || value.eq_ignore_ascii_case("off")
        || value.eq_ignore_ascii_case("no"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Normal,
    Help,
    Settings {
        selected: usize,
    },
    ConfirmDelete {
        candidate_idx: usize,
    },
    /// The exclusion list: the only place an exclusion can be undone, since an
    /// excluded directory is by definition absent from the candidate pool and
    /// so can never be selected to un-exclude it.
    Excludes {
        selected: usize,
    },
}

struct App {
    settings: UiSettings,
    language: Language,
    locale_language: Language,
    color_enabled: bool,
    theme_choice: ThemeChoice,
    pending_mouse_candidate: Option<UiPreferences>,
    candidates: Vec<Candidate>,
    filter: Filter,
    query: String,
    /// Grapheme-cluster offset in `query`, never a UTF-8 byte offset.
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
    /// List geometry published by the last successful draw, consumed by the
    /// mouse handler to map clicks back onto result rows. Rendering itself no
    /// longer reaches back here: `draw` returns the geometry and the event
    /// loop stores it, so a render pass cannot mutate state mid-frame.
    last_list_area: Rect,
    last_list_start: usize,
    /// Monotonic clock origin for the ambient corner wireframe cube.
    corner_anim_started: Instant,
    /// The `CDH_CORNER_3D` opt-out, read once at startup. Environment cannot
    /// change under a running process, and resolving it here keeps `env::var`
    /// out of the layout and render paths -- which run every animation frame --
    /// and lets tests exercise the opt-out without mutating process state.
    corner_3d_env: bool,
    /// Streamed batches from directory-tree discovery workers. Empty when
    /// discovery is disabled (`CDH_DISCOVER=0`), non-interactive, or every scan
    /// has finished (a disconnected channel is dropped). A list rather than one
    /// slot because un-excluding a directory starts a top-up scan of just that
    /// subtree, which can overlap the still-running startup scan.
    discover_rx: Vec<mpsc::Receiver<Vec<String>>>,
    /// Parent directory -> best recommendation score. A discovered candidate
    /// inherits the score of the neighborhood it sits in (a sibling of a hot
    /// history entry ranks with that entry's heat); parents unknown to history
    /// map to 0. This is the internal ordering key for the discovery layer --
    /// no fabricated frecency, just "how hot is this corner of the tree".
    discover_score_map: HashMap<String, f32>,
    /// `$HOME` for abbreviating paths at render time, captured once.
    home: Option<String>,
    /// Fingerprints (see `path_fingerprint`) of the normalized raw paths already
    /// present, so streamed discoveries dedup against history (history wins) and
    /// against each other. Storing hashes rather than the paths keeps this off the
    /// per-candidate memory bill — see `path_fingerprint` for the trade-off.
    known_paths: HashSet<u64>,
    /// Index where the discovered slice begins in `candidates` (== the history
    /// count). The prefix `[0, discovered_start)` is history in frecency order
    /// and never reordered; the suffix is the discovery layer, kept sorted by
    /// (score desc, path asc) so the empty-query view is deterministic.
    discovered_start: usize,
    /// User exclusion list, subtree semantics. Nothing under an excluded root
    /// reaches the pool: history candidates are filtered at startup, discovered
    /// ones in `ingest_discovered`, and the scan worker takes the same set as a
    /// prune set so the subtree is never even `read_dir`'d.
    excludes: Excludes,
    /// `CDH_IGNORE_RE`. History candidates were already filtered by it inside
    /// the recommend pipeline, but discovered ones are pushed straight into
    /// `ingest_discovered` and bypass that pipeline entirely -- without this
    /// second check the user's regex silently stops applying to ~98% of the pool.
    ignore_re: Option<Regex>,
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
        let known_paths = candidates
            .iter()
            .map(|candidate| path_fingerprint(&candidate.raw))
            .collect::<HashSet<_>>();
        let discovered_start = candidates.len();
        let mut app = Self {
            settings: loaded.settings,
            language,
            locale_language,
            color_enabled: effective.color,
            theme_choice: effective.theme,
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
            // No frame has been drawn yet, so the area has no height and the
            // click guard below rejects everything until a draw succeeds.
            last_list_area: Rect::new(0, 0, 0, 0),
            last_list_start: 0,
            corner_anim_started: Instant::now(),
            corner_3d_env: env_flag_enabled("CDH_CORNER_3D", true),
            discover_rx: Vec::new(),
            discover_score_map: HashMap::new(),
            home: env::var("HOME").ok().filter(|home| !home.is_empty()),
            known_paths,
            discovered_start,
            excludes: Excludes::default(),
            ignore_re: None,
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

    /// Snapshot everything rendering reads, once per frame. Drawing then
    /// consumes only the snapshot, never `App` -- no chance for a render to
    /// disagree with the state another renderer saw in the same frame. The
    /// wall clock arrives as a parameter so no renderer reads one itself.
    fn view(&self, now_unix: i64) -> FrameView<'_> {
        let preview_panel = match self.selected_candidate() {
            Some(candidate) => preview_outcome_for_selected(self, candidate),
            // No selection means the preview header shows the no-selection
            // message before the panel outcome is ever consulted.
            None => PreviewPanelOutcome::Loading,
        };
        FrameView {
            language: self.language,
            prefs: self.settings.effective(),
            locked: self.settings.locks(),
            exclude_roots: self.excludes.roots(),
            home: self.home.as_deref(),
            query: &self.query,
            query_cursor: self.query_cursor,
            results: &self.filtered_results,
            candidates: &self.candidates,
            page: self.page(),
            selected_index: self.selected_index,
            preview_visible: self.preview_visible,
            preview_panel,
            notice: self.notice.as_deref(),
            mode: self.mode,
            corner: self.corner_3d_enabled(),
            now_unix,
        }
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

    /// The ambient cube is opt-out, color-only chrome: it needs both an
    /// environment that has not switched it off and a palette to draw with.
    /// Colorless mode has no accent to shade against, so the cube would be a
    /// featureless smudge rather than a depth cue.
    fn corner_3d_enabled(&self) -> bool {
        self.color_enabled && self.corner_3d_env
    }

    /// Where the cube's tumble has reached. The clock is the picker's (the cube
    /// module never reads one); the pose the elapsed time maps to is the cube's.
    fn corner_anim_angle(&self, now: Instant) -> f32 {
        cube::spin_angle(now.saturating_duration_since(self.corner_anim_started))
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

    /// Merge streamed discovery batches into the candidate pool. Called once per
    /// event-loop drain with every pending batch, so the O(pool) refilter runs a
    /// single time no matter how many batches arrived -- the reason we drain-all
    /// before refiltering is that not doing so would smear ~100 batches x a
    /// ~25k-deep pool of rescoring across the fill window and stutter typing.
    /// Returns whether anything changed (and the view needs a redraw).
    fn ingest_discovered(&mut self, batches: Vec<Vec<String>>) -> bool {
        let selected_path = self.selected_raw();
        let mut added = false;
        for batch in batches {
            for raw in batch {
                // Checked before the fingerprint insert so a filtered path stays
                // genuinely unknown: batches already in flight when the user
                // excludes something still have to be dropped here, and the
                // prune set only stops descent, not paths already queued.
                if self.excludes.contains(&raw)
                    || self
                        .ignore_re
                        .as_ref()
                        .is_some_and(|pattern| pattern.is_match(&raw))
                {
                    continue;
                }
                if !self.known_paths.insert(path_fingerprint(&raw)) {
                    // History or an earlier discovery already owns this path.
                    continue;
                }
                let score = parent_dir(&raw)
                    .and_then(|parent| self.discover_score_map.get(&parent).copied())
                    .unwrap_or(0.0);
                self.candidates.push(Candidate {
                    raw,
                    score,
                    exists: true,
                    last_visit: None,
                    source: CandidateSource::Discovered,
                });
                added = true;
            }
        }
        if !added {
            return false;
        }
        // Keep the discovered suffix ordered by (score desc, path asc): the empty
        // query renders candidates in vec order and the history prefix must stay
        // untouched, so only the suffix is (re)sorted.
        self.candidates[self.discovered_start..].sort_by(|left, right| {
            right
                .score
                .partial_cmp(&left.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| left.raw.cmp(&right.raw))
        });
        self.filtered_results = self.filter.run(&self.candidates, &self.query);
        self.restore_selected_path(selected_path.as_deref());
        true
    }

    /// Empty-history bootstrap: synchronously seed the pool from `$PWD` -- its
    /// ancestor chain plus one level of children -- so an interactive picker with
    /// no history still has something to jump to on the first frame. Pure string
    /// splitting plus a single `read_dir`; the full tree scan streams in behind.
    fn bootstrap_from_pwd(&mut self, pwd: &str) {
        let mut seeds: Vec<String> = Vec::new();
        let mut current = normalize_path(pwd);
        loop {
            seeds.push(current.clone());
            match parent_dir(&current) {
                Some(parent) if parent != current => current = parent,
                _ => break,
            }
        }
        if let Ok(entries) = fs::read_dir(pwd) {
            for entry in entries.flatten() {
                let is_dir = entry
                    .file_type()
                    .map(|file_type| file_type.is_dir() && !file_type.is_symlink())
                    .unwrap_or(false);
                if is_dir {
                    seeds.push(entry.path().to_string_lossy().into_owned());
                }
            }
        }
        self.ingest_discovered(vec![seeds]);
    }

    fn recompute_after_query_change(&mut self) {
        self.clamp_query_cursor();
        self.filtered_results = self.filter.run(&self.candidates, &self.query);
        self.selected_index = 0;
        self.sync_pagination();
        self.notice = None;
        self.invalidate_preview_selection();
    }

    fn query_grapheme_count(&self) -> usize {
        grapheme_count(&self.query)
    }

    fn clamp_query_cursor(&mut self) {
        self.query_cursor = self.query_cursor.min(self.query_grapheme_count());
    }

    fn query_byte_index(&self, grapheme_index: usize) -> usize {
        byte_index_at_grapheme(&self.query, grapheme_index)
    }

    fn move_query_cursor(&mut self, delta: isize) -> bool {
        self.clamp_query_cursor();
        let next = if delta < 0 {
            self.query_cursor.saturating_sub((-delta) as usize)
        } else {
            self.query_cursor
                .saturating_add(delta as usize)
                .min(self.query_grapheme_count())
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
        if self.query_cursor >= self.query_grapheme_count() {
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

    fn toggle_hidden_directories(&mut self) {
        let selected_path = self.selected_raw();
        let hidden = self.filter.toggle_hidden();
        self.filtered_results = self.filter.run(&self.candidates, &self.query);
        self.restore_selected_path(selected_path.as_deref());
        self.notice = Some(
            self.language
                .text(if hidden {
                    TextKey::HiddenDirectoriesFiltered
                } else {
                    TextKey::HiddenDirectoriesShown
                })
                .to_string(),
        );
        self.invalidate_preview_selection();
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

    /// Drop `root` and everything under it from the pool.
    ///
    /// Subtree rather than single row, because that is what the exclusion list
    /// stores: excluding `~/miniforge3` while its 6,000 children stayed on
    /// screen would read as a broken delete.
    ///
    /// Fingerprints have to leave `known_paths` with the rows. They used to
    /// stay -- an excluded path could not be produced again, so dropping them
    /// only cost hashing -- but `unexclude` now re-scans the subtree, and every
    /// re-emitted path would dedup away against its own stale fingerprint. The
    /// undo would silently restore nothing, in exactly the "I just excluded the
    /// wrong row" case the panel exists for.
    fn exclude_subtree(&mut self, root: &str) {
        let selected = self.selected_index;
        // Keep `discovered_start` pointing at the first discovered candidate.
        // Every removal inside the history prefix shifts the whole discovered
        // suffix left; forgetting this leaves the start stale-high, which both
        // drops the leading discovered rows out of the sort window and, once the
        // start exceeds the pool length, panics the next `[start..]` slice.
        let discovered_start = self.discovered_start;
        let mut removed_from_history = 0;
        let mut idx = 0;
        let mut orphaned = Vec::new();
        self.candidates.retain(|candidate| {
            let keep = !discover::under_prefix(&candidate.raw, root);
            if !keep {
                if idx < discovered_start {
                    removed_from_history += 1;
                }
                orphaned.push(path_fingerprint(&candidate.raw));
            }
            idx += 1;
            keep
        });
        for fingerprint in orphaned {
            self.known_paths.remove(&fingerprint);
        }
        self.discovered_start -= removed_from_history;
        self.filtered_results = self.filter.run(&self.candidates, &self.query);
        self.selected_index = selected.min(self.filtered_results.len().saturating_sub(1));
        self.sync_pagination();
        self.notice = Some(self.language.text(TextKey::HistoryDeleted).to_string());
        self.invalidate_preview_selection();
    }

    /// Drop one entry from the exclusion list and bring its subtree straight
    /// back into the pool.
    ///
    /// The top-up scan matters: the startup scan's prune set was fixed when it
    /// spawned and will not revisit the subtree, so without this the undo would
    /// only take effect on the next launch -- which is the exact
    /// "it worked, but not until tomorrow" behaviour that made the old delete
    /// semantics confusing in the first place.
    fn unexclude(&mut self, selected: usize, ctx: Option<&AppContext>) {
        let Some(root) = self.excludes.roots().get(selected).cloned() else {
            return;
        };
        let Some(ctx) = ctx else {
            self.notice = Some(
                self.language
                    .text(TextKey::HistoryWriteUnavailable)
                    .to_string(),
            );
            return;
        };
        match excludes::remove(&ctx.paths.excludes, &root) {
            Ok(excludes) => {
                self.excludes = excludes;
                let rescanning = match discover::spawn_subtree(root, self.excludes.prune_set()) {
                    Some(rx) => {
                        self.discover_rx.push(rx);
                        true
                    }
                    // `CDH_DISCOVER=0`: there is no scan to run, so the
                    // directory only returns if it is still in history.
                    None => false,
                };
                self.mode = Mode::Excludes {
                    selected: selected.min(self.excludes.roots().len().saturating_sub(1)),
                };
                self.notice = Some(
                    self.language
                        .text(if rescanning {
                            TextKey::ExcludeRemoved
                        } else {
                            TextKey::ExcludeRemovedNoRescan
                        })
                        .to_string(),
                );
            }
            Err(error) => {
                self.notice = Some(format!(
                    "{}{error}",
                    self.language.text(TextKey::ExcludeFailedPrefix)
                ));
            }
        }
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
            SettingKey::Theme => self.theme_choice = effective.theme,
            SettingKey::Language | SettingKey::Mouse => {}
        }
    }

    fn cycle_theme(&mut self, direction: isize) {
        if self.settings.is_locked(SettingKey::Theme) {
            self.notice = Some(self.language.text(TextKey::SettingsLocked).to_string());
            return;
        }
        let Some(candidate) = self.settings.candidate(SettingKey::Theme, direction) else {
            self.notice = Some(self.language.text(TextKey::SettingsLocked).to_string());
            return;
        };
        match self.settings.persist(candidate) {
            Ok(()) => {
                self.theme_choice = candidate.theme;
                self.notice = Some(format!(
                    "{}: {}",
                    self.language.text(TextKey::SettingTheme),
                    overlays::theme_choice_label(self.language, candidate.theme)
                ));
            }
            Err(error) => {
                self.notice = Some(format!(
                    "{}{error}",
                    self.language.text(TextKey::SettingsSaveFailedPrefix)
                ));
            }
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

    // Exclusions are applied before the pool is built, not after: a filtered-out
    // candidate must never reach `known_paths`, or the scan would treat it as
    // already-owned and the row would be missing from history and discovery both.
    let excludes = ctx
        .map(|context| Excludes::load(&context.paths.excludes))
        .unwrap_or_default();
    let items: Vec<Recommendation> = items
        .iter()
        .filter(|item| !excludes.contains(&item.path))
        .cloned()
        .collect();

    let mut app = App::new(
        build_candidates_with_visits(&items, &last_visits),
        loaded,
        locale_language,
    );
    app.excludes = excludes;
    app.ignore_re = ctx.and_then(|context| context.config.ignore_re.clone());

    // Wire up the directory-tree discovery layer. The score map orders the
    // discovered candidates; the scan worker streams new paths in on a channel
    // and takes the exclusion list as a prune set, so excluded subtrees cost no
    // I/O at all rather than being filtered out after the fact.
    app.discover_score_map = build_score_map(&items);
    app.discover_rx.extend(discover::spawn(
        items.iter().map(|item| item.path.clone()).collect(),
        app.excludes.prune_set(),
    ));
    // Empty-history bootstrap: seed from $PWD before the loop so the first frame
    // isn't blank while the full scan spins up.
    if app.candidates.is_empty() {
        if let Ok(pwd) = env::current_dir() {
            app.bootstrap_from_pwd(&pwd.to_string_lossy());
        }
    }

    let mut guard = TermGuard::enter(initial_mouse)?;
    let backend = CrosstermBackend::new(io::stderr());
    let mut terminal = Terminal::new(backend)?;
    let mut dirty = true;

    loop {
        // Drain every pending discovery batch, then merge and refilter once.
        if !app.discover_rx.is_empty() {
            let mut batches = Vec::new();
            app.discover_rx.retain(|rx| loop {
                match rx.try_recv() {
                    Ok(batch) => batches.push(batch),
                    Err(mpsc::TryRecvError::Empty) => return true,
                    Err(mpsc::TryRecvError::Disconnected) => return false,
                }
            });
            if !batches.is_empty() && app.ingest_discovered(batches) {
                dirty = true;
            }
        }

        let terminal_size = terminal.size()?;
        let terminal_area = Rect::new(0, 0, terminal_size.width, terminal_size.height);
        if app.set_page_size(page_size_for(
            terminal_area,
            app.preview_visible,
            app.corner_3d_enabled(),
        )) {
            dirty = true;
        }
        if dirty {
            let corner_angle = app.corner_anim_angle(Instant::now());
            let now_unix = unix_now();
            let mut list_geometry = None;
            terminal.draw(|frame| {
                let theme = Theme::with_choice(app.color_enabled, app.theme_choice);
                let view = app.view(now_unix);
                list_geometry = draw(frame, &view, &theme, corner_angle);
            })?;
            if let Some(geometry) = list_geometry {
                app.last_list_area = geometry.area;
                app.last_list_start = geometry.start;
            }
            dirty = false;
        }

        let now = Instant::now();
        if app.update_preview(now) {
            dirty = true;
            continue;
        }

        let animating = app.corner_3d_enabled() && matches!(app.mode, Mode::Normal);
        let mut timeout = if animating {
            cube::FRAME.min(app.preview_wait_timeout(now))
        } else {
            app.preview_wait_timeout(now)
        };
        // While the scan streams, wake often enough to drain batches promptly so
        // the pool fills visibly instead of in one late lurch.
        if !app.discover_rx.is_empty() {
            timeout = timeout.min(Duration::from_millis(50));
        }
        if !event::poll(timeout)? {
            // Keep the ambient cube moving while idle in Normal mode.
            if animating {
                dirty = true;
            }
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
        Mode::Excludes { selected } => handle_key_excludes(app, code, modifiers, ctx, selected),
    }
}

fn handle_key_excludes(
    app: &mut App,
    code: KeyCode,
    modifiers: KeyModifiers,
    ctx: Option<&AppContext>,
    selected: usize,
) -> Option<Option<String>> {
    let len = app.excludes.roots().len();
    match code {
        KeyCode::F(4) | KeyCode::Esc => app.mode = Mode::Normal,
        KeyCode::Up => {
            app.mode = Mode::Excludes {
                selected: selected.saturating_sub(1),
            };
        }
        KeyCode::Down => {
            app.mode = Mode::Excludes {
                selected: selected.saturating_add(1).min(len.saturating_sub(1)),
            };
        }
        KeyCode::Char('d') if modifiers.contains(KeyModifiers::CONTROL) => {
            app.unexclude(selected, ctx);
        }
        _ => {}
    }
    None
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
    const ROWS: [SettingKey; 5] = [
        SettingKey::Language,
        SettingKey::Theme,
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
    let Some(candidate) = app.candidates.get(candidate_idx) else {
        app.notice = Some(app.language.text(TextKey::RecordMissing).to_string());
        return None;
    };
    let path = candidate.raw.clone();
    let from_history = candidate.source == CandidateSource::History;

    // History rows lose their history entry too, not just their visibility. The
    // exclusion list only governs what the picker shows; leaving the record on
    // disk would keep it feeding frecency and would bring the row straight back
    // for anyone running with `CDH_DISCOVER=0`.
    if from_history {
        if let Err(error) = history::remove_path(ctx, &path) {
            app.notice = Some(format!(
                "{}{error}",
                app.language.text(TextKey::DeleteFailedPrefix)
            ));
            return None;
        }
    }
    match excludes::add(&ctx.paths.excludes, &path) {
        Ok(excludes) => {
            app.excludes = excludes;
            app.exclude_subtree(&path);
        }
        Err(error) => {
            // The history entry is already gone at this point; say what failed
            // rather than implying the whole action rolled back.
            app.notice = Some(format!(
                "{}{error}",
                app.language.text(TextKey::ExcludeFailedPrefix)
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
        // Works on discovered rows too. They have no history entry to delete,
        // but with 50k of them on tap, banishing noise is the whole point of the
        // key -- refusing there left the pool with no in-TUI way to get quieter.
        KeyCode::Char('d') if ctrl => match app.selected_candidate_idx() {
            Some(candidate_idx) => app.mode = Mode::ConfirmDelete { candidate_idx },
            None => {
                app.notice = Some(app.language.text(TextKey::NoDeletableHistory).to_string());
            }
        },
        KeyCode::F(5) => {
            app.toggle_hidden_directories();
        }
        KeyCode::Char('h') | KeyCode::Char('H') if ctrl => {
            app.toggle_hidden_directories();
        }
        KeyCode::Enter => match app.selected_candidate() {
            Some(candidate) if candidate.exists => return Some(Some(candidate.raw.clone())),
            Some(_) => {
                app.notice = Some(app.language.text(TextKey::MissingDeleteHint).to_string());
            }
            None => app.notice = Some(app.language.text(TextKey::NoJumpTarget).to_string()),
        },
        KeyCode::Tab => app.toggle_preview(),
        KeyCode::Char('t') | KeyCode::Char('T') if ctrl => {
            app.cycle_theme(1);
        }
        KeyCode::F(1) | KeyCode::Char('?') | KeyCode::Char('？') => app.mode = Mode::Help,
        KeyCode::F(2) => app.mode = Mode::Settings { selected: 0 },
        KeyCode::F(4) => app.mode = Mode::Excludes { selected: 0 },
        KeyCode::F(3) => {
            app.cycle_theme(1);
        }
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
        KeyCode::Backspace if ctrl => {
            app.toggle_hidden_directories();
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
            let list_area = app.last_list_area;
            if list_area.height == 0
                || event.row < list_area.y
                || event.row >= list_area.y + list_area.height
                || event.column < list_area.x
                || event.column >= list_area.x + list_area.width
            {
                return None;
            }

            let selected = app.last_list_start + (event.row - list_area.y) as usize;
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
    bottom_divider: Rect,
    footer: Rect,
    list: Rect,
    preview: Option<(Rect, PreviewPlacement)>,
    preview_unavailable: bool,
    /// Reserved space for the ambient cube, already excluded from `list` and
    /// `preview`. `None` when the cube is disabled or the terminal is too small.
    corner: Option<Rect>,
}

/// Carve the ambient cube's gutter out of the content area before the list and
/// preview are laid out. The cube is chrome, so it gets reserved space rather
/// than being painted over content: that keeps every list row the same width
/// (paths truncate honestly, with the usual ellipsis) and lets the selection
/// highlight span each row edge to edge.
///
/// The gutter spans the full content height even though the cube only occupies
/// its bottom rows -- a uniform content width is what keeps rows from going
/// ragged, and a stable layout is worth more than the blank columns above.
fn reserve_corner_gutter(content: Rect, enabled: bool) -> (Rect, Option<Rect>) {
    if !enabled
        || content.height < cube::HEIGHT
        || content.width < CORNER_3D_GUTTER + CORNER_3D_MIN_CONTENT
    {
        return (content, None);
    }
    let width = content.width - CORNER_3D_GUTTER;
    let corner = Rect {
        x: content.x + width + 1,
        y: content.y + content.height - cube::HEIGHT,
        width: cube::WIDTH,
        height: cube::HEIGHT,
    };
    (Rect { width, ..content }, Some(corner))
}

fn screen_layout(full: Rect, preview_visible: bool, corner_enabled: bool) -> Option<ScreenLayout> {
    if full.width < 3 || full.height < MIN_HEIGHT {
        return None;
    }
    // Flat layout: no outer border inset. Keep a single column of side padding
    // so text does not hug the terminal edge.
    let inner = Rect::new(
        full.x + 1,
        full.y,
        full.width.saturating_sub(2),
        full.height,
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
    let (content, corner) = reserve_corner_gutter(sections[3], corner_enabled);
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
        bottom_divider: sections[4],
        footer: sections[5],
        list,
        preview,
        preview_unavailable,
        corner,
    })
}

fn page_size_for(full: Rect, preview_visible: bool, corner_enabled: bool) -> usize {
    screen_layout(full, preview_visible, corner_enabled)
        .map(|layout| (layout.list.height as usize).max(1))
        .unwrap_or(1)
}

/// The list geometry a frame published: where the list sat on screen and
/// which result index its first row showed. The mouse handler consumes it to
/// turn screen rows back into result indices.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ListGeometry {
    area: Rect,
    start: usize,
}

/// One frame's worth of render input, snapshotted from `App` before `draw`
/// runs. Everything on screen is a function of this struct plus the theme
/// and cube angle -- no reaching back into `App` mid-render, which is what
/// lets a test pin a frame and get the same pixels every time.
struct FrameView<'a> {
    language: Language,
    /// `settings.effective()`, taken once per frame.
    prefs: UiPreferences,
    /// The per-key environment locks, taken once per frame.
    locked: SettingLocks,
    exclude_roots: &'a [String],
    home: Option<&'a str>,
    query: &'a str,
    query_cursor: usize,
    results: &'a [Match],
    candidates: &'a [Candidate],
    page: PageWindow,
    selected_index: usize,
    preview_visible: bool,
    /// The preview state machine's verdict for the selected row, decided here
    /// so `render_preview` never re-derives it.
    preview_panel: PreviewPanelOutcome<'a>,
    notice: Option<&'a str>,
    mode: Mode,
    /// `corner_3d_enabled()`, resolved once per frame.
    corner: bool,
    /// The frame's wall clock, supplied by the caller so relative times are
    /// a pure function of the snapshot (and a test can pin them).
    now_unix: i64,
}

impl FrameView<'_> {
    fn selected_candidate(&self) -> Option<&Candidate> {
        self.results
            .get(self.selected_index)
            .map(|matched| &self.candidates[matched.idx])
    }
}

/// `corner_angle` is passed in rather than read from the clock here, so that
/// rendering stays a pure function of state and a test can pin a frame.
///
/// Returns the list geometry the frame published, or `None` when the
/// terminal was too small to lay out a list at all (the caller then keeps
/// whatever geometry the last successful frame published).
fn draw(
    frame: &mut Frame,
    view: &FrameView,
    theme: &Theme,
    corner_angle: f32,
) -> Option<ListGeometry> {
    let full = frame.area();

    let mut list_geometry = None;
    let corner = if let Some(layout) = screen_layout(full, view.preview_visible, view.corner) {
        // Flat main chrome: solid surface fill, no outer box border. Hierarchy
        // comes from dividers, spacing, and the elevated panel overlays.
        frame.render_widget(Clear, full);
        frame.render_widget(Block::default().style(theme.surface()), full);
        render_header(frame, view, theme, layout.header);
        render_input(frame, view, theme, layout.input);
        render_divider(frame, theme, layout.top_divider);
        list_geometry = Some(render_list(frame, view, theme, layout.list));
        if let Some((preview_area, placement)) = layout.preview {
            render_preview(frame, view, theme, preview_area, placement);
        }
        render_divider(frame, theme, layout.bottom_divider);
        render_footer(
            frame,
            view,
            theme,
            layout.footer,
            layout.preview_unavailable,
        );
        if let Some(corner) = layout.corner {
            cube::render(frame, corner, corner_angle, theme.cube_ink());
        }
        layout.corner
    } else {
        frame.render_widget(Clear, full);
        frame.render_widget(Block::default().style(theme.surface()), full);
        frame.render_widget(
            Paragraph::new(view.language.text(TextKey::TerminalTooSmall)).style(theme.dim()),
            full,
        );
        None
    };

    let overlay_area = screen_overlay_area(full, corner);
    match view.mode {
        Mode::Normal => {}
        Mode::Help => overlays::render_help(frame, view.language, theme, overlay_area),
        Mode::Settings { selected } => {
            overlays::render_settings(frame, view, theme, overlay_area, selected)
        }
        Mode::Excludes { selected } => {
            overlays::render_excludes(frame, view, theme, overlay_area, selected)
        }
        Mode::ConfirmDelete { candidate_idx } => {
            overlays::render_confirm_delete(frame, view, theme, overlay_area, candidate_idx)
        }
    }
    list_geometry
}

fn screen_overlay_area(full: Rect, corner: Option<Rect>) -> Rect {
    let right = corner.map_or(full.x + full.width, |corner| corner.x);
    Rect {
        width: right.saturating_sub(full.x),
        ..full
    }
}

fn render_header(frame: &mut Frame, view: &FrameView, theme: &Theme, area: Rect) {
    let title = "cdh";
    let summary = view.page.summary(view.results.len(), view.language);
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
    let cursor_index = cursor_index.min(grapheme_count(query));
    let (before, after) = split_at_grapheme_index(query, cursor_index);
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

fn render_input(frame: &mut Frame, view: &FrameView, theme: &Theme, area: Rect) {
    let prompt = "❯ ";
    let cursor = "▏";
    let width = area.width as usize;
    let available = width.saturating_sub(UnicodeWidthStr::width(prompt));
    let cursor_width = UnicodeWidthStr::width(cursor);
    let mut spans = vec![Span::styled(prompt, theme.accent())];
    if view.query.is_empty() {
        spans.push(Span::styled(cursor, theme.accent()));
        spans.push(Span::styled(
            trim_end(
                view.language.text(TextKey::SearchPlaceholder),
                available.saturating_sub(cursor_width),
            ),
            theme.dim(),
        ));
    } else {
        let cursor_index = view.query_cursor.min(grapheme_count(view.query));
        let viewport = query_viewport(
            view.query,
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

fn render_list(frame: &mut Frame, view: &FrameView, theme: &Theme, area: Rect) -> ListGeometry {
    let page = view.page;
    let row_capacity = area.height as usize;
    let list_area = Rect::new(area.x, area.y, area.width, row_capacity as u16);
    // Published up front, before any early return: the mouse handler has to
    // see where this frame's list ended up even when the page is empty or the
    // list area has no height, not where an older, larger one did.
    let geometry = ListGeometry {
        area: Rect::new(
            list_area.x,
            list_area.y,
            list_area.width,
            page.end.saturating_sub(page.start) as u16,
        ),
        start: page.start,
    };

    if view.results.is_empty() {
        if list_area.height == 0 {
            return geometry;
        }
        let empty_area = Rect::new(
            list_area.x,
            list_area.y + list_area.height.saturating_sub(1) / 2,
            list_area.width,
            1,
        );
        frame.render_widget(
            Paragraph::new(empty_state_line(view.query, view.language, theme))
                .alignment(Alignment::Center),
            empty_area,
        );
        return geometry;
    }

    // Highlights and the abbreviated display are built here, for visible rows
    // only -- one throwaway matcher for the page instead of a stored index per
    // candidate. See `Filter::run` and `Candidate::display`.
    let home = view.home;
    let mut matcher = Matcher::new(Config::DEFAULT.match_paths());
    let mut lines = Vec::with_capacity(page.end - page.start);
    for (offset, matched) in view.results[page.start..page.end].iter().enumerate() {
        let index = page.start + offset;
        let candidate = &view.candidates[matched.idx];
        let highlights = if candidate.exists {
            compute_row_highlights(&mut matcher, &candidate.raw, view.query)
        } else {
            Vec::new()
        };
        lines.push(list_row_line(
            candidate,
            home,
            &highlights,
            ListRowOptions {
                index,
                total: view.results.len(),
                selected: index == view.selected_index,
                width: area.width as usize,
                language: view.language,
            },
            theme,
        ));
    }
    frame.render_widget(Paragraph::new(lines), list_area);
    geometry
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
    home: Option<&str>,
    highlights: &[u32],
    options: ListRowOptions,
    theme: &Theme,
) -> Line<'static> {
    let display = candidate.display(home);
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
                theme.selected().fg(theme.rgb(theme.palette.dim)),
                theme
                    .selected()
                    .fg(theme.rgb(theme.palette.selected_fg))
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
        &display,
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
    view: &FrameView,
    theme: &Theme,
    area: Rect,
    placement: PreviewPlacement,
) {
    // Flat preview separator: a single rule instead of a boxed border.
    let inner = match placement {
        PreviewPlacement::Side => {
            if area.width == 0 {
                area
            } else {
                let rule = Rect::new(area.x, area.y, 1, area.height);
                let mut lines = Vec::with_capacity(area.height as usize);
                for _ in 0..area.height {
                    lines.push(Line::from(Span::styled("│", theme.border())));
                }
                frame.render_widget(Paragraph::new(lines), rule);
                Rect::new(
                    area.x.saturating_add(1),
                    area.y,
                    area.width.saturating_sub(1),
                    area.height,
                )
            }
        }
        PreviewPlacement::Bottom => {
            if area.height == 0 {
                area
            } else {
                frame.render_widget(
                    Paragraph::new(Span::styled(
                        "─".repeat(area.width as usize),
                        theme.border(),
                    )),
                    Rect::new(area.x, area.y, area.width, 1),
                );
                Rect::new(
                    area.x,
                    area.y.saturating_add(1),
                    area.width,
                    area.height.saturating_sub(1),
                )
            }
        }
    };
    if inner.is_empty() {
        return;
    }

    let Some(candidate) = view.selected_candidate() else {
        frame.render_widget(
            Paragraph::new(Span::styled(
                view.language.text(TextKey::NoSelection),
                theme.dim(),
            )),
            inner,
        );
        return;
    };
    let width = inner.width as usize;
    let mut lines = vec![
        Line::from(Span::styled(
            trim_end(candidate.name(), width),
            theme.title(),
        )),
        Line::from(Span::styled(
            trim_middle(&candidate.raw, width),
            theme.dim(),
        )),
        Line::raw(""),
    ];

    match view.preview_panel {
        PreviewPanelOutcome::Loading => {
            lines.push(Line::from(Span::styled(
                view.language.text(TextKey::Loading),
                theme.dim(),
            )));
        }
        PreviewPanelOutcome::Missing | PreviewPanelOutcome::Outcome(PreviewOutcome::Missing) => {
            lines.push(Line::from(Span::styled(
                view.language.text(TextKey::DirectoryMissing),
                theme.warning(),
            )));
        }
        PreviewPanelOutcome::Outcome(PreviewOutcome::Error(message)) => {
            let prefix = view.language.text(TextKey::CannotReadPrefix);
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
                lines.push(git_line(git, view.language, theme, width));
            }
            lines.push(Line::from(vec![
                Span::styled(view.language.text(TextKey::LastVisitPrefix), theme.dim()),
                Span::styled(
                    relative_time_at(candidate.last_visit, view.now_unix, view.language),
                    theme.primary(),
                ),
            ]));
            lines.push(Line::raw(""));
            if data.entries.is_empty() {
                lines.push(Line::from(Span::styled(
                    view.language.text(TextKey::EmptyDirectory),
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
                        view.language.text(TextKey::MoreEntries),
                        theme.dim(),
                    )));
                }
            }
        }
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

fn git_line(git: &git::GitInfo, language: Language, theme: &Theme, width: usize) -> Line<'static> {
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

#[derive(Clone, Copy)]
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

/// The wall clock for a frame, read once per frame by the event loop. Render
/// code never calls this: the time arrives inside the `FrameView` snapshot so
/// a pinned frame is reproducible.
fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn relative_time_at(timestamp: Option<i64>, now: i64, language: Language) -> String {
    language.relative_time(timestamp, now)
}

fn render_footer(
    frame: &mut Frame,
    view: &FrameView,
    theme: &Theme,
    area: Rect,
    preview_unavailable: bool,
) {
    let line = if let Some(notice) = view.notice {
        Line::from(Span::styled(
            trim_end(notice, area.width as usize),
            theme.primary(),
        ))
    } else if preview_unavailable {
        Line::from(Span::styled(
            trim_end(
                view.language.text(TextKey::PreviewSpaceInsufficient),
                area.width as usize,
            ),
            theme.warning(),
        ))
    } else {
        let hint = fit_footer(
            view.language.text(TextKey::FooterPrimary),
            view.language.text(TextKey::FooterCompact),
            view.language.text(TextKey::FooterShort),
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

fn grapheme_count(text: &str) -> usize {
    text.graphemes(true).count()
}

fn byte_index_at_grapheme(text: &str, grapheme_index: usize) -> usize {
    text.grapheme_indices(true)
        .nth(grapheme_index)
        .map(|(byte_index, _)| byte_index)
        .unwrap_or(text.len())
}

fn split_at_grapheme_index(text: &str, grapheme_index: usize) -> (&str, &str) {
    let byte_index = byte_index_at_grapheme(text, grapheme_index);
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
    if max_width == 0 {
        return String::new();
    }
    let mut width = 0;
    let mut result = String::new();
    for grapheme in text.graphemes(true) {
        let grapheme_width = UnicodeWidthStr::width(grapheme);
        if width + grapheme_width > max_width {
            break;
        }
        width += grapheme_width;
        result.push_str(grapheme);
    }
    result
}

fn take_width_back(text: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    let mut width = 0;
    let mut reverse = String::new();
    for grapheme in text.graphemes(true).rev() {
        let grapheme_width = UnicodeWidthStr::width(grapheme);
        if width + grapheme_width > max_width {
            break;
        }
        width += grapheme_width;
        reverse.push_str(grapheme);
    }
    reverse.graphemes(true).rev().collect()
}
