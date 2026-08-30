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
use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet, VecDeque};
use std::env;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::{self, IsTerminal, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
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
/// Ambient cube frame interval. Every tick rebuilds the whole frame -- ratatui
/// diffs the result so the terminal writes stay tiny, but the widget tree is
/// reconstructed regardless, and this runs precisely while the user sits
/// reading. 20fps is well past smooth for something turning at a fifth of a
/// revolution per second, and costs a third less idle work than 30.
/// Ambient corner wireframe cube: small, continuous, non-blocking chrome.
const CORNER_3D_WIDTH: u16 = 14;
const CORNER_3D_HEIGHT: u16 = 7;
const CORNER_3D_FRAME: Duration = Duration::from_millis(50);
/// Cube columns plus one blank separator column, carved out of the content area.
const CORNER_3D_GUTTER: u16 = CORNER_3D_WIDTH + 1;
/// Content width that must survive the gutter before the cube is allowed at
/// all. Ambient decoration never costs the list room it actually needs, so on
/// narrower terminals the cube simply does not appear.
const CORNER_3D_MIN_CONTENT: u16 = 64;

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
    // Spawn `git status` with a piped stdout so a slow filesystem (WSL2, network
    // mounts) can't stall the picker. On timeout we kill the child and return,
    // and the reader thread drains the pipe so neither the process nor the
    // thread lingers after `git` finally exits.
    let mut child = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(repo_root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    let mut stdout = child.stdout.take()?;
    let (tx, rx) = mpsc::channel();
    let reader = thread::spawn(move || {
        let mut buffer = Vec::new();
        let read = stdout.read_to_end(&mut buffer);
        let _ = tx.send(read.map(|_| !buffer.is_empty()));
    });

    let dirty = match rx.recv_timeout(timeout) {
        Ok(read_result) => child
            .wait()
            .ok()
            .and_then(|status| status.success().then_some(()).and(read_result.ok())),
        Err(_) => {
            // Timed out: kill the child so `read_to_end` returns and the reader
            // thread unblocks; joining keeps the pipe alive until then.
            let _ = child.kill();
            let _ = child.wait();
            None
        }
    };

    let _ = reader.join();
    dirty
}

fn preview_error_message(error: &io::Error, language: Language) -> String {
    match error.kind() {
        io::ErrorKind::PermissionDenied => language.text(TextKey::PermissionDenied).to_string(),
        io::ErrorKind::NotFound => language.text(TextKey::DirectoryMissing).to_string(),
        _ => error.to_string(),
    }
}

/// An RGB triple used to seed a palette. Kept separate from `Color` so palettes
/// stay plain data and the color-disabled path can ignore them uniformly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Rgb(u8, u8, u8);

/// A named set of colors that drives every themed style. Adding a theme is a
/// matter of defining one `Palette`; the rendering code keeps calling the same
/// `Theme` methods regardless of which palette is active.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Palette {
    /// Full-screen canvas background. Makes theme switches visible even when
    /// the terminal default bg would otherwise swallow fg-only changes.
    surface: Rgb,
    /// Elevated background for help/settings/confirm panels.
    panel: Rgb,
    border: Rgb,
    title: Rgb,
    primary: Rgb,
    dim: Rgb,
    accent: Rgb,
    match_hit: Rgb,
    warning: Rgb,
    success: Rgb,
    selected_fg: Rgb,
    selected_bg: Rgb,
}

/// Default calm blue-graphite scheme. Surface is a deep navy so the flat UI
/// still reads as a solid canvas without a boxed border.
const PALETTE_GRAPHITE: Palette = Palette {
    surface: Rgb(0x14, 0x18, 0x22),
    panel: Rgb(0x1b, 0x22, 0x30),
    border: Rgb(0x51, 0x5f, 0x7d),
    title: Rgb(0xe8, 0xee, 0xff),
    primary: Rgb(0xd8, 0xe1, 0xf5),
    dim: Rgb(0x7d, 0x89, 0xa6),
    accent: Rgb(0xa8, 0xb8, 0xff),
    match_hit: Rgb(0xc3, 0xe8, 0x8d),
    warning: Rgb(0xff, 0xcb, 0x6b),
    success: Rgb(0x98, 0xc3, 0x79),
    selected_fg: Rgb(0xf7, 0xf9, 0xff),
    selected_bg: Rgb(0x35, 0x45, 0x6a),
};

/// Nord: cooler polar-night surface with teal accents.
const PALETTE_NORD: Palette = Palette {
    surface: Rgb(0x2e, 0x34, 0x40),
    panel: Rgb(0x3b, 0x42, 0x52),
    border: Rgb(0x4c, 0x56, 0x6a),
    title: Rgb(0xec, 0xef, 0xf4),
    primary: Rgb(0xd8, 0xde, 0xe9),
    dim: Rgb(0x7b, 0x88, 0xa1),
    accent: Rgb(0x88, 0xc0, 0xd0),
    match_hit: Rgb(0xa3, 0xbe, 0x8c),
    warning: Rgb(0xeb, 0xcb, 0x8b),
    success: Rgb(0xa3, 0xbe, 0x8c),
    selected_fg: Rgb(0xec, 0xef, 0xf4),
    selected_bg: Rgb(0x43, 0x4c, 0x5e),
};

/// Daylight: inverted light canvas for bright terminals.
const PALETTE_DAYLIGHT: Palette = Palette {
    surface: Rgb(0xf4, 0xf6, 0xfa),
    panel: Rgb(0xff, 0xff, 0xff),
    border: Rgb(0xc4, 0xc9, 0xd4),
    title: Rgb(0x1c, 0x22, 0x2b),
    primary: Rgb(0x2e, 0x35, 0x40),
    dim: Rgb(0x8a, 0x91, 0x9e),
    accent: Rgb(0x2f, 0x6f, 0xd0),
    match_hit: Rgb(0x2f, 0x8a, 0x4e),
    warning: Rgb(0xb0, 0x6a, 0x00),
    success: Rgb(0x2f, 0x8a, 0x4e),
    selected_fg: Rgb(0x1c, 0x22, 0x2b),
    selected_bg: Rgb(0xd5, 0xe2, 0xf7),
};

/// Mono: near-monochrome grays with a single cool accent.
const PALETTE_MONO: Palette = Palette {
    surface: Rgb(0x12, 0x12, 0x12),
    panel: Rgb(0x1c, 0x1c, 0x1c),
    border: Rgb(0x44, 0x44, 0x44),
    title: Rgb(0xf0, 0xf0, 0xf0),
    primary: Rgb(0xd0, 0xd0, 0xd0),
    dim: Rgb(0x80, 0x80, 0x80),
    accent: Rgb(0x9a, 0xb8, 0xe0),
    match_hit: Rgb(0x9a, 0xb8, 0xe0),
    warning: Rgb(0xc8, 0xc8, 0xc8),
    success: Rgb(0x9a, 0xb8, 0xe0),
    selected_fg: Rgb(0xf7, 0xf7, 0xf7),
    selected_bg: Rgb(0x3a, 0x3a, 0x3a),
};

/// Dracula: purple/pink high-contrast dark theme, clearly distinct from blue-gray sets.
const PALETTE_DRACULA: Palette = Palette {
    surface: Rgb(0x28, 0x2a, 0x36),
    panel: Rgb(0x31, 0x34, 0x44),
    border: Rgb(0x62, 0x72, 0xa4),
    title: Rgb(0xf8, 0xf8, 0xf2),
    primary: Rgb(0xf8, 0xf8, 0xf2),
    dim: Rgb(0x62, 0x72, 0xa4),
    accent: Rgb(0xbd, 0x93, 0xf9),
    match_hit: Rgb(0x50, 0xfa, 0x7b),
    warning: Rgb(0xff, 0xb8, 0x6c),
    success: Rgb(0x50, 0xfa, 0x7b),
    selected_fg: Rgb(0xf8, 0xf8, 0xf2),
    selected_bg: Rgb(0x44, 0x47, 0x5a),
};

/// Amber: warm terminal phosphor on deep brown, high visual distance from cool themes.
const PALETTE_AMBER: Palette = Palette {
    surface: Rgb(0x1a, 0x12, 0x08),
    panel: Rgb(0x24, 0x18, 0x0c),
    border: Rgb(0x8a, 0x5a, 0x20),
    title: Rgb(0xff, 0xd2, 0x7a),
    primary: Rgb(0xf0, 0xc8, 0x78),
    dim: Rgb(0x9a, 0x74, 0x3c),
    accent: Rgb(0xff, 0xb0, 0x40),
    match_hit: Rgb(0xff, 0xe0, 0x8a),
    warning: Rgb(0xff, 0x8a, 0x4c),
    success: Rgb(0xc8, 0xe0, 0x6a),
    selected_fg: Rgb(0x1a, 0x12, 0x08),
    selected_bg: Rgb(0xff, 0xb0, 0x40),
};

/// Forest: green-moss dark theme for a nature-leaning contrast set.
const PALETTE_FOREST: Palette = Palette {
    surface: Rgb(0x10, 0x18, 0x12),
    panel: Rgb(0x16, 0x22, 0x18),
    border: Rgb(0x3d, 0x5c, 0x45),
    title: Rgb(0xe4, 0xf0, 0xe6),
    primary: Rgb(0xc8, 0xdc, 0xcc),
    dim: Rgb(0x6f, 0x8a, 0x76),
    accent: Rgb(0x7d, 0xc4, 0x92),
    match_hit: Rgb(0xb8, 0xe0, 0x86),
    warning: Rgb(0xe0, 0xc0, 0x6a),
    success: Rgb(0x7d, 0xc4, 0x92),
    selected_fg: Rgb(0xe4, 0xf0, 0xe6),
    selected_bg: Rgb(0x2a, 0x42, 0x32),
};

/// The selectable color themes, in the order the settings panel and the
/// `Ctrl+T` hotkey cycle through them. Graphite is first so it stays the
/// default when no preference is stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ThemeChoice {
    Graphite,
    Nord,
    Daylight,
    Mono,
    Dracula,
    Amber,
    Forest,
}

impl ThemeChoice {
    /// All themes in cycle order. The single source of truth for iteration,
    /// wrap-around, and count, so adding a theme only touches this array.
    const ALL: [ThemeChoice; 7] = [
        ThemeChoice::Graphite,
        ThemeChoice::Nord,
        ThemeChoice::Daylight,
        ThemeChoice::Mono,
        ThemeChoice::Dracula,
        ThemeChoice::Amber,
        ThemeChoice::Forest,
    ];

    fn palette(self) -> Palette {
        match self {
            ThemeChoice::Graphite => PALETTE_GRAPHITE,
            ThemeChoice::Nord => PALETTE_NORD,
            ThemeChoice::Daylight => PALETTE_DAYLIGHT,
            ThemeChoice::Mono => PALETTE_MONO,
            ThemeChoice::Dracula => PALETTE_DRACULA,
            ThemeChoice::Amber => PALETTE_AMBER,
            ThemeChoice::Forest => PALETTE_FOREST,
        }
    }

    /// The stable token persisted to `tui.toml` and accepted from `CDH_THEME`.
    fn tag(self) -> &'static str {
        match self {
            ThemeChoice::Graphite => "graphite",
            ThemeChoice::Nord => "nord",
            ThemeChoice::Daylight => "daylight",
            ThemeChoice::Mono => "mono",
            ThemeChoice::Dracula => "dracula",
            ThemeChoice::Amber => "amber",
            ThemeChoice::Forest => "forest",
        }
    }

    fn from_tag(value: &str) -> Option<ThemeChoice> {
        let normalized = value.trim().to_ascii_lowercase();
        ThemeChoice::ALL
            .into_iter()
            .find(|choice| choice.tag() == normalized)
    }

    /// Step `direction` positions through `ALL`, wrapping at both ends so the
    /// settings panel and hotkey cycle without a dead stop.
    fn cycle(self, direction: isize) -> ThemeChoice {
        let index = ThemeChoice::ALL
            .iter()
            .position(|choice| *choice == self)
            .unwrap_or(0) as isize;
        let count = ThemeChoice::ALL.len() as isize;
        let next = (index + direction).rem_euclid(count) as usize;
        ThemeChoice::ALL[next]
    }
}

struct Theme {
    on: bool,
    palette: Palette,
}

impl Theme {
    #[cfg(test)]
    fn new(on: bool) -> Self {
        Self::with_choice(on, ThemeChoice::Graphite)
    }

    fn with_choice(on: bool, choice: ThemeChoice) -> Self {
        Self::with_palette(on, choice.palette())
    }

    fn with_palette(on: bool, palette: Palette) -> Self {
        Self { on, palette }
    }

    /// Full-screen canvas fill. Colorless mode leaves the terminal bg alone.
    fn surface(&self) -> Style {
        if self.on {
            Style::default().bg(self.rgb(self.palette.surface))
        } else {
            Style::default()
        }
    }

    /// Elevated panel fill for help/settings/confirm overlays.
    fn panel(&self) -> Style {
        if self.on {
            Style::default().bg(self.rgb(self.palette.panel))
        } else {
            Style::default()
        }
    }

    fn rgb(&self, rgb: Rgb) -> Color {
        self.color(rgb.0, rgb.1, rgb.2)
    }

    /// Resolve a raw RGB triple, honoring the color-disabled path. Retained so
    /// the few call sites that still pass literal colors keep working.
    fn color(&self, red: u8, green: u8, blue: u8) -> Color {
        if self.on {
            Color::Rgb(red, green, blue)
        } else {
            Color::Reset
        }
    }

    fn border(&self) -> Style {
        Style::default().fg(self.rgb(self.palette.border))
    }

    fn title(&self) -> Style {
        Style::default()
            .fg(self.rgb(self.palette.title))
            .add_modifier(Modifier::BOLD)
    }

    fn primary(&self) -> Style {
        Style::default().fg(self.rgb(self.palette.primary))
    }

    fn dim(&self) -> Style {
        Style::default().fg(self.dim_color())
    }

    fn dim_color(&self) -> Color {
        self.rgb(self.palette.dim)
    }

    fn accent(&self) -> Style {
        Style::default().fg(self.rgb(self.palette.accent))
    }

    fn key_hint(&self) -> Style {
        self.accent().add_modifier(Modifier::BOLD)
    }

    fn match_color(&self) -> Color {
        self.rgb(self.palette.match_hit)
    }

    fn warning_color(&self) -> Color {
        self.rgb(self.palette.warning)
    }

    fn warning(&self) -> Style {
        Style::default().fg(self.warning_color())
    }

    fn success_color(&self) -> Color {
        self.rgb(self.palette.success)
    }

    fn selected(&self) -> Style {
        if self.on {
            Style::default()
                .fg(self.rgb(self.palette.selected_fg))
                .bg(self.rgb(self.palette.selected_bg))
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
}

impl Filter {
    fn new() -> Self {
        Self {
            matcher: Matcher::new(Config::DEFAULT.match_paths()),
            buffer: Vec::new(),
        }
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
            return (0..candidates.len()).map(|idx| Match { idx }).collect();
        }

        let pattern = Pattern::parse(query, CaseMatching::Ignore, Normalization::Smart);
        let mut valid = Vec::new();
        let mut missing = Vec::new();

        for (idx, candidate) in candidates.iter().enumerate() {
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
    last_list_area: Cell<Rect>,
    last_list_start: Cell<usize>,
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
            last_list_area: Cell::new(Rect::new(0, 0, 0, 0)),
            last_list_start: Cell::new(0),
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

    fn corner_anim_angle(&self, now: Instant) -> f32 {
        let elapsed = now.saturating_duration_since(self.corner_anim_started);
        // Wrap so a long-lived process keeps f32 precision. Unbounded, the angle
        // reaches ~10^5 rad after a day or so, where an f32 step is ~0.01 rad and
        // the rotation visibly ratchets. We cannot wrap at TAU, though: the two
        // spin rates are `angle` (rate 1) and `angle * 0.47`, so a TAU wrap of the
        // raw angle would snap the second rotation by 0.47*TAU and jump the pose.
        // Both rates realign to their start only after the angle advances a full
        // `CORNER_SPIN_PERIOD`; wrapping there bounds the magnitude while the pose
        // sequence stays continuous across the seam.
        (elapsed.as_secs_f32() * 1.15).rem_euclid(CORNER_SPIN_PERIOD)
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
                    theme_choice_label(self.language, candidate.theme)
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
            terminal.draw(|frame| {
                let theme = Theme::with_choice(app.color_enabled, app.theme_choice);
                draw(frame, &app, &theme, corner_angle);
            })?;
            dirty = false;
        }

        let now = Instant::now();
        if app.update_preview(now) {
            dirty = true;
            continue;
        }

        let animating = app.corner_3d_enabled() && matches!(app.mode, Mode::Normal);
        let mut timeout = if animating {
            CORNER_3D_FRAME.min(app.preview_wait_timeout(now))
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
        || content.height < CORNER_3D_HEIGHT
        || content.width < CORNER_3D_GUTTER + CORNER_3D_MIN_CONTENT
    {
        return (content, None);
    }
    let width = content.width - CORNER_3D_GUTTER;
    let corner = Rect {
        x: content.x + width + 1,
        y: content.y + content.height - CORNER_3D_HEIGHT,
        width: CORNER_3D_WIDTH,
        height: CORNER_3D_HEIGHT,
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

/// `corner_angle` is passed in rather than read from the clock here, so that
/// rendering stays a pure function of state and a test can pin a frame.
fn draw(frame: &mut Frame, app: &App, theme: &Theme, corner_angle: f32) {
    let full = frame.area();

    // The gutter is reserved for the whole session, not per mode: opening help
    // or settings must not reflow the list underneath the overlay.
    if let Some(layout) = screen_layout(full, app.preview_visible, app.corner_3d_enabled()) {
        // Flat main chrome: solid surface fill, no outer box border. Hierarchy
        // comes from dividers, spacing, and the elevated panel overlays.
        frame.render_widget(Clear, full);
        frame.render_widget(Block::default().style(theme.surface()), full);
        render_header(frame, app, theme, layout.header);
        render_input(frame, app, theme, layout.input);
        render_divider(frame, theme, layout.top_divider);
        render_list(frame, app, theme, layout.list);
        if let Some((preview_area, placement)) = layout.preview {
            render_preview(frame, app, theme, preview_area, placement);
        }
        render_divider(frame, theme, layout.bottom_divider);
        render_footer(frame, app, theme, layout.footer, layout.preview_unavailable);
        if let (Some(corner), Mode::Normal) = (layout.corner, app.mode) {
            render_corner_3d(frame, theme, corner, corner_angle);
        }
    } else {
        frame.render_widget(Clear, full);
        frame.render_widget(Block::default().style(theme.surface()), full);
        frame.render_widget(
            Paragraph::new(app.language.text(TextKey::TerminalTooSmall)).style(theme.dim()),
            full,
        );
    }

    match app.mode {
        Mode::Normal => {}
        Mode::Help => render_help(frame, app.language, theme, full),
        Mode::Settings { selected } => render_settings(frame, app, theme, full, selected),
        Mode::Excludes { selected } => render_excludes(frame, app, theme, full, selected),
        Mode::ConfirmDelete { candidate_idx } => {
            render_confirm_delete(frame, app, theme, full, candidate_idx)
        }
    }
}

/// The 256 Braille cell glyphs (U+2800..U+28FF), mapped from a `CubeCell`'s
/// glyph char to a shared `&'static str`. The renderer draws one per lit cell
/// every frame; without this each would `char::to_string()` into a fresh heap
/// `String` (~800 allocations/sec at the animation rate). The table is built
/// once on first use and every frame after borrows from it.
fn braille_glyph(glyph: char) -> &'static str {
    static TABLE: std::sync::OnceLock<[String; 256]> = std::sync::OnceLock::new();
    let table = TABLE.get_or_init(|| {
        std::array::from_fn(|i| {
            char::from_u32(0x2800 + i as u32)
                .expect("U+2800..=U+28FF are all valid scalars")
                .to_string()
        })
    });
    let idx = (glyph as u32).wrapping_sub(0x2800) as usize;
    table.get(idx).map(String::as_str).unwrap_or("\u{2800}")
}

/// Paint the cube into the gutter `screen_layout` reserved for it. The area is
/// already excluded from the list and preview, so filling it opaquely cannot
/// clip a path or break the selection bar.
fn render_corner_3d(frame: &mut Frame, theme: &Theme, area: Rect, angle: f32) {
    let grid = corner_cube_grid(angle, area.width as usize, area.height as usize);
    if grid.is_empty() {
        return;
    }
    // Map each cell's depth-derived light onto a shadow -> accent -> highlight
    // ramp, so near edges read hot and far ones sink toward the background.
    // On a light palette `highlight` is the darkest ink and `shadow` sits
    // nearest the surface, which inverts the ramp and still lands on the cue
    // that matters: near edges high-contrast, far edges low.
    let shadow = cube_shadow(theme.palette.accent, theme.palette.surface);
    let accent = theme.palette.accent;
    let highlight = theme.palette.title;
    let ramp = |light: f32| -> Color {
        let color = if light < 0.5 {
            lerp_rgb(shadow, accent, light / 0.5)
        } else {
            lerp_rgb(accent, highlight, (light - 0.5) / 0.5)
        };
        theme.rgb(color)
    };
    let surface_bg = theme.rgb(theme.palette.surface);
    let lines: Vec<Line> = grid
        .into_iter()
        .map(|row| {
            let spans: Vec<Span> = row
                .into_iter()
                .map(|cell| match cell {
                    Some(c) => {
                        // One color per cell is all a terminal offers, so the
                        // cell takes the coverage-weighted light of its lit dots.
                        // `braille_glyph` hands back a `&'static str` from a table
                        // built once, sparing the ~800 heap allocations/sec the
                        // old per-cell `glyph.to_string()` cost.
                        Span::styled(
                            braille_glyph(c.glyph),
                            Style::default().fg(ramp(c.light)).bg(surface_bg),
                        )
                    }
                    None => Span::raw(" "),
                })
                .collect();
            Line::from(spans)
        })
        .collect();
    frame.render_widget(Paragraph::new(lines).style(theme.surface()), area);
}

/// The dim end of the cube's shading ramp. Pulls the accent past halfway toward
/// the surface so receding edges stay hue-tinted rather than washing out to
/// gray, while sitting far enough from the lit end to read as depth.
///
/// The pull is 0.55 rather than nearer the surface on purpose. `CUBE_AMBIENT`
/// was lowered to give the lighting its dynamic range back, which drops the
/// darkest edges further down this ramp; pulling all the way to the surface
/// from there would fade the dimmest silhouette edges into the background,
/// breaking the outline. Measured against surface across the dark palettes and
/// daylight, 0.55 keeps even the darkest edge clearly separated from the
/// background while the shadow end still reads as the dim, receding one.
fn cube_shadow(accent: Rgb, surface: Rgb) -> Rgb {
    lerp_rgb(accent, surface, 0.55)
}

/// One rendered terminal cell of the cube: a Braille glyph packing a 2x4 dot
/// matrix, plus the diffuse light (0..1) averaged over its lit dots.
///
/// Braille beats the block-quadrant glyphs it replaced on both axes that
/// matter here. Resolution: 8 dots per cell instead of 4, so a line is a thin
/// stroke rather than a chunky stair-step. Aspect: a terminal cell is about
/// twice as tall as it is wide, so a 2x4 split yields near-square dots, while
/// a 2x2 split yields dots twice as tall as they are wide -- that stretch is
/// what made the old cube look sheared. The whole Braille block is also East
/// Asian Width "Neutral", so nothing here can widen to two columns and tear the
/// drawing apart in a CJK-configured terminal; the full block U+2588 that the
/// quadrant encoding reached for on a solid cell is "Ambiguous" and could.
#[derive(Debug, Clone, Copy)]
struct CubeCell {
    glyph: char,
    light: f32,
}

/// Sub-pixel dot canvas carrying, per dot, its accumulated ink coverage (0..1)
/// plus the nearest depth and the light at that depth. Bundling the buffers keeps
/// the rasterizer signatures small (clippy caps args at 7).
struct DotCanvas {
    width: usize,
    height: usize,
    coverage: Vec<f32>,
    depth: Vec<f32>,
    light: Vec<f32>,
}

impl DotCanvas {
    fn new(width: usize, height: usize) -> Self {
        Self {
            width,
            height,
            coverage: vec![0.0; width * height],
            depth: vec![f32::NEG_INFINITY; width * height],
            light: vec![0.0; width * height],
        }
    }

    /// Deposit `coverage` (0..1) of ink at a dot, carrying its `z` and `light`.
    ///
    /// Two things are resolved here, and deliberately differently:
    ///
    /// * **Colour follows depth.** Larger z is nearer the camera; where two edges
    ///   cross, the nearer one owns the dot's light, exactly as the old binary
    ///   plotter did. Only a strictly-nearer z reclaims the light, so the outcome
    ///   is independent of the order edges are drawn in.
    /// * **Coverage accumulates.** A nearer stroke that only *grazes* a dot must
    ///   not blank out the farther ink already sitting under it -- that would
    ///   trade a stair-step for a hole. So coverage saturating-adds rather than
    ///   overwriting: the dot ends up as inked as its busiest pair of strokes make
    ///   it (capped at full), while the *colour* is still the nearest stroke's.
    ///   The residual case -- a near sliver stealing the colour from a far solid
    ///   stroke beneath it -- is left as-is: at the near corner where crossings
    ///   actually happen the two strokes' brightnesses are close, so the visible
    ///   error is negligible and not worth a coverage-weighted colour blend here.
    fn plot_dot(&mut self, x: i32, y: i32, z: f32, light: f32, coverage: f32) {
        if coverage <= 0.0 {
            return;
        }
        if x < 0 || y < 0 || x as usize >= self.width || y as usize >= self.height {
            return;
        }
        let idx = y as usize * self.width + x as usize;
        if z > self.depth[idx] {
            self.depth[idx] = z;
            self.light[idx] = light;
        }
        self.coverage[idx] = (self.coverage[idx] + coverage).min(1.0);
    }

    /// Xiaolin Wu anti-aliased edge draw. Bresenham lit exactly one dot per step
    /// and hard-snapped it to the nearer grid line, which is what stair-stepped
    /// the old edges; Wu instead splits each step's ink across the two dots
    /// straddling the true line, weighted by how close the line runs to each. Fed
    /// the fractional endpoints `project_point` now returns, that turns a vertex
    /// creeping less than a dot per frame into a smooth shift of coverage between
    /// neighbours rather than a periodic one-dot jump.
    ///
    /// Depth and light are interpolated by the parameter `t` along the segment
    /// (0 at `start`, 1 at `end`) so an edge running front-to-back still fades as
    /// it recedes; `plot_dot` then composites the coverage and resolves depth.
    fn draw_edge(&mut self, start: (f32, f32, f32, f32), end: (f32, f32, f32, f32)) {
        let (mut x0, mut y0, mut z0, mut l0) = start;
        let (mut x1, mut y1, mut z1, mut l1) = end;

        // Wu marches the major axis one integer step at a time and splits ink
        // along the minor axis. Transpose steep lines so the major axis is always
        // x, then orient left-to-right so `t` runs forward with the endpoints.
        let steep = (y1 - y0).abs() > (x1 - x0).abs();
        if steep {
            std::mem::swap(&mut x0, &mut y0);
            std::mem::swap(&mut x1, &mut y1);
        }
        if x0 > x1 {
            std::mem::swap(&mut x0, &mut x1);
            std::mem::swap(&mut y0, &mut y1);
            std::mem::swap(&mut z0, &mut z1);
            std::mem::swap(&mut l0, &mut l1);
        }

        let dx = x1 - x0;
        // Coincident (or sub-dot) endpoints: one dot, no gradient to march.
        if dx.abs() < 1e-6 {
            let (px, py) = if steep { (y0, x0) } else { (x0, y0) };
            self.plot_dot(px.round() as i32, py.round() as i32, z0.max(z1), l0, 1.0);
            return;
        }
        let gradient = (y1 - y0) / dx;

        let fpart = |v: f32| v - v.floor();
        let rfpart = |v: f32| 1.0 - fpart(v);

        // Plot the two dots straddling minor coordinate `lower..lower+1` for the
        // step at major coordinate `maj`, honouring the steep transpose and
        // interpolating depth/light at `t`.
        let plot =
            |canvas: &mut Self, maj: i32, lower: i32, lower_cov: f32, upper_cov: f32, t: f32| {
                let z = z0 + (z1 - z0) * t;
                let light = l0 + (l1 - l0) * t;
                if steep {
                    canvas.plot_dot(lower, maj, z, light, lower_cov);
                    canvas.plot_dot(lower + 1, maj, z, light, upper_cov);
                } else {
                    canvas.plot_dot(maj, lower, z, light, lower_cov);
                    canvas.plot_dot(maj, lower + 1, z, light, upper_cov);
                }
            };

        // First endpoint. `xgap` fades the ink where the line starts partway into
        // a dot, so a sub-dot endpoint shift changes coverage smoothly.
        let xend1 = x0.round();
        let yend1 = y0 + gradient * (xend1 - x0);
        let xgap1 = rfpart(x0 + 0.5);
        let ypxl1 = yend1.floor();
        let t1 = ((xend1 - x0) / dx).clamp(0.0, 1.0);
        plot(
            self,
            xend1 as i32,
            ypxl1 as i32,
            rfpart(yend1) * xgap1,
            fpart(yend1) * xgap1,
            t1,
        );

        // Second endpoint.
        let xend2 = x1.round();
        let yend2 = y1 + gradient * (xend2 - x1);
        let xgap2 = fpart(x1 + 0.5);
        let ypxl2 = yend2.floor();
        let t2 = ((xend2 - x0) / dx).clamp(0.0, 1.0);
        plot(
            self,
            xend2 as i32,
            ypxl2 as i32,
            rfpart(yend2) * xgap2,
            fpart(yend2) * xgap2,
            t2,
        );

        // Interior: each step deposits a total of ~1 ink split across the pair, so
        // the busier dot always clears the membership threshold and the stroke
        // never gaps.
        let mut intery = yend1 + gradient;
        for maj in (xend1 as i32 + 1)..(xend2 as i32) {
            let lower = intery.floor();
            let t = ((maj as f32 - x0) / dx).clamp(0.0, 1.0);
            plot(self, maj, lower as i32, rfpart(intery), fpart(intery), t);
            intery += gradient;
        }
    }
}

/// Unit cube centered at the origin.
const CUBE_V: [(f32, f32, f32); 8] = [
    (-1.0, -1.0, -1.0),
    (1.0, -1.0, -1.0),
    (1.0, 1.0, -1.0),
    (-1.0, 1.0, -1.0),
    (-1.0, -1.0, 1.0),
    (1.0, -1.0, 1.0),
    (1.0, 1.0, 1.0),
    (-1.0, 1.0, 1.0),
];

const CUBE_EDGES: [(usize, usize); 12] = [
    (0, 1),
    (1, 2),
    (2, 3),
    (3, 0),
    (4, 5),
    (5, 6),
    (6, 7),
    (7, 4),
    (0, 4),
    (1, 5),
    (2, 6),
    (3, 7),
];

/// The six faces as (corner loop, outward normal). Used to hide the edges that
/// belong only to faces pointing away from the camera.
const CUBE_FACES: [([usize; 4], (f32, f32, f32)); 6] = [
    ([4, 5, 6, 7], (0.0, 0.0, 1.0)),
    ([0, 1, 2, 3], (0.0, 0.0, -1.0)),
    ([1, 2, 6, 5], (1.0, 0.0, 0.0)),
    ([0, 3, 7, 4], (-1.0, 0.0, 0.0)),
    ([3, 2, 6, 7], (0.0, 1.0, 0.0)),
    ([0, 1, 5, 4], (0.0, -1.0, 0.0)),
];

/// Camera distance along +z. Must exceed the cube's corner radius (sqrt(3)) so
/// no vertex passes through the eye.
const CUBE_CAMERA: f32 = 4.6;

/// Largest screen radius the projection can produce, and therefore what the
/// canvas has to accommodate. A corner rides the sphere of radius sqrt(3), so
/// its projected distance from center is
///
/// ```text
///   r(z) = sqrt(3 - z^2) * CUBE_CAMERA / (CUBE_CAMERA - z)
/// ```
///
/// which peaks at about z = 0.65 -- not at either extreme, because swinging a
/// corner toward the eye magnifies it but also foreshortens how far off-axis it
/// sits. Taking the naive bound (full radius at maximum magnification, a pose
/// no corner can actually reach) costs about a third of the drawing's size for
/// nothing. `corner_cube_span_is_a_tight_upper_bound` pins this value.
const CUBE_SPAN: f32 = 1.87;

/// Direction the light arrives from, in *view* space -- fixed relative to the
/// camera rather than to the cube, so the cube tumbles beneath a stationary
/// lamp and each face brightens as it turns into the light. Upper left and
/// tilted toward the viewer: the conventional key-light placement, and the one
/// that keeps at least one visible face well lit at any orientation. The tilt
/// toward the viewer (+z) is deliberately the largest component: a lamp that
/// leans overhead (+y dominant) leaves the side faces -- whose normals are
/// near-horizontal -- all pointing the same amount away from it, so they read
/// identically flat. Leaning the lamp toward the eye instead splays the side
/// faces across a range of angles, so each takes a distinct brightness. Unit
/// length, which `cube_light_direction_is_unit_length` pins.
const CUBE_LIGHT: (f32, f32, f32) = (-0.551380, 0.451129, 0.701757);

/// Floor under the diffuse term. A face turned fully from the lamp lands here
/// rather than at zero: pure Lambert would erase its edges entirely and punch
/// holes in the silhouette, so the cube has to stay whole even where it is
/// unlit. Kept low so the lit faces keep most of the dynamic range and the
/// sides show real gradation rather than sitting bunched near the floor.
const CUBE_AMBIENT: f32 = 0.10;

/// Contrast curve on the diffuse term. Half-Lambert (below) is intrinsically
/// low-contrast -- it compresses the whole sphere of normals into the upper
/// half of the range -- so a gamma > 1 pulls the midtones back down and
/// restores the sense of a shaped, directionally lit object. Tuned against a
/// 64-pose rotation sweep to hold the brightness spread across visible faces
/// even with ambient this low.
const CUBE_GAMMA: f32 = 1.8;

/// Diffuse brightness for a face, given its rotated unit normal. Returns
/// `CUBE_AMBIENT..=1.0`.
///
/// Uses half-Lambert -- `(dot + 1) / 2` with no clamp -- rather than clamped
/// Lambert. Clamped Lambert collapses every face at or past 90 degrees from the
/// lamp onto the exact same `max(0, dot) = 0`, i.e. the ambient floor; with an
/// overhead lamp that is most of the side faces at once, and two visible faces
/// pinned to an identical value read as unlit, not lit. Half-Lambert keeps
/// `dot` flowing through the full -1..1 range, so faces angled away from the
/// lamp still differ from one another and the shading never flat-lines. The
/// gamma curve pays back the contrast half-Lambert costs.
fn face_light(nx: f32, ny: f32, nz: f32) -> f32 {
    let (lx, ly, lz) = CUBE_LIGHT;
    // (dot + 1) / 2 is mathematically in 0..1, but a dot of exactly -1 (a face
    // pointing dead away from the lamp) can land a hair below zero in f32, and
    // a negative base under a fractional gamma is NaN -- clamp so the floor
    // stays the floor rather than blowing up the whole cell.
    let half_lambert = (((nx * lx + ny * ly + nz * lz) + 1.0) * 0.5).max(0.0);
    CUBE_AMBIENT + (1.0 - CUBE_AMBIENT) * half_lambert.powf(CUBE_GAMMA)
}

/// Depth's remaining contribution, now that lighting carries the shading. Kept
/// deliberately narrow: it kicks in along an edge running away from the viewer,
/// where lighting alone is constant and would flatten the recession.
fn depth_falloff(depth: f32) -> f32 {
    0.78 + 0.22 * depth
}

fn cube_edge_index(a: usize, b: usize) -> Option<usize> {
    CUBE_EDGES
        .iter()
        .position(|&(p, q)| (p == a && q == b) || (p == b && q == a))
}

/// The angle at which the cube's tumble returns to its starting pose. The two
/// spin rates below are `ay = angle` and `ax = angle * 0.47 + 0.5`; `ay` repeats
/// every TAU and `ax` every TAU/0.47, so the combined pose repeats only when the
/// angle has advanced a common multiple of both -- 100*TAU, since 0.47 = 47/100
/// clears its denominator at 100 turns. `corner_anim_angle` wraps the running
/// angle here so it never grows large enough to lose f32 resolution, and because
/// this is a whole-pose period the wrap is seamless.
const CORNER_SPIN_PERIOD: f32 = std::f32::consts::TAU * 100.0;

/// Coverage a Braille dot must reach before it counts as part of the glyph. This
/// is the one place coverage must collapse to a binary yes/no -- a terminal cell
/// either sets a dot's bit or it does not. Wu splits each step's ink across the
/// two dots straddling the line, and that split always sums to ~1, so the busier
/// of the pair is always >= 0.5: a threshold at or below 0.5 therefore lights at
/// least one dot per step and a stroke never gaps. 0.35 sits below the 0.5 an
/// even straddle lands on -- so a line running exactly between two dot rows lights
/// both and reads as the ~1.5-dot stroke it is -- yet well above the sliver a line
/// merely grazing a dot deposits, so strokes stay a crisp one-to-two dots wide
/// instead of blooming to double width. (Cell *brightness*, unlike membership, is
/// left continuous and weighted by coverage -- see the cell-assembly loop.)
const CUBE_DOT_ON: f32 = 0.35;

/// Rasterize a rotating cube into Braille sub-pixels. Each terminal cell holds
/// a 2x4 dot matrix, so a `width x height` cell area gives a `2*width x
/// 4*height` dot canvas.
///
/// Two things make this read as a solid object rather than a flat tangle:
///
/// * **Hidden-line removal.** Only edges bordering a camera-facing face are
///   drawn. A full 12-edge wireframe is a Necker cube -- genuinely ambiguous,
///   it visibly flips inside-out as it turns. Culling the back edges leaves the
///   9-edge silhouette the eye already knows as a cube, so the rotation reads
///   in one consistent direction.
/// * **Diffuse lighting.** Each vertex is lit from the averaged normal of the
///   visible faces meeting there, and an edge fades between its two endpoint
///   brightnesses, so a face turning toward the lamp lifts its whole outline as
///   a smooth gradient. Depth then modulates that slightly, so the two cues
///   reinforce rather than compete.
fn corner_cube_grid(angle: f32, width: usize, height: usize) -> Vec<Vec<Option<CubeCell>>> {
    if width == 0 || height == 0 {
        return Vec::new();
    }

    let dot_w = width * 2;
    let dot_h = height * 4;
    let mut canvas = DotCanvas::new(dot_w, dot_h);

    // Tumble on two axes at unrelated rates so the cube keeps presenting new
    // orientations rather than looping through one pose.
    let ay = angle;
    let ax = angle * 0.47 + 0.5;

    let mut proj = [(0f32, 0f32, 0f32); 8];
    let mut depth_shade = [0f32; 8];
    for (i, &(x, y, z)) in CUBE_V.iter().enumerate() {
        let (x, y, z) = rotate_y(x, y, z, ay);
        let (x, y, z) = rotate_x(x, y, z, ax);
        // z spans about -1.73..1.73; map that to 0..1.
        depth_shade[i] = (0.5 + 0.5 * (z / 1.74)).clamp(0.0, 1.0);
        proj[i] = project_point(x, y, z, dot_w, dot_h);
    }

    // Smooth (Gouraud) shading from per-vertex normals. A face is camera-facing
    // when its rotated outward normal still points toward +z (the eye), which is
    // also the culling test -- the same normal decides both whether an edge is
    // drawn and how it is shaded. For each vertex we sum the normals of the
    // *visible* faces meeting there and light the normalized result; an edge then
    // gets a distinct brightness at each end and `draw_edge` interpolates between
    // them, so a face reads as a shaped gradient rather than one flat tone.
    //
    // This replaces the old per-edge flat average of adjacent face lights. Only
    // visible faces contribute a vertex's normal, so a silhouette vertex leans
    // toward its single visible face while the near-corner vertex blends three --
    // and crucially, two faces placed symmetrically about the lamp still hand a
    // shared vertex *different* normals, so the seam between them never flat-lines
    // the way a fill light (rejected: it buys outlier removal with global
    // contrast) would leave it.
    let mut vnormal = [(0f32, 0f32, 0f32); 8];
    let mut edge_drawn = [false; 12];
    for (corners, normal) in CUBE_FACES {
        let (nx, ny, nz) = normal;
        let (nx, ny, nz) = rotate_y(nx, ny, nz, ay);
        let (nx, ny, nz) = rotate_x(nx, ny, nz, ax);
        if nz <= 0.0 {
            continue;
        }
        for i in 0..4 {
            let v = &mut vnormal[corners[i]];
            v.0 += nx;
            v.1 += ny;
            v.2 += nz;
            if let Some(edge) = cube_edge_index(corners[i], corners[(i + 1) % 4]) {
                edge_drawn[edge] = true;
            }
        }
    }

    // Light each vertex from its accumulated normal. A drawn edge always borders a
    // visible face, so both its endpoints have a non-zero accumulated normal here.
    let mut vlight = [0.0f32; 8];
    for (i, &(nx, ny, nz)) in vnormal.iter().enumerate() {
        let len = (nx * nx + ny * ny + nz * nz).sqrt();
        if len > 1e-6 {
            vlight[i] = face_light(nx / len, ny / len, nz / len);
        }
    }

    for (i, &(a, b)) in CUBE_EDGES.iter().enumerate() {
        if !edge_drawn[i] {
            continue;
        }
        let (ax0, ay0, az0) = proj[a];
        let (bx0, by0, bz0) = proj[b];
        canvas.draw_edge(
            (ax0, ay0, az0, vlight[a] * depth_falloff(depth_shade[a])),
            (bx0, by0, bz0, vlight[b] * depth_falloff(depth_shade[b])),
        );
    }

    // Braille dot -> bit layout within a 2x4 cell. The low six bits run down
    // the two columns, and the bottom row is the high two bits -- a historical
    // quirk of the 8-dot encoding, hence the table rather than a formula.
    //   (col, row) -> bit
    const BRAILLE_BITS: [[u8; 4]; 2] = [
        [0x01, 0x02, 0x04, 0x40], // left column, top to bottom
        [0x08, 0x10, 0x20, 0x80], // right column, top to bottom
    ];

    (0..height)
        .map(|cy| {
            (0..width)
                .map(|cx| {
                    let mut bits = 0u8;
                    let mut ink_sum = 0.0f32;
                    let mut members = 0u32;
                    for (col, column) in BRAILLE_BITS.iter().enumerate() {
                        for (row, bit) in column.iter().enumerate() {
                            let idx = (cy * 4 + row) * dot_w + cx * 2 + col;
                            let cov = canvas.coverage[idx];
                            // Membership is binary -- the dot's bit is set only
                            // once it holds a real share of a stroke. Brightness,
                            // though, stays continuous: each member dot's light is
                            // weighted by its coverage, so a cell a stroke merely
                            // grazes (few dots, low coverage) reads faint while one
                            // a stroke crosses through the middle (many dots near
                            // full coverage) reads at full brightness. The old mean
                            // of lit-dot light ignored coverage entirely, so a cell
                            // grazed by one dot was as bright as one packed solid.
                            if cov >= CUBE_DOT_ON {
                                bits |= bit;
                                ink_sum += cov * canvas.light[idx];
                                members += 1;
                            }
                        }
                    }
                    if bits == 0 {
                        None
                    } else {
                        Some(CubeCell {
                            glyph: char::from_u32(0x2800 + bits as u32).unwrap_or('\u{2800}'),
                            light: ink_sum / members as f32,
                        })
                    }
                })
                .collect()
        })
        .collect()
}

fn rotate_y(x: f32, y: f32, z: f32, angle: f32) -> (f32, f32, f32) {
    let (s, c) = angle.sin_cos();
    (x * c + z * s, y, -x * s + z * c)
}

fn rotate_x(x: f32, y: f32, z: f32, angle: f32) -> (f32, f32, f32) {
    let (s, c) = angle.sin_cos();
    (x, y * c - z * s, y * s + z * c)
}

/// Perspective-project a rotated point into dot-canvas coordinates.
///
/// Larger z is nearer the eye, so the divisor shrinks as z grows and near
/// corners are drawn *larger*. (The reverse -- near corners drawn smaller while
/// shaded brighter -- is what previously made the cube read as a flat smear:
/// the size and lighting cues pointed opposite ways.)
///
/// Braille dots are near-square, so the same scale applies to both axes.
///
/// The screen coordinates are returned *fractional*, not snapped to the nearest
/// dot. Rounding here is what made the cube stutter: at the running spin rate a
/// vertex advances only ~0.7 dots per frame, so a rounded position sat still for
/// a frame or so and then jumped a whole dot every ~70ms. Handing the fractional
/// position to Wu's anti-aliased rasterizer instead lets that sub-dot motion show
/// as a smooth shift of coverage between neighbouring dots.
fn project_point(x: f32, y: f32, z: f32, width: usize, height: usize) -> (f32, f32, f32) {
    let factor = CUBE_CAMERA / (CUBE_CAMERA - z);
    // Fill the canvas, keeping one dot of margin on every side.
    let scale = (width.min(height) as f32 * 0.5 - 1.0) / CUBE_SPAN;
    let px = width as f32 * 0.5 + x * scale * factor;
    let py = height as f32 * 0.5 - y * scale * factor;
    (px, py, z)
}

/// Linear blend between two RGB seeds; `t` in 0..1 moves `from` -> `to`.
fn lerp_rgb(from: Rgb, to: Rgb, t: f32) -> Rgb {
    let t = t.clamp(0.0, 1.0);
    let mix = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * t).round() as u8;
    Rgb(mix(from.0, to.0), mix(from.1, to.1), mix(from.2, to.2))
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
        let cursor_index = app.query_cursor.min(app.query_grapheme_count());
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

    // Highlights and the abbreviated display are built here, for visible rows
    // only -- one throwaway matcher for the page instead of a stored index per
    // candidate. See `Filter::run` and `Candidate::display`.
    let home = app.home.as_deref();
    let mut matcher = Matcher::new(Config::DEFAULT.match_paths());
    let mut lines = Vec::with_capacity(page.end - page.start);
    for (offset, matched) in app.filtered_results[page.start..page.end]
        .iter()
        .enumerate()
    {
        let index = page.start + offset;
        let candidate = &app.candidates[matched.idx];
        let highlights = if candidate.exists {
            compute_row_highlights(&mut matcher, &candidate.raw, &app.query)
        } else {
            Vec::new()
        };
        lines.push(list_row_line(
            candidate,
            home,
            &highlights,
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
    app: &App,
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
            trim_end(candidate.name(), width),
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
    frame.render_widget(Block::default().style(theme.panel()), area);
    // Flat panel: top rule instead of a boxed border.
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "─".repeat(area.width as usize),
            theme.border(),
        ))),
        Rect::new(area.x, area.y, area.width, 1),
    );
    let inner = Rect::new(
        area.x.saturating_add(1),
        area.y.saturating_add(1),
        area.width.saturating_sub(2),
        area.height.saturating_sub(2),
    );
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
        help_row("F4", language.text(TextKey::OpenExcludes), theme),
        help_row("Ctrl+T / F3", language.text(TextKey::SettingTheme), theme),
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
    let height = 10u16.min(full.height);
    if width < 2 || height < 2 {
        return;
    }

    let area = centered(full, width, height);
    frame.render_widget(Clear, area);
    frame.render_widget(Block::default().style(theme.panel()), area);
    // Flat panel: no box border — title + dim rule keep hierarchy without a frame.
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "─".repeat(area.width as usize),
            theme.border(),
        ))),
        Rect::new(area.x, area.y, area.width, 1),
    );
    let inner = Rect::new(
        area.x.saturating_add(1),
        area.y.saturating_add(1),
        area.width.saturating_sub(2),
        area.height.saturating_sub(2),
    );
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
        SettingKey::Theme,
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
        let style = if index == selected.min(4) {
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

/// How many entry rows the exclusion panel can show, and whether the footer
/// fits below them.
///
/// Layout is title(0), blank(1), entries(2..), footer(last). Below four rows
/// there is no room for both an entry and the footer, and an entry is the more
/// useful of the two -- forcing one row anyway would just let the footer
/// overwrite it.
fn excludes_layout(height: u16) -> (usize, bool) {
    if height >= 4 {
        (height.saturating_sub(3) as usize, true)
    } else {
        (height.saturating_sub(2) as usize, false)
    }
}

fn excludes_visible_rows(height: u16) -> usize {
    excludes_layout(height).0
}

/// First visible row of the exclusion panel.
///
/// The cursor drags the window rather than the window paging: the list has no
/// fixed bound, and anchoring the top would strand later entries below the panel
/// with no key that reaches them. Clamped so a short list never scrolls and the
/// last page is always full.
fn excludes_window_start(len: usize, rows: usize, selected: usize) -> usize {
    if len <= rows {
        return 0;
    }
    selected
        .saturating_sub(rows.saturating_sub(1))
        .min(len - rows)
}

fn render_excludes(frame: &mut Frame, app: &App, theme: &Theme, full: Rect, selected: usize) {
    let width = 72u16.min(full.width.saturating_sub(2));
    let height = 14u16.min(full.height);
    if width < 2 || height < 2 {
        return;
    }

    let area = centered(full, width, height);
    frame.render_widget(Clear, area);
    frame.render_widget(Block::default().style(theme.panel()), area);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "─".repeat(area.width as usize),
            theme.border(),
        ))),
        Rect::new(area.x, area.y, area.width, 1),
    );
    let inner = Rect::new(
        area.x.saturating_add(1),
        area.y.saturating_add(1),
        area.width.saturating_sub(2),
        area.height.saturating_sub(2),
    );
    if inner.is_empty() {
        return;
    }

    render_settings_line(
        frame,
        inner,
        0,
        app.language.text(TextKey::ExcludesTitle),
        theme.title(),
    );

    let roots = app.excludes.roots();
    if roots.is_empty() {
        render_settings_line(
            frame,
            inner,
            2,
            app.language.text(TextKey::ExcludesEmpty),
            theme.dim(),
        );
    } else {
        // Scroll the window with the cursor: the list is unbounded in principle
        // and a fixed top would strand entries below the panel with no way to
        // reach them.
        let rows = excludes_visible_rows(inner.height);
        let selected = selected.min(roots.len() - 1);
        let start = excludes_window_start(roots.len(), rows, selected);
        for (offset, root) in roots[start..].iter().take(rows).enumerate() {
            let index = start + offset;
            let style = if index == selected {
                theme.selected()
            } else {
                theme.primary()
            };
            let text = format!(
                " {}",
                PathDisplay::from_path(root, app.home.as_deref()).text
            );
            render_settings_line(
                frame,
                inner,
                2 + offset as u16,
                &trim_middle(&text, inner.width as usize),
                style,
            );
        }
    }

    if excludes_layout(inner.height).1 {
        let footer = trim_end(
            app.language.text(TextKey::ExcludesFooter),
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
        SettingKey::Theme => (
            app.language.text(TextKey::SettingTheme),
            theme_choice_label(app.language, effective.theme),
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

fn theme_choice_label(language: Language, choice: ThemeChoice) -> &'static str {
    language.text(match choice {
        ThemeChoice::Graphite => TextKey::ThemeGraphite,
        ThemeChoice::Nord => TextKey::ThemeNord,
        ThemeChoice::Daylight => TextKey::ThemeDaylight,
        ThemeChoice::Mono => TextKey::ThemeMono,
        ThemeChoice::Dracula => TextKey::ThemeDracula,
        ThemeChoice::Amber => TextKey::ThemeAmber,
        ThemeChoice::Forest => TextKey::ThemeForest,
    })
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
        .map(|candidate| candidate.display(app.home.as_deref()).text)
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
    frame.render_widget(Block::default().style(theme.panel()), area);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "─".repeat(area.width as usize),
            theme.border(),
        ))),
        Rect::new(area.x, area.y, area.width, 1),
    );
    let inner = Rect::new(
        area.x.saturating_add(1),
        area.y.saturating_add(1),
        area.width.saturating_sub(2),
        area.height.saturating_sub(2),
    );
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidate_name_borrows_the_last_segment() {
        // `name` is derived from `raw` rather than stored, so the derivation is
        // what needs pinning: trailing slashes normalize away and rootless paths
        // fall back to the whole input.
        // (Mutation check: return `path` unconditionally and the first case fails.)
        let cases = [
            ("/home/jason/workspace/cdh", "cdh"),
            ("/home/jason/workspace/cdh/", "cdh"),
            ("/", "/"),
            ("relative", "relative"),
        ];
        for (raw, want) in cases {
            assert_eq!(directory_name_str(raw), want, "directory_name_str({raw:?})");
            let candidate = Candidate {
                raw: raw.to_string(),
                score: 0.0,
                exists: true,
                last_visit: None,
                source: CandidateSource::Discovered,
            };
            assert_eq!(candidate.name(), want, "Candidate::name() for {raw:?}");
        }
    }

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

    /// Fixed cube orientation for rendering tests. Pinning the angle keeps
    /// full-screen buffer assertions reproducible instead of depending on how
    /// long the process has been alive.
    const TEST_CUBE_ANGLE: f32 = 0.7;

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
            .draw(|frame| draw(frame, app, &Theme::new(color), TEST_CUBE_ANGLE))
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
                    "Theme",
                    "Graphite",
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
                    "主题",
                    "石墨",
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
        settings_mode_select(&mut app, 2);

        let text = settings_panel_text(&settings_panel_buffer(&app, 80, 24, true));

        assert!(text.contains("Preview on startup"));
        assert!(text.contains("Environment controlled/read-only"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn main_screen_is_flat_with_surface_fill_and_no_outer_box_corners() {
        let (root, app) =
            settings_mode_app("flat-main", None, UiEnvironment::default(), Language::En);
        // Leave settings mode so draw paints the main chrome.
        let mut app = app;
        app.mode = Mode::Normal;
        let theme = Theme::with_choice(true, ThemeChoice::Amber);
        let backend = TestBackend::new(60, 16);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| draw(frame, &app, &theme, TEST_CUBE_ANGLE))
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let surface = theme.surface().bg.unwrap();
        // Corners should be surface-colored spaces, not box-drawing characters.
        for (x, y) in [(0, 0), (59, 0), (0, 15), (59, 15)] {
            let cell = &buffer[(x, y)];
            assert_eq!(
                cell.symbol(),
                " ",
                "corner ({x},{y}) should be empty, got {:?}",
                cell.symbol()
            );
            assert_eq!(cell.bg, surface, "corner ({x},{y}) should use surface bg");
        }
        // Horizontal dividers still provide hierarchy without a frame.
        let divider = buffer.content.iter().any(|cell| cell.symbol() == "─");
        assert!(divider, "expected flat dividers");
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
        settings_mode_select(&mut app, 2);
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
        settings_mode_select(&mut app, 3);
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
    fn settings_mode_up_down_selects_exactly_five_rows_and_clamps() {
        let (root, mut app) =
            settings_mode_app("selection", None, UiEnvironment::default(), Language::En);
        settings_mode_select(&mut app, 0);

        handle_key(&mut app, KeyCode::Up, KeyModifiers::NONE, None);
        assert_eq!(app.mode, Mode::Settings { selected: 0 });
        for selected in 1..=4 {
            handle_key(&mut app, KeyCode::Down, KeyModifiers::NONE, None);
            assert_eq!(app.mode, Mode::Settings { selected });
        }
        handle_key(&mut app, KeyCode::Down, KeyModifiers::NONE, None);
        assert_eq!(app.mode, Mode::Settings { selected: 4 });
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

        // Theme is row 1; booleans are rows 2..=4 (Preview/Color/Mouse).
        settings_mode_select(&mut app, 1);
        assert_eq!(app.settings.saved().theme, ThemeChoice::Graphite);
        handle_key(&mut app, KeyCode::Enter, KeyModifiers::NONE, None);
        assert_eq!(app.settings.saved().theme, ThemeChoice::Nord);
        assert_eq!(app.theme_choice, ThemeChoice::Nord);
        handle_key(&mut app, KeyCode::Right, KeyModifiers::NONE, None);
        assert_eq!(app.settings.saved().theme, ThemeChoice::Daylight);
        handle_key(&mut app, KeyCode::Left, KeyModifiers::NONE, None);
        assert_eq!(app.settings.saved().theme, ThemeChoice::Nord);

        for selected in 2..=4 {
            settings_mode_select(&mut app, selected);
            let before = app.settings.saved();
            handle_key(&mut app, KeyCode::Enter, KeyModifiers::NONE, None);
            let after = if selected == 4 {
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
        settings_mode_select(&mut app, 3);
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
    fn settings_mode_theme_cycles_and_persists_from_settings_and_shortcuts() {
        let (root, mut app) =
            settings_mode_app("theme-cycle", None, UiEnvironment::default(), Language::En);
        assert_eq!(app.theme_choice, ThemeChoice::Graphite);

        // Settings row Theme (index 1): Graphite -> Nord
        settings_mode_select(&mut app, 1);
        handle_key(&mut app, KeyCode::Enter, KeyModifiers::NONE, None);
        assert_eq!(app.theme_choice, ThemeChoice::Nord);
        assert_eq!(app.settings.saved().theme, ThemeChoice::Nord);
        assert!(fs::read_to_string(root.join("tui.toml"))
            .unwrap()
            .contains("theme = \"nord\""));
        // Settings-panel edits use the generic saved notice; shortcut cycles name the theme.
        assert_eq!(app.notice.as_deref(), Some("Settings saved"));

        // Ctrl+T cycles again: Nord -> Daylight
        app.mode = Mode::Normal;
        handle_key(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL, None);
        assert_eq!(app.theme_choice, ThemeChoice::Daylight);
        assert_eq!(app.notice.as_deref(), Some("Theme: Daylight"));
        assert!(fs::read_to_string(root.join("tui.toml"))
            .unwrap()
            .contains("theme = \"daylight\""));

        // F3 continues through the expanded palette list.
        handle_key(&mut app, KeyCode::F(3), KeyModifiers::NONE, None);
        assert_eq!(app.theme_choice, ThemeChoice::Mono);
        handle_key(&mut app, KeyCode::F(3), KeyModifiers::NONE, None);
        assert_eq!(app.theme_choice, ThemeChoice::Dracula);
        handle_key(&mut app, KeyCode::F(3), KeyModifiers::NONE, None);
        assert_eq!(app.theme_choice, ThemeChoice::Amber);
        handle_key(&mut app, KeyCode::F(3), KeyModifiers::NONE, None);
        assert_eq!(app.theme_choice, ThemeChoice::Forest);
        handle_key(&mut app, KeyCode::F(3), KeyModifiers::NONE, None);
        assert_eq!(app.theme_choice, ThemeChoice::Graphite);
        assert!(fs::read_to_string(root.join("tui.toml"))
            .unwrap()
            .contains("theme = \"graphite\""));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn settings_mode_theme_env_lock_rejects_cycle_without_disk_change() {
        let environment = UiEnvironment {
            theme: Some(ThemeChoice::Nord),
            ..UiEnvironment::default()
        };
        let (root, mut app) = settings_mode_app("theme-locked", None, environment, Language::ZhCn);
        assert_eq!(app.theme_choice, ThemeChoice::Nord);

        settings_mode_select(&mut app, 1);
        handle_key(&mut app, KeyCode::Enter, KeyModifiers::NONE, None);
        assert_eq!(app.theme_choice, ThemeChoice::Nord);
        assert!(!root.join("tui.toml").exists());
        assert_eq!(app.notice.as_deref(), Some("此设置由环境变量锁定"));

        app.mode = Mode::Normal;
        app.notice = None;
        handle_key(&mut app, KeyCode::F(3), KeyModifiers::NONE, None);
        assert_eq!(app.theme_choice, ThemeChoice::Nord);
        assert!(!root.join("tui.toml").exists());
        assert_eq!(app.notice.as_deref(), Some("此设置由环境变量锁定"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn settings_mode_persisted_theme_and_environment_override_seed_runtime() {
        let (persisted_root, persisted_app) = settings_mode_app(
            "theme-persisted",
            Some(
                "language = \"en\"\ntheme = \"daylight\"\npreview = false\ncolor = true\nmouse = true\n",
            ),
            UiEnvironment::default(),
            Language::ZhCn,
        );
        assert_eq!(persisted_app.theme_choice, ThemeChoice::Daylight);
        assert_eq!(persisted_app.settings.saved().theme, ThemeChoice::Daylight);
        let _ = fs::remove_dir_all(persisted_root);

        let environment = UiEnvironment {
            theme: Some(ThemeChoice::Mono),
            ..UiEnvironment::default()
        };
        let (root, app) = settings_mode_app(
            "theme-env-override",
            Some(
                "language = \"en\"\ntheme = \"nord\"\npreview = false\ncolor = true\nmouse = true\n",
            ),
            environment,
            Language::En,
        );
        assert_eq!(app.theme_choice, ThemeChoice::Mono);
        assert_eq!(app.settings.saved().theme, ThemeChoice::Nord);
        assert_eq!(app.settings.effective().theme, ThemeChoice::Mono);
        assert!(app.settings.is_locked(SettingKey::Theme));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn settings_mode_locked_edit_is_rejected_without_disk_change() {
        let environment = UiEnvironment {
            preview: Some(true),
            ..UiEnvironment::default()
        };
        let (root, mut app) = settings_mode_app("locked", None, environment, Language::ZhCn);
        settings_mode_select(&mut app, 2);

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
        settings_mode_select(&mut app, 3);

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
            theme: None,
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
        settings_mode_select(&mut app, 2);

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
        settings_mode_select(&mut app, 4);
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
        settings_mode_select(&mut app, 4);
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
        settings_mode_select(&mut app, 4);
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
        settings_mode_select(&mut app, 4);
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
            excludes: root.join("data").join("excludes"),
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
    fn env_truthy_treats_common_falsey_spellings_as_off() {
        // A set env var that is empty means "present but unset" -- fall back to
        // the default rather than reading it as off.
        assert!(env_truthy("", true));
        assert!(!env_truthy("", false));
        assert!(env_truthy("1", true));
        assert!(env_truthy("yes", true));
        assert!(env_truthy(" 1 ", true));
        assert!(!env_truthy("0", true));
        assert!(!env_truthy("false", true));
        assert!(!env_truthy("OFF", true));
        assert!(!env_truthy("No", true));
        assert!(!env_truthy(" off ", true));
    }

    #[test]
    fn corner_3d_needs_both_color_and_the_environment_opt_in() {
        // Previously untestable: the gate read `env::var` inline, so the only
        // reachable case was the colorless one and `CDH_CORNER_3D=0` was never
        // exercised at all. Resolving the flag onto `App` makes every
        // combination assertable without touching process state.
        let mut app = app_with_paths(&[("/tmp/cdh-corner-gate", 0.9)]);
        for (color, env, expected) in [
            (true, true, true),
            (true, false, false),
            (false, true, false),
            (false, false, false),
        ] {
            app.color_enabled = color;
            app.corner_3d_env = env;
            assert_eq!(app.corner_3d_enabled(), expected, "color={color} env={env}");
        }
    }

    #[test]
    fn corner_3d_opt_out_removes_both_the_cube_and_its_gutter() {
        // The opt-out must give the width back, not just stop drawing -- a
        // reserved-but-empty gutter would silently cost columns forever.
        let full = Rect::new(0, 0, 100, 24);
        let mut app = corner_overlap_app(30);
        app.corner_3d_env = false;
        assert!(!app.corner_3d_enabled());

        let layout = screen_layout(full, false, app.corner_3d_enabled()).unwrap();
        assert!(layout.corner.is_none(), "no gutter when opted out");
        assert_eq!(
            layout.list.width,
            screen_layout(full, false, false).unwrap().list.width,
            "opting out must return the full list width"
        );

        app.set_page_size(page_size_for(full, false, app.corner_3d_enabled()));
        let buffer = settings_panel_buffer(&app, full.width, full.height, true);
        let text = settings_panel_text(&buffer);
        assert!(text.contains("cdh"));
        assert!(
            !text.chars().any(|c| ('\u{2800}'..='\u{28FF}').contains(&c)),
            "CDH_CORNER_3D=0 must draw no cube: {text:?}"
        );
    }

    #[test]
    fn corner_cube_grid_draws_a_depth_shaded_braille_wireframe() {
        let grid = corner_cube_grid(0.7, CORNER_3D_WIDTH as usize, CORNER_3D_HEIGHT as usize);
        assert_eq!(grid.len(), CORNER_3D_HEIGHT as usize);
        assert!(grid.iter().all(|row| row.len() == CORNER_3D_WIDTH as usize));
        let cells: Vec<CubeCell> = grid.iter().flatten().flatten().copied().collect();
        // An edge outline, not a fill: some cells lit, nowhere near all of them.
        //
        // Re-derived for anti-aliasing. Bresenham lit one dot per step, so this
        // pose drew ~30 cells and the old 12..80 bracket fit. Wu now splits each
        // step's ink across the two straddling dots, and the 0.35 membership
        // threshold keeps the busier one (plus, at a true straddle, both), so a
        // stroke reads 1-2 dots wide and this pose lights 46 of the 98 (14x7)
        // cells. The bracket is derived from that, not widened to fit:
        //   * upper 60 (~60% of the grid) rejects a fill -- interior spill would
        //     climb toward 98, and the busiest pose over a full turn still only
        //     reaches 49, so 60 clears the real outline yet bars a fill;
        //   * lower 34 (~a third of the grid) rejects a collapsed outline -- if
        //     the membership threshold were cranked so high it dropped the AA body
        //     (e.g. 0.9), this pose falls to ~20 and trips the floor.
        assert!(
            (34..60).contains(&cells.len()),
            "expected an edge outline, got {} cells",
            cells.len()
        );
        // Braille only -- and never the blank pattern U+2800, which would be an
        // invisible cell masquerading as drawn content.
        assert!(
            cells
                .iter()
                .all(|c| ('\u{2801}'..='\u{28FF}').contains(&c.glyph)),
            "all glyphs should be non-blank braille, got {:?}",
            cells.iter().map(|c| c.glyph).collect::<Vec<_>>()
        );
        assert!(cells.iter().all(|c| (0.0..=1.0).contains(&c.light)));
        // Depth shading must produce real contrast between near and far edges.
        let max_light = cells.iter().map(|c| c.light).fold(0.0f32, f32::max);
        let min_light = cells.iter().map(|c| c.light).fold(1.0f32, f32::min);
        assert!(
            max_light - min_light > 0.2,
            "expected near/far depth contrast, got {min_light}..{max_light}"
        );
    }

    #[test]
    fn corner_cube_hides_edges_facing_away_from_the_camera() {
        // A cube shows at most 9 of its 12 edges from any viewpoint (7 when a
        // face is dead-on). Drawing all 12 is the ambiguous Necker wireframe
        // this renderer deliberately avoids, so sample the spin and prove the
        // back edges really are dropped.
        for step in 0..48 {
            let angle = step as f32 * 0.13;
            let drawn = cube_visible_edges(angle);
            assert!(
                (7..=9).contains(&drawn),
                "angle {angle}: {drawn} edges drawn, expected a culled 7..=9"
            );
        }
    }

    /// Count the edges `corner_cube_grid` would draw at `angle`, mirroring its
    /// culling rule so the test asserts on the rule rather than on pixels.
    fn cube_visible_edges(angle: f32) -> usize {
        let ay = angle;
        let ax = angle * 0.47 + 0.5;
        let mut visible = [false; 12];
        for (corners, normal) in CUBE_FACES {
            let (nx, ny, nz) = normal;
            let (nx, ny, nz) = rotate_y(nx, ny, nz, ay);
            let (_, _, nz) = rotate_x(nx, ny, nz, ax);
            if nz <= 0.0 {
                continue;
            }
            for i in 0..4 {
                visible[cube_edge_index(corners[i], corners[(i + 1) % 4]).unwrap()] = true;
            }
        }
        visible.iter().filter(|v| **v).count()
    }

    #[test]
    fn corner_cube_never_clips_against_its_canvas() {
        // `plot_dot` silently drops out-of-bounds dots, so asserting that lit
        // cells are in bounds proves nothing -- it is true even for a cube
        // scaled ten times too large. Assert on the projection instead: every
        // corner, at every orientation, must land inside the dot canvas. That
        // is what actually fails when the scale is too generous.
        let (dot_w, dot_h) = (CORNER_3D_WIDTH as usize * 2, CORNER_3D_HEIGHT as usize * 4);
        for step in 0..360 {
            let angle = step as f32 * 0.0349;
            let ay = angle;
            let ax = angle * 0.47 + 0.5;
            for &(x, y, z) in CUBE_V.iter() {
                let (x, y, z) = rotate_y(x, y, z, ay);
                let (x, y, z) = rotate_x(x, y, z, ax);
                let (px, py, _) = project_point(x, y, z, dot_w, dot_h);
                // Fractional now, so bound the continuous coordinate against the
                // canvas: a corner must land within [0, dim) even before the
                // rasterizer floors it to a dot.
                assert!(
                    px >= 0.0 && px < dot_w as f32 && py >= 0.0 && py < dot_h as f32,
                    "angle {angle}: corner projected to ({px},{py}), outside {dot_w}x{dot_h}"
                );
            }
        }
        // And the cube must actually be there and reasonably large -- a scale
        // that is too small would pass the bounds check above trivially.
        let grid = corner_cube_grid(0.4, CORNER_3D_WIDTH as usize, CORNER_3D_HEIGHT as usize);
        let lit = grid.iter().flatten().filter(|c| c.is_some()).count();
        assert!(
            lit >= 20,
            "cube should fill its corner, only {lit} cells lit"
        );
    }

    #[test]
    fn cube_light_direction_is_unit_length() {
        let (lx, ly, lz) = CUBE_LIGHT;
        let len = (lx * lx + ly * ly + lz * lz).sqrt();
        assert!(
            (len - 1.0).abs() < 1e-3,
            "CUBE_LIGHT must be normalized or the diffuse term is scaled, got {len}"
        );
    }

    #[test]
    fn cube_face_light_tracks_the_normal_against_the_lamp() {
        let (lx, ly, lz) = CUBE_LIGHT;
        // Facing straight into the light: fully lit.
        assert!((face_light(lx, ly, lz) - 1.0).abs() < 1e-3);
        // Facing straight away: this is the only orientation that reaches the
        // floor. Under half-Lambert the floor is a single point, not a whole
        // hemisphere -- that is the fix; a face merely angled off the lamp must
        // stay above it.
        assert!((face_light(-lx, -ly, -lz) - CUBE_AMBIENT).abs() < 1e-3);
        // Perpendicular to the lamp: half-Lambert puts dot = 0 at the midpoint
        // of the curve, so this sits strictly between floor and full rather
        // than pinned to the floor the way clamped Lambert left it.
        let (px, py, pz) = (-ly, lx, 0.0);
        let perp = face_light(px, py, pz);
        assert!(
            perp > CUBE_AMBIENT + 0.02 && perp < 1.0,
            "an edge-on face must lift off the floor under half-Lambert, got {perp}"
        );
        // Monotonic in between, so a face turning toward the lamp brightens, and
        // strictly brighter than the perpendicular face it turned from.
        let half = face_light(
            (lx + px) / 2f32.sqrt(),
            (ly + py) / 2f32.sqrt(),
            (lz + pz) / 2f32.sqrt(),
        );
        assert!(
            half > perp && half < 1.0,
            "a half-turned face should sit between the edge-on face and full, got {half}"
        );
    }

    #[test]
    fn cube_shading_depends_on_orientation_not_only_depth() {
        // The distinguishing property of lighting over the depth-only shading
        // this replaced: hold depth constant and brightness must still change.
        // A face's depth cue is symmetric about the z axis, so a pose and its
        // mirror share depths while presenting different normals to the lamp.
        let brightest = |angle: f32| -> f32 {
            corner_cube_grid(angle, CORNER_3D_WIDTH as usize, CORNER_3D_HEIGHT as usize)
                .iter()
                .flatten()
                .flatten()
                .map(|c| c.light)
                .fold(0.0f32, f32::max)
        };
        // Sweep a full turn and require the peak brightness itself to vary --
        // under depth-only shading the near corner is always at max depth, so
        // the peak would be pinned and this spread would be ~0.
        let peaks: Vec<f32> = (0..24).map(|i| brightest(i as f32 * 0.26)).collect();
        let spread = peaks.iter().cloned().fold(0.0f32, f32::max)
            - peaks.iter().cloned().fold(1.0f32, f32::min);
        assert!(
            spread > 0.1,
            "peak brightness should swing as faces turn into the lamp, got {spread} over {peaks:?}"
        );
    }

    /// The per-face diffuse brightness for every camera-facing face at `angle`,
    /// mirroring `corner_cube_grid`'s cull-then-light rule so the test asserts
    /// on the lighting model rather than on rasterized pixels.
    fn cube_visible_face_lights(angle: f32) -> Vec<f32> {
        let ay = angle;
        let ax = angle * 0.47 + 0.5;
        let mut lights = Vec::new();
        for (_corners, normal) in CUBE_FACES {
            let (nx, ny, nz) = normal;
            let (nx, ny, nz) = rotate_y(nx, ny, nz, ay);
            let (nx, ny, nz) = rotate_x(nx, ny, nz, ax);
            if nz <= 0.0 {
                continue;
            }
            lights.push(face_light(nx, ny, nz));
        }
        lights
    }

    #[test]
    fn cube_never_pins_two_visible_faces_to_the_ambient_floor() {
        // The defect this shading was rebuilt to kill: clamped Lambert sends
        // every face at or past 90 degrees from the lamp to `max(0, dot) = 0`,
        // i.e. the exact ambient floor. With more than a couple such faces
        // visible at once, two of the three the eye can see sit at an identical
        // dead value and the cube stops reading as lit at all. Half-Lambert
        // makes the floor a single unreachable-in-practice point (only the
        // normal pointing dead away from the lamp), so at most one visible face
        // can ever touch it.
        //
        // Sweep a full turn and assert no pose puts two visible faces on the
        // floor together. This fails against the old clamped model -- verified
        // by mutation: temporarily restoring `max(0.0, dot)` in `face_light`
        // pins two-plus faces to the floor in 9 of these 64 poses.
        for step in 0..64 {
            let angle = step as f32 / 64.0 * std::f32::consts::TAU;
            let lights = cube_visible_face_lights(angle);
            let at_floor = lights
                .iter()
                .filter(|l| (**l - CUBE_AMBIENT).abs() < 1e-3)
                .count();
            assert!(
                at_floor < 2,
                "angle {angle}: {at_floor} visible faces pinned to the ambient \
                 floor together -- that is the flat-shading defect, lights {lights:?}"
            );
        }
    }

    /// The per-vertex diffuse brightness the renderer now shades edges with:
    /// average the normals of the *visible* faces meeting at each vertex,
    /// normalize, light that. Mirrors `corner_cube_grid` so the test asserts on
    /// the shading model rather than on rasterized pixels. Only vertices touched
    /// by a visible face (a non-zero accumulated normal) are returned -- those are
    /// exactly the endpoints of the edges that get drawn.
    fn cube_visible_vertex_lights(angle: f32) -> Vec<f32> {
        let ay = angle;
        let ax = angle * 0.47 + 0.5;
        let mut vnormal = [(0f32, 0f32, 0f32); 8];
        for (corners, normal) in CUBE_FACES {
            let (nx, ny, nz) = normal;
            let (nx, ny, nz) = rotate_y(nx, ny, nz, ay);
            let (nx, ny, nz) = rotate_x(nx, ny, nz, ax);
            if nz <= 0.0 {
                continue;
            }
            for &c in &corners {
                vnormal[c].0 += nx;
                vnormal[c].1 += ny;
                vnormal[c].2 += nz;
            }
        }
        vnormal
            .iter()
            .filter_map(|&(nx, ny, nz)| {
                let len = (nx * nx + ny * ny + nz * nz).sqrt();
                (len > 1e-6).then(|| face_light(nx / len, ny / len, nz / len))
            })
            .collect()
    }

    #[test]
    fn cube_never_pins_two_visible_vertices_to_the_ambient_floor() {
        // The flat-shading floor defect, restated at the level shading now
        // happens. With per-vertex normals the edges are lit from vertex
        // brightnesses, not face brightnesses, so the invariant that keeps the
        // cube from reading as unlit is now vertex-level: no pose may pin two of
        // the drawn vertices to the ambient floor at once.
        //
        // This is a strictly stronger guarantee than the face-level one, and for
        // a good reason: a vertex normal is the average of two or three visible
        // face normals, so it only reaches the floor if that *average* points
        // dead away from the lamp -- rarer than any single face doing so. Over a
        // full turn no vertex touches the floor at all, so any pair-at-floor pose
        // would signal the averaging or culling had broken.
        for step in 0..64 {
            let angle = step as f32 / 64.0 * std::f32::consts::TAU;
            let lights = cube_visible_vertex_lights(angle);
            let at_floor = lights
                .iter()
                .filter(|l| (**l - CUBE_AMBIENT).abs() < 1e-3)
                .count();
            assert!(
                at_floor < 2,
                "angle {angle}: {at_floor} drawn vertices pinned to the ambient \
                 floor together, lights {lights:?}"
            );
        }
    }

    #[test]
    fn corner_cube_vertices_advance_smoothly_between_frames() {
        // Regression cover for the stutter in change (1), which had none. The old
        // `project_point` ended in `.round()`, snapping every vertex to an integer
        // dot. At the spin rate a vertex crosses only ~0.7 dots per frame, so a
        // rounded vertex sat frozen for a frame or two and then jumped a whole dot
        // -- a visible ~72ms ratchet, temporal and independent of resolution.
        // Fractional projection feeding Wu makes that motion continuous.
        //
        // Assert it on the projected vertex path, not on a whole-grid coverage
        // sum: culling legitimately pops whole edges in and out as faces cross the
        // horizon, which would swamp the sub-dot signal. A single vertex's screen
        // position has no such discontinuity, so bound its second difference --
        // discrete acceleration. Smooth sub-dot motion keeps this near zero; a
        // staircase of whole-dot jumps spikes it (a one-dot jump that reverses
        // across three frames contributes a second difference of order sqrt(2)).
        //
        // Load-bearing, proven by mutation: restoring `.round()` in
        // `project_point` lifts the measured maximum from ~0.08 to ~2.24, far past
        // this 0.5 bound, and the test fails. (See the change report.)
        let (dw, dh) = (CORNER_3D_WIDTH as usize * 2, CORNER_3D_HEIGHT as usize * 4);
        let dangle = 1.15 * 0.05; // spin rate (rad/s) * one 20fps frame
        let pos = |vx: f32, vy: f32, vz: f32, a: f32| {
            let (x, y, z) = rotate_y(vx, vy, vz, a);
            let (x, y, z) = rotate_x(x, y, z, a * 0.47 + 0.5);
            let (px, py, _) = project_point(x, y, z, dw, dh);
            (px, py)
        };
        let mut worst = 0.0f32;
        for &(vx, vy, vz) in CUBE_V.iter() {
            for step in 0..1200 {
                let a = step as f32 * dangle;
                let (x0, y0) = pos(vx, vy, vz, a);
                let (x1, y1) = pos(vx, vy, vz, a + dangle);
                let (x2, y2) = pos(vx, vy, vz, a + 2.0 * dangle);
                let ddx = x2 - 2.0 * x1 + x0;
                let ddy = y2 - 2.0 * y1 + y0;
                worst = worst.max((ddx * ddx + ddy * ddy).sqrt());
            }
        }
        assert!(
            worst < 0.5,
            "vertex screen path should advance smoothly (second difference), got {worst}"
        );
    }

    #[test]
    fn corner_cube_span_is_a_tight_upper_bound() {
        // Sweep the corner sphere and confirm CUBE_SPAN both covers the real
        // maximum (or the cube clips) and does not overshoot it (or the cube is
        // needlessly small). Guards the hand-derived constant against a later
        // change to CUBE_CAMERA silently invalidating it.
        let mut peak = 0.0f32;
        for i in 0..=2000 {
            let z = -3f32.sqrt() + (2.0 * 3f32.sqrt()) * (i as f32 / 2000.0);
            let r = (3.0 - z * z).max(0.0).sqrt() * CUBE_CAMERA / (CUBE_CAMERA - z);
            peak = peak.max(r);
        }
        assert!(
            peak <= CUBE_SPAN,
            "CUBE_SPAN {CUBE_SPAN} must cover the peak {peak} or the cube clips"
        );
        assert!(
            CUBE_SPAN - peak < 0.05,
            "CUBE_SPAN {CUBE_SPAN} wastes {} of canvas over peak {peak}",
            CUBE_SPAN - peak
        );
    }

    #[test]
    fn corner_cube_projects_near_corners_larger_than_far_ones() {
        // Perspective and shading must agree: the corner swung toward the eye
        // is both brighter and further from the center. When these disagree the
        // cube reads inside-out.
        let center = 14.0f32;
        let (near_x, _, near_z) = project_point(1.0, 0.0, 1.5, 28, 28);
        let (far_x, _, far_z) = project_point(1.0, 0.0, -1.5, 28, 28);
        assert!(near_z > far_z, "z ordering: {near_z} should exceed {far_z}");
        assert!(
            (near_x - center).abs() > (far_x - center).abs(),
            "near corner must project wider: near {near_x}, far {far_x}"
        );
    }

    #[test]
    fn corner_gutter_is_reserved_outside_the_list_and_preview() {
        let full = Rect::new(0, 0, 100, 24);
        let plain = screen_layout(full, false, false).expect("roomy terminal");
        assert!(plain.corner.is_none());

        let layout = screen_layout(full, false, true).expect("roomy terminal");
        let corner = layout.corner.expect("cube gutter");
        assert_eq!(corner.width, CORNER_3D_WIDTH);
        assert_eq!(corner.height, CORNER_3D_HEIGHT);
        // The gutter costs the list exactly its own width plus the separator,
        // and the two must not overlap at all.
        assert_eq!(layout.list.width, plain.list.width - CORNER_3D_GUTTER);
        assert!(
            layout.list.x + layout.list.width <= corner.x,
            "list {:?} must not reach into corner {corner:?}",
            layout.list
        );
        assert_eq!(corner.x + corner.width, plain.list.x + plain.list.width);
        assert_eq!(corner.y + corner.height, layout.list.y + layout.list.height);

        // Side preview is laid out inside the already-narrowed content, so it
        // stays clear of the gutter too.
        let with_preview = screen_layout(Rect::new(0, 0, 120, 24), true, true).unwrap();
        let (preview, _) = with_preview.preview.expect("side preview");
        let corner = with_preview.corner.expect("cube gutter");
        assert!(preview.x + preview.width <= corner.x);
    }

    #[test]
    fn corner_gutter_is_dropped_when_it_would_crowd_the_list() {
        // Too narrow: the list would lose room it needs, so no cube at all.
        assert!(screen_layout(Rect::new(0, 0, 70, 24), false, true)
            .expect("layout")
            .corner
            .is_none());
        // Too short for the cube to fit in the content area.
        assert!(screen_layout(Rect::new(0, 0, 100, MIN_HEIGHT), false, true)
            .expect("layout")
            .corner
            .is_none());
    }

    #[test]
    fn corner_gutter_does_not_reflow_when_an_overlay_opens() {
        let full = Rect::new(0, 0, 100, 24);
        let normal = screen_layout(full, false, true).unwrap();
        // `draw` gates only rendering on mode, never the layout, so a list row
        // keeps its width and position while help or settings is open.
        let again = screen_layout(full, false, true).unwrap();
        assert_eq!(normal.list, again.list);
        assert_eq!(
            page_size_for(full, false, true),
            normal.list.height as usize
        );
    }

    /// Read one buffer row restricted to a column range, so assertions can look
    /// at the list without picking up the cube gutter beside it.
    fn buffer_row_range(buffer: &Buffer, y: u16, x0: u16, x1: u16) -> String {
        (x0..x1).map(|x| buffer[(x, y)].symbol()).collect()
    }

    fn corner_overlap_app(count: usize) -> App {
        let paths: Vec<String> = (0..count)
            .map(|i| format!("/home/u/workspace/repos/github.com/acme/project-{i:02}/deeply/nested/source/tree/module"))
            .collect();
        let refs: Vec<(&str, f64)> = paths
            .iter()
            .enumerate()
            .map(|(i, p)| (p.as_str(), 0.9 - i as f64 * 0.001))
            .collect();
        let mut app = app_with_paths(&refs);
        app.color_enabled = true;
        app
    }

    #[test]
    fn corner_cube_never_clips_list_rows() {
        let full = Rect::new(0, 0, 100, 24);
        let layout = screen_layout(full, false, true).unwrap();
        let mut app = corner_overlap_app(30);
        app.set_page_size(page_size_for(full, false, true));
        let buffer = settings_panel_buffer(&app, full.width, full.height, true);

        // Every visible row is truncated by the same list width, so rows beside
        // the cube read exactly like the rows above them -- no silent clipping.
        let list_end = layout.list.x + layout.list.width;
        let widths: Vec<usize> = (layout.list.y..layout.list.y + layout.list.height)
            .map(|y| {
                UnicodeWidthStr::width(
                    buffer_row_range(&buffer, y, layout.list.x, list_end).trim_end(),
                )
            })
            .collect();
        assert!(
            widths.windows(2).all(|w| w[0] == w[1]),
            "list rows must truncate uniformly, got {widths:?}"
        );
        // And the truncation is visible rather than swallowed by the overlay.
        let last = buffer_row_range(
            &buffer,
            layout.list.y + layout.list.height - 1,
            layout.list.x,
            list_end,
        );
        assert!(
            last.contains('…'),
            "clipped path should show an ellipsis: {last:?}"
        );
    }

    #[test]
    fn corner_cube_never_breaks_the_selection_bar() {
        let full = Rect::new(0, 0, 100, 24);
        let layout = screen_layout(full, false, true).unwrap();
        let rows = layout.list.height;
        let mut app = corner_overlap_app(30);
        app.set_page_size(page_size_for(full, false, true));

        let bar_run = |app: &App| -> (u16, u16) {
            let buffer = settings_panel_buffer(app, full.width, full.height, true);
            let y = layout.list.y + app.selected_index as u16;
            let bg = buffer[(layout.list.x, y)].bg;
            let end = (layout.list.x..layout.list.x + layout.list.width)
                .take_while(|x| buffer[(*x, y)].bg == bg)
                .count() as u16;
            (layout.list.x, end)
        };

        // A row well above the cube and a row level with it must highlight the
        // same way: the bar spans the full list width in both cases.
        app.set_selected(0);
        let top = bar_run(&app);
        app.set_selected(rows as usize - 1);
        let beside_cube = bar_run(&app);
        assert_eq!(
            top, beside_cube,
            "selection bar must not be cut short beside the cube"
        );
        assert_eq!(
            beside_cube.1, layout.list.width,
            "selection bar should span the whole list row"
        );
    }

    #[test]
    fn corner_3d_render_is_a_no_op_when_colorless() {
        let mut app = app_with_paths(&[("/tmp/cdh-corner-alpha", 0.9)]);
        app.color_enabled = false;
        let buffer = settings_panel_buffer(&app, 60, 16, false);
        let text = settings_panel_text(&buffer);
        // Main UI should still render; no braille cube anywhere on screen.
        assert!(text.contains("cdh"));
        assert!(
            !text.chars().any(|c| ('\u{2800}'..='\u{28FF}').contains(&c)),
            "colorless mode must skip corner cube: {text:?}"
        );
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

        // 80, not 50: the subtree clause makes the English sentence 61 columns,
        // so a 50-column budget now exercises truncation rather than the copy.
        let message = confirm_delete_message("~/archive/old-project", 80, Language::En);
        assert_eq!(
            message,
            "Stop showing “~/archive/old-project” and everything under it?"
        );
    }

    #[test]
    fn english_help_contains_no_chinese_copy() {
        let theme = Theme::new(false);
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
            Some("Directory is missing; press Ctrl+D to exclude it")
        );

        app.query = "not-found".to_string();
        app.query_cursor = app.query.chars().count();
        app.recompute_after_query_change();
        assert_eq!(
            line_text(&empty_state_line(
                &app.query,
                app.language,
                &Theme::new(false)
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
        let regular = page_size_for(regular_area, false, false);
        let short = page_size_for(short_area, false, false);
        assert_eq!(
            regular,
            screen_layout(regular_area, false, false)
                .unwrap()
                .list
                .height as usize
        );
        assert_eq!(
            short,
            screen_layout(short_area, false, false).unwrap().list.height as usize
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
        let theme = Theme::new(true);
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
            score: 1.0,
            exists: true,
            last_visit: None,
            source: CandidateSource::History,
        };
        let mut filter = Filter::new();
        let matches = filter.run(std::slice::from_ref(&candidate), "/home/jason");
        assert_eq!(matches.len(), 1);
        // Highlights are computed at render time (deferred to visible rows);
        // rebuild them here and confirm they still map onto the "~" display char.
        let display = candidate.display(Some("/home/jason"));
        let mut matcher = Matcher::new(Config::DEFAULT.match_paths());
        let highlights = compute_row_highlights(&mut matcher, &candidate.raw, "/home/jason");
        assert!(display.display_highlight_indices(&highlights).contains(&0));
    }

    #[test]
    fn ascii_highlighting_handles_windows_paths_without_aborting() {
        let paths = [
            "/mnt/d/Jason/Documents/Workspace/vs2022/repo/Vela",
            "/mnt/d/Jason/Documents/Workspace/vs2022/repo/app01",
            "/mnt/d/Jason/Documents/Workspace/vs2022/repo/winUI-demo",
            "/mnt/c/Users/Jason/Documents/Virtual Machines",
            "/mnt/c/Users/Jason/Documents/Visual Studio 2022",
            "/mnt/c/Users/Jason/Documents/Voicemeeter",
            "/tmp/VelaShellPreview",
            "/tmp/vela-audit",
            "/tmp/vela-final-runtime-sizes",
            "/tmp/vela-final-sizes",
            "/tmp/vela-final-sizes-v2",
            "/mnt/d/Jason/Documents/Workspace/vue-vben-admin",
            "/home/jason/workspace/labs/python/vllm-qwen",
        ];
        let mut matcher = Matcher::new(Config::DEFAULT.match_paths());

        for path in paths {
            let _ = compute_row_highlights(&mut matcher, path, "V");
        }
    }

    #[test]
    fn highlighting_uses_filter_pattern_for_case_and_fuzzy_syntax() {
        let raw = "/tmp/VelaShellPreview/Marvel's Spider-Man 2";
        let mut matcher = Matcher::new(Config::DEFAULT.match_paths());
        let highlights = compute_row_highlights(&mut matcher, raw, "V'");

        assert!(highlights
            .iter()
            .any(|&index| { raw.chars().nth(index as usize) == Some('v') }));
        assert!(highlights
            .iter()
            .any(|&index| { raw.chars().nth(index as usize) == Some('\'') }));
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
    fn unicode_query_cursor_moves_and_deletes_by_graphemes() {
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
    fn query_cursor_moves_by_grapheme_clusters() {
        let mut app = app_with_paths(&[("/👩‍💻", 0.9)]);
        app.query = "a👩‍💻b".to_string();
        app.query_cursor = 1;

        for _ in 0..3 {
            handle_key(&mut app, KeyCode::Right, KeyModifiers::NONE, None);
        }
        assert_eq!(app.query_cursor, 3);

        handle_key(&mut app, KeyCode::Left, KeyModifiers::NONE, None);
        assert_eq!(app.query_cursor, 2);
    }

    #[test]
    fn query_cursor_stays_within_grapheme_boundaries() {
        let mut app = app_with_paths(&[("/中文🧭", 0.9)]);
        app.query = "中🧭".to_string();
        app.query_cursor = 0;

        handle_key(&mut app, KeyCode::Left, KeyModifiers::NONE, None);
        assert_eq!(app.query_cursor, 0);
        handle_key(&mut app, KeyCode::Right, KeyModifiers::NONE, None);
        handle_key(&mut app, KeyCode::Right, KeyModifiers::NONE, None);
        handle_key(&mut app, KeyCode::Right, KeyModifiers::NONE, None);
        assert_eq!(app.query_cursor, 2);
        assert_eq!(
            split_at_grapheme_index(&app.query, app.query_cursor).0,
            "中🧭"
        );
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
    fn query_viewport_keeps_combined_graphemes_within_width() {
        let query = "🧭‍🧭";
        let viewport = query_viewport(query, 1, 2);

        assert_eq!(viewport.before, query);
        assert!(viewport.after.is_empty());
        assert!(viewport.display_width() <= 2);
    }

    #[test]
    fn query_viewport_stays_within_width_for_grapheme_boundaries() {
        let queries = ["🧭‍🧭", "👩‍💻", "👨‍👩‍👧‍👦", "e\u{301}", "🇨🇳", "a👩‍💻b"];

        for query in queries {
            for cursor in 0..=grapheme_count(query) {
                for width in 0..=8 {
                    let viewport = query_viewport(query, cursor, width);
                    assert!(
                        viewport.display_width() <= width,
                        "query={query:?}, cursor={cursor}, width={width}, viewport={viewport:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn render_input_handles_a_combined_grapheme_at_the_cursor() {
        let mut app = app_with_paths(&[("/👩‍💻", 0.9)]);
        app.query = "🧭‍🧭".to_string();
        app.query_cursor = 1;

        let backend = TestBackend::new(5, 1);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render_input(frame, &app, &Theme::new(true), frame.area()))
            .unwrap();
    }

    #[test]
    fn width_truncation_preserves_combined_graphemes() {
        let query = "👩‍💻";

        assert_eq!(take_width_front(query, 2), query);
        assert_eq!(take_width_back(query, 2), query);
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
        let theme = Theme::new(true);
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
        let theme = Theme::new(false);
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
        let selected = app.selected_raw().unwrap();
        app.exclude_subtree(&selected);
        assert_eq!(app.filtered_results.len(), 2);
        assert_eq!(app.selected_index, 1);
        assert_eq!(app.page().page, 1);

        let selected = app.selected_raw().unwrap();
        app.exclude_subtree(&selected);
        let selected = app.selected_raw().unwrap();
        app.exclude_subtree(&selected);
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
        assert_eq!(app.notice.as_deref(), Some("目录已失效，按 Ctrl+D 排除它"));
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
        // Deleting a history row must also write the exclusion list, or the
        // directory walks straight back in as a discovered row on next launch --
        // and discovered rows carry no history entry to delete a second time.
        assert!(
            crate::excludes::Excludes::load(&ctx.paths.excludes).contains(stale.to_str().unwrap())
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn ctrl_d_on_discovered_row_excludes_without_touching_history() {
        let (root, ctx) = test_ctx("exclude_discovered");
        let keep = root.join("keep");
        fs::create_dir_all(&keep).unwrap();
        let history_line = format!("{}\n", keep.display());
        fs::write(&ctx.paths.history_raw, format!("100\t{}\n", keep.display())).unwrap();
        fs::write(&ctx.paths.history_uniq, &history_line).unwrap();
        let mut app = App::with_preview_worker(
            build_candidates(&recs(&[(keep.to_str().unwrap(), 0.8)])),
            None,
            false,
        );
        let noise = root.join("noise");
        app.ingest_discovered(vec![vec![
            noise.to_string_lossy().into_owned(),
            noise.join("deep/deeper").to_string_lossy().into_owned(),
        ]]);
        assert_eq!(app.candidates.len(), 3);
        app.set_selected(1);
        assert_eq!(
            app.selected_candidate().unwrap().source,
            CandidateSource::Discovered
        );

        handle_key(
            &mut app,
            KeyCode::Char('d'),
            KeyModifiers::CONTROL,
            Some(&ctx),
        );
        handle_key(
            &mut app,
            KeyCode::Char('d'),
            KeyModifiers::CONTROL,
            Some(&ctx),
        );

        // The subtree goes, not just the selected row.
        assert_eq!(app.candidates.len(), 1);
        assert_eq!(app.candidates[0].raw, keep.to_str().unwrap());
        assert!(app
            .excludes
            .contains(&noise.join("deep/deeper").to_string_lossy()));
        assert!(
            crate::excludes::Excludes::load(&ctx.paths.excludes).contains(&noise.to_string_lossy())
        );
        // A discovered row has no history entry; the history files must be
        // untouched rather than rewritten for a path that was never in them.
        assert_eq!(
            fs::read_to_string(&ctx.paths.history_uniq).unwrap(),
            history_line
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn excluded_and_ignored_paths_never_enter_the_pool() {
        let mut app =
            App::with_preview_worker(build_candidates(&recs(&[("/h", 0.9)])), None, false);
        app.excludes = crate::excludes::Excludes::from_paths(["/noise"]);
        app.ignore_re = Some(Regex::new("/target/").unwrap());
        app.ingest_discovered(vec![vec![
            "/noise".to_string(),
            "/noise/deep".to_string(),
            "/keep/src".to_string(),
            "/keep/target/debug".to_string(),
        ]]);
        let discovered: Vec<_> = app
            .candidates
            .iter()
            .filter(|candidate| candidate.source == CandidateSource::Discovered)
            .map(|candidate| candidate.raw.as_str())
            .collect();
        assert_eq!(discovered, vec!["/keep/src"]);
        // Filtered paths must stay unknown: recording them as seen would make a
        // later, legitimate arrival of the same path dedup away into nothing.
        assert!(!app.known_paths.contains(&path_fingerprint("/noise/deep")));
        assert!(!app
            .known_paths
            .contains(&path_fingerprint("/keep/target/debug")));
    }

    #[test]
    fn excluding_then_unexcluding_in_one_session_brings_the_subtree_back() {
        // The case the F4 panel exists for: excluded the wrong row, undo it now.
        // `unexclude` re-scans the subtree, so every path is re-emitted -- if
        // `exclude_subtree` left the fingerprints in `known_paths`, all of them
        // dedup away and the undo silently restores nothing.
        // (Mutation check: drop the `known_paths.remove` loop in
        // `exclude_subtree` and this lands back at 1 candidate.)
        let mut app =
            App::with_preview_worker(build_candidates(&recs(&[("/h", 0.9)])), None, false);
        app.ingest_discovered(vec![vec!["/noise".to_string(), "/noise/deep".to_string()]]);
        assert_eq!(app.candidates.len(), 3);

        app.exclude_subtree("/noise");
        assert_eq!(app.candidates.len(), 1);
        // While still excluded the pool must stay clean even if a batch arrives.
        app.excludes = crate::excludes::Excludes::from_paths(["/noise"]);
        app.ingest_discovered(vec![vec!["/noise/deep".to_string()]]);
        assert_eq!(
            app.candidates.len(),
            1,
            "exclusion still filters in-flight batches"
        );

        // Undo: the list no longer covers it, and the top-up scan re-emits.
        app.excludes = crate::excludes::Excludes::default();
        app.ingest_discovered(vec![vec!["/noise".to_string(), "/noise/deep".to_string()]]);
        let raws: Vec<_> = app.candidates.iter().map(|c| c.raw.as_str()).collect();
        assert_eq!(app.candidates.len(), 3, "subtree must come back: {raws:?}");
        assert!(raws.contains(&"/noise"));
        assert!(raws.contains(&"/noise/deep"));
    }

    #[test]
    fn exclusion_panel_drops_the_footer_before_it_covers_an_entry() {
        // Layout is title / blank / entries / footer. Below four rows only one
        // of the last two fits, and an entry beats the key hint.
        assert_eq!(excludes_layout(12), (9, true));
        assert_eq!(excludes_layout(4), (1, true));
        assert_eq!(excludes_layout(3), (1, false));
        assert_eq!(excludes_layout(2), (0, false));
        assert_eq!(excludes_layout(0), (0, false));
        // Rows must never reach the footer row.
        for height in 4..40u16 {
            let (rows, _) = excludes_layout(height);
            assert!(
                2 + rows <= (height - 1) as usize,
                "overlap at height {height}"
            );
        }
    }

    #[test]
    fn f4_opens_the_exclusion_panel_and_esc_closes_it() {
        // The panel is the only way back: an excluded directory is by definition
        // absent from the pool, so there is no row to press a key on.
        let mut app =
            App::with_preview_worker(build_candidates(&recs(&[("/a", 0.9)])), None, false);
        handle_key(&mut app, KeyCode::F(4), KeyModifiers::NONE, None);
        assert!(matches!(app.mode, Mode::Excludes { selected: 0 }));
        handle_key(&mut app, KeyCode::Esc, KeyModifiers::NONE, None);
        assert_eq!(app.mode, Mode::Normal);
        // F4 also toggles closed.
        handle_key(&mut app, KeyCode::F(4), KeyModifiers::NONE, None);
        handle_key(&mut app, KeyCode::F(4), KeyModifiers::NONE, None);
        assert_eq!(app.mode, Mode::Normal);
    }

    #[test]
    fn exclusion_panel_navigation_clamps_at_both_ends() {
        let mut app =
            App::with_preview_worker(build_candidates(&recs(&[("/a", 0.9)])), None, false);
        app.excludes = crate::excludes::Excludes::from_paths(["/x", "/y", "/z"]);
        handle_key(&mut app, KeyCode::F(4), KeyModifiers::NONE, None);
        for _ in 0..5 {
            handle_key(&mut app, KeyCode::Down, KeyModifiers::NONE, None);
        }
        assert!(matches!(app.mode, Mode::Excludes { selected: 2 }));
        for _ in 0..5 {
            handle_key(&mut app, KeyCode::Up, KeyModifiers::NONE, None);
        }
        assert!(matches!(app.mode, Mode::Excludes { selected: 0 }));
    }

    #[test]
    fn exclusion_window_follows_the_cursor_without_overscrolling() {
        // Short list never scrolls.
        assert_eq!(excludes_window_start(3, 8, 2), 0);
        // Cursor drags the window down one row at a time...
        assert_eq!(excludes_window_start(20, 5, 4), 0);
        assert_eq!(excludes_window_start(20, 5, 5), 1);
        // ...and the last page stays full instead of scrolling past the end.
        assert_eq!(excludes_window_start(20, 5, 19), 15);
        // Degenerate heights must not underflow.
        assert_eq!(excludes_window_start(0, 1, 0), 0);
        assert_eq!(excludes_window_start(4, 1, 3), 3);
    }

    #[test]
    fn unexclude_rewrites_the_file_and_shrinks_the_panel() {
        let (root, ctx) = test_ctx("unexclude");
        crate::excludes::add(&ctx.paths.excludes, "/noise/one").unwrap();
        crate::excludes::add(&ctx.paths.excludes, "/noise/two").unwrap();
        let mut app =
            App::with_preview_worker(build_candidates(&recs(&[("/a", 0.9)])), None, false);
        app.excludes = crate::excludes::Excludes::load(&ctx.paths.excludes);
        assert_eq!(app.excludes.roots().len(), 2);

        handle_key(&mut app, KeyCode::F(4), KeyModifiers::NONE, Some(&ctx));
        handle_key(
            &mut app,
            KeyCode::Char('d'),
            KeyModifiers::CONTROL,
            Some(&ctx),
        );

        // In memory and on disk, and the panel stays open on a valid row.
        assert_eq!(app.excludes.roots(), ["/noise/two".to_string()]);
        let reloaded = crate::excludes::Excludes::load(&ctx.paths.excludes);
        assert!(!reloaded.contains("/noise/one"));
        assert!(reloaded.contains("/noise/two"));
        assert!(matches!(app.mode, Mode::Excludes { selected: 0 }));

        // Removing the last entry must not leave the cursor past the end.
        handle_key(
            &mut app,
            KeyCode::Char('d'),
            KeyModifiers::CONTROL,
            Some(&ctx),
        );
        assert!(app.excludes.is_empty());
        assert!(matches!(app.mode, Mode::Excludes { selected: 0 }));
        // Ctrl+D on an empty list is a no-op, not a panic.
        handle_key(
            &mut app,
            KeyCode::Char('d'),
            KeyModifiers::CONTROL,
            Some(&ctx),
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn exclusion_panel_renders_safely_when_empty_or_tiny() {
        let mut app =
            App::with_preview_worker(build_candidates(&recs(&[("/a", 0.9)])), None, false);
        app.mode = Mode::Excludes { selected: 0 };
        // Empty list: the panel must say so rather than render a blank box.
        let empty = settings_panel_text(&settings_panel_buffer(&app, 60, 20, true));
        assert!(empty.contains("排除清单"));
        assert!(empty.contains("清单为空"));

        app.excludes = crate::excludes::Excludes::from_paths(["/x/one", "/y/two", "/z/three"]);
        app.mode = Mode::Excludes { selected: 2 };
        let listed = settings_panel_text(&settings_panel_buffer(&app, 60, 20, true));
        assert!(listed.contains("/x/one"));
        assert!(listed.contains("/z/three"));

        // Terminal sizes that leave no room for the panel body must not panic.
        for (width, height) in [(24, 12), (10, 4), (3, 3), (1, 1)] {
            let buffer = settings_panel_buffer(&app, width, height, true);
            assert_eq!(buffer.area.width, width);
            assert_eq!(buffer.area.height, height);
        }
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
            score: 1.0,
            exists: true,
            last_visit: None,
            source: CandidateSource::History,
        };
        let theme = Theme::new(true);
        let line = list_row_line(
            &candidate,
            Some("/home/jason"),
            &[],
            row_options(1, 10, false, 80),
            &theme,
        );
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
            score: 1.0,
            exists: false,
            last_visit: None,
            source: CandidateSource::History,
        };
        let theme = Theme::new(true);
        let line = list_row_line(&candidate, None, &[], row_options(5, 10, false, 80), &theme);
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
            score: 1.0,
            exists: true,
            last_visit: None,
            source: CandidateSource::History,
        };
        let theme = Theme::new(true);
        let home = Some("/home/jason");
        let wide = list_row_line(&candidate, home, &[], row_options(0, 1, false, 80), &theme);
        let narrow = list_row_line(&candidate, home, &[], row_options(0, 1, false, 24), &theme);
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
        assert!(message.starts_with("不再显示 “"));
        // The subtree half of the sentence must survive truncation: it is the
        // difference between hiding one row and hiding a few thousand.
        assert!(message.ends_with("” 及其子目录？"));
    }

    #[test]
    fn search_match_in_selected_row_keeps_the_same_background() {
        let candidate = Candidate {
            raw: "/projects/api-client".to_string(),
            score: 1.0,
            exists: true,
            last_visit: None,
            source: CandidateSource::History,
        };
        let theme = Theme::new(true);
        let line = list_row_line(
            &candidate,
            None,
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
            score: 1.0,
            exists: true,
            last_visit: None,
            source: CandidateSource::History,
        };
        let theme = Theme::new(false);
        let line = list_row_line(
            &candidate,
            None,
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
            score: 1.0,
            exists: true,
            last_visit: None,
            source: CandidateSource::History,
        };
        let theme = Theme::new(true);
        let highlights = (raw[..raw.find("api-client").unwrap()].chars().count() as u32
            ..raw.chars().count() as u32)
            .collect::<Vec<_>>();
        let line = list_row_line(
            &candidate,
            Some("/home/jason"),
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
        assert!(screen_layout(Rect::new(0, 0, 110, 24), true, false)
            .unwrap()
            .preview
            .is_some());
        assert!(screen_layout(Rect::new(0, 0, 80, 24), true, false)
            .unwrap()
            .preview
            .is_some());
        assert!(
            screen_layout(Rect::new(0, 0, 60, 24), true, false)
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

    // ---- Directory-tree discovery layer ----

    fn discovered_cand(raw: &str, score: f32) -> Candidate {
        Candidate {
            raw: raw.to_string(),
            score,
            exists: true,
            last_visit: None,
            source: CandidateSource::Discovered,
        }
    }

    fn history_cand(raw: &str, score: f32) -> Candidate {
        Candidate {
            source: CandidateSource::History,
            ..discovered_cand(raw, score)
        }
    }

    #[test]
    fn filter_ranks_history_before_discovered_at_equal_fuzzy_score() {
        // Same raw path => identical fuzzy score and identical recommendation
        // score, so only the source tiebreak can order them. Discovered is listed
        // first to prove the sort actively reorders. (Mutation check: drop the
        // source clause and Rust's stable sort leaves Discovered first -> fails.)
        let candidates = vec![discovered_cand("/a/b", 0.5), history_cand("/a/b", 0.5)];
        let mut filter = Filter::new();
        let matches = filter.run(&candidates, "b");
        assert_eq!(matches.len(), 2);
        assert_eq!(candidates[matches[0].idx].source, CandidateSource::History);
        assert_eq!(
            candidates[matches[1].idx].source,
            CandidateSource::Discovered
        );
    }

    #[test]
    fn ingest_dedups_against_history_and_within_itself() {
        // History owns /a/b. The batch re-offers /a/b (must lose) and /a/c twice
        // (once with a trailing slash, which normalizes equal). (Mutation check:
        // remove the known_paths dedup and this becomes 4 candidates -> fails.)
        let mut app =
            App::with_preview_worker(build_candidates(&recs(&[("/a/b", 0.9)])), None, false);
        app.ingest_discovered(vec![vec![
            "/a/b".to_string(),
            "/a/c".to_string(),
            "/a/c/".to_string(),
        ]]);
        assert_eq!(app.candidates.len(), 2);
        assert_eq!(app.candidates[0].source, CandidateSource::History);
        let discovered: Vec<_> = app
            .candidates
            .iter()
            .filter(|candidate| candidate.source == CandidateSource::Discovered)
            .map(|candidate| candidate.raw.clone())
            .collect();
        assert_eq!(discovered, vec!["/a/c".to_string()]);
    }

    #[test]
    fn empty_query_keeps_history_first_then_sorted_discovery() {
        let mut app = App::with_preview_worker(
            build_candidates(&recs(&[("/hist1", 0.9), ("/hist2", 0.8)])),
            None,
            false,
        );
        app.ingest_discovered(vec![vec!["/disc/z".to_string(), "/disc/a".to_string()]]);
        let order: Vec<_> = app
            .filtered_results
            .iter()
            .map(|matched| app.candidates[matched.idx].raw.clone())
            .collect();
        // History prefix unchanged (frecency order); discovered suffix sorted by
        // (score desc, path asc) -- both score 0 here, so path ascending.
        assert_eq!(
            order,
            vec![
                "/hist1".to_string(),
                "/hist2".to_string(),
                "/disc/a".to_string(),
                "/disc/z".to_string(),
            ]
        );
    }

    #[test]
    fn discovery_inherits_parent_neighborhood_score_for_ordering() {
        // /hot has a high score, so its parent /p maps hot; a discovered sibling
        // /p/cold under the same parent inherits that heat and sorts ahead of a
        // discovered dir in a cold corner.
        let mut app =
            App::with_preview_worker(build_candidates(&recs(&[("/p/hot", 0.9)])), None, false);
        app.discover_score_map = build_score_map(&recs(&[("/p/hot", 0.9)]));
        app.ingest_discovered(vec![vec!["/z/cold".to_string(), "/p/sibling".to_string()]]);
        let discovered: Vec<_> = app.candidates[app.discovered_start..]
            .iter()
            .map(|candidate| candidate.raw.clone())
            .collect();
        assert_eq!(
            discovered,
            vec!["/p/sibling".to_string(), "/z/cold".to_string()]
        );
    }

    #[test]
    fn ingest_preserves_selected_path() {
        let mut app = App::with_preview_worker(
            build_candidates(&recs(&[("/hist1", 0.9), ("/hist2", 0.8)])),
            None,
            false,
        );
        app.set_selected(1);
        let before = app.selected_raw();
        assert_eq!(before.as_deref(), Some("/hist2"));
        app.ingest_discovered(vec![vec!["/disc/a".to_string(), "/disc/b".to_string()]]);
        assert_eq!(app.selected_raw(), before);
    }

    #[test]
    fn ctrl_d_on_discovered_row_arms_confirmation_too() {
        // Discovered rows have no history entry, but Ctrl+D now excludes rather
        // than deletes, and excluding noise out of a 50k pool is exactly what a
        // discovered row needs. Refusing here would leave the discovery layer
        // with no in-TUI way to get quieter.
        let mut app =
            App::with_preview_worker(build_candidates(&recs(&[("/hist", 0.9)])), None, false);
        app.ingest_discovered(vec![vec!["/disc/a".to_string()]]);
        app.set_selected(1);
        assert_eq!(
            app.selected_candidate().unwrap().source,
            CandidateSource::Discovered
        );
        let result = handle_key_normal(&mut app, KeyCode::Char('d'), KeyModifiers::CONTROL);
        assert!(result.is_none());
        assert!(matches!(app.mode, Mode::ConfirmDelete { .. }));
    }

    #[test]
    fn ctrl_d_on_history_row_still_arms_confirmation() {
        // Guard the mutation the other way: History rows keep the delete flow.
        let mut app =
            App::with_preview_worker(build_candidates(&recs(&[("/hist", 0.9)])), None, false);
        let result = handle_key_normal(&mut app, KeyCode::Char('d'), KeyModifiers::CONTROL);
        assert!(result.is_none());
        assert!(matches!(app.mode, Mode::ConfirmDelete { .. }));
    }

    #[test]
    fn bootstrap_seeds_pwd_ancestors_and_children() {
        let tree = {
            let stamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            std::env::temp_dir().join(format!("cdh-bootstrap-{}-{stamp}", std::process::id()))
        };
        fs::create_dir_all(tree.join("child_a")).unwrap();
        fs::create_dir_all(tree.join("child_b")).unwrap();
        let mut app = App::with_preview_worker(Vec::new(), None, false);
        app.bootstrap_from_pwd(&tree.to_string_lossy());
        let raws: HashSet<String> = app.candidates.iter().map(|c| c.raw.clone()).collect();
        // $PWD itself, its children, and at least one ancestor are present.
        assert!(raws.contains(&tree.to_string_lossy().into_owned()));
        assert!(raws.contains(&tree.join("child_a").to_string_lossy().into_owned()));
        assert!(raws.contains(&tree.join("child_b").to_string_lossy().into_owned()));
        assert!(app
            .candidates
            .iter()
            .all(|c| c.source == CandidateSource::Discovered));
        let _ = fs::remove_dir_all(&tree);
    }

    #[test]
    fn ingest_after_deleting_all_history_does_not_panic() {
        // Repro: delete every history row (Ctrl+D's main use), then a small
        // discovery batch arrives. If `discovered_start` isn't decremented on
        // removal it stays stale-high and `candidates[discovered_start..]` panics
        // once the start exceeds the pool length.
        let mut app = App::with_preview_worker(
            build_candidates(&recs(&[("/h1", 0.9), ("/h2", 0.8)])),
            None,
            false,
        );
        assert_eq!(app.discovered_start, 2);
        app.exclude_subtree("/h1");
        app.exclude_subtree("/h2");
        assert!(app.candidates.is_empty());
        // Must not panic on the slice inside ingest.
        app.ingest_discovered(vec![vec!["/d/a".to_string()]]);
        assert_eq!(app.candidates.len(), 1);
        assert_eq!(app.candidates[0].source, CandidateSource::Discovered);
    }

    #[test]
    fn ingest_after_deleting_history_keeps_discovered_suffix_sorted() {
        // Deleting a history row must shift `discovered_start` so the whole
        // discovered suffix stays in the sort window; otherwise the leading
        // discovered rows freeze in insertion order.
        let mut app = App::with_preview_worker(
            build_candidates(&recs(&[("/h1", 0.9), ("/h2", 0.8)])),
            None,
            false,
        );
        app.ingest_discovered(vec![vec!["/d/z".to_string()]]);
        app.exclude_subtree("/h1"); // history prefix shrinks by one
        app.ingest_discovered(vec![vec!["/d/a".to_string()]]);
        let discovered: Vec<_> = app
            .candidates
            .iter()
            .filter(|candidate| candidate.source == CandidateSource::Discovered)
            .map(|candidate| candidate.raw.clone())
            .collect();
        // Both score 0 -> path ascending across the whole suffix.
        assert_eq!(discovered, vec!["/d/a".to_string(), "/d/z".to_string()]);
    }
}
