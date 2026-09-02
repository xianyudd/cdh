//! Keyboard-first interactive directory picker.
//!
//! The picker keeps ranking and filesystem work outside the rendering path:
//! filtering happens on input, preview I/O runs on a dedicated worker, and
//! drawing only formats the current page of already prepared data.

#[path = "picker_cube.rs"]
mod cube;
#[path = "picker_i18n.rs"]
mod i18n;
#[path = "picker_overlays.rs"]
mod overlays;
#[path = "tui_settings.rs"]
mod settings;

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
    LanguagePreference, SettingKey, SettingLocks, SettingsLoad, UiEnvironment, UiPreferences,
    UiSettings,
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
/// Cube columns plus one blank separator column, carved out of the content area.
const CORNER_3D_GUTTER: u16 = cube::WIDTH + 1;
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

    /// Which palette slots the ambient cube shades between. Picking the slots is
    /// theme business, so it lives here; what the cube does with them (the
    /// shadow pull, the ramp, the sub-pixel rasterizer) is entirely its own and
    /// lives in `cube`.
    ///
    /// Hands over the raw seeds rather than resolved `Color`s because the ramp
    /// interpolates in RGB space, and skips the `on` check because the cube only
    /// ever draws when `App::corner_3d_enabled` already required color.
    fn cube_ink(&self) -> cube::Ink {
        cube::Ink {
            accent: self.palette.accent,
            surface: self.palette.surface,
            highlight: self.palette.title,
        }
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
            render_confirm_delete(frame, view, theme, overlay_area, candidate_idx)
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

fn render_confirm_delete(
    frame: &mut Frame,
    view: &FrameView,
    theme: &Theme,
    full: Rect,
    candidate_idx: usize,
) {
    let width = 56u16.min(full.width.saturating_sub(4));
    let path = view
        .candidates
        .get(candidate_idx)
        .map(|candidate| candidate.display(view.home).text)
        .unwrap_or_else(|| view.language.text(TextKey::UnknownDirectory).to_string());
    let message = confirm_delete_message(&path, width.saturating_sub(2) as usize, view.language);
    let lines = vec![
        Line::from(Span::styled(
            view.language.text(TextKey::ConfirmDeleteTitle),
            theme.title(),
        )),
        Line::raw(""),
        Line::from(Span::styled(message, theme.primary())),
        Line::raw(""),
        Line::from(Span::styled(
            view.language.text(TextKey::ConfirmDeleteAgain),
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
    use ratatui::{
        backend::TestBackend,
        buffer::{Buffer, Cell as BufferCell},
    };
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

    /// `list_render_app` with the preview panel on, for click tests that need
    /// chrome beside the list. The worker stays absent: rendering never
    /// consults it, only the event loop does.
    fn preview_click_app(full: Rect, count: usize) -> App {
        let mut app = list_render_app(full, count);
        app.preview_visible = true;
        app.set_page_size(page_size_for(full, true, false));
        app
    }

    #[test]
    fn mouse_click_selects_the_clicked_list_row() {
        // End to end through the render-to-mouse channel: a rendered frame
        // publishes the list geometry, and a click inside row N of that
        // geometry selects result N. The expected row comes from the
        // independently derived list area, not from the renderer's output.
        let full = Rect::new(0, 0, 80, 24);
        let list = independently_derived_list_area(full);

        let mut app = list_render_app(full, 6);
        render_frame(&mut app, full.width, full.height);
        click(&mut app, list.x + 4, list.y + 2);
        assert_eq!(app.selected_index, 2);
        assert_eq!(
            app.selected_raw().as_deref(),
            Some("/home/jason/workspace/project-02")
        );

        // A different row on a different candidate set, so the hit is not an
        // accident of one geometry or one pool.
        let mut app = list_render_app(full, 12);
        render_frame(&mut app, full.width, full.height);
        click(&mut app, list.x + 1, list.y + 7);
        assert_eq!(app.selected_index, 7);
        assert_eq!(
            app.selected_raw().as_deref(),
            Some("/home/jason/workspace/project-07")
        );
    }

    #[test]
    fn mouse_click_outside_the_list_keeps_the_selection() {
        let full = Rect::new(0, 0, 80, 24);
        let list = independently_derived_list_area(full);
        let mut app = list_render_app(full, 6);
        app.set_selected(1);
        render_frame(&mut app, full.width, full.height);

        // Below the list (the divider/footer rows) and the side padding
        // column at a valid list row are both off-target.
        click(&mut app, list.x + 4, list.y + list.height);
        click(&mut app, list.x.saturating_sub(1), list.y + 1);
        assert_eq!(app.selected_index, 1);

        // The preview panel is chrome, not a list row: a click there must not
        // move the selection either. The click column is read off the frame
        // itself -- wherever the preview's own placeholder renders is inside
        // the preview panel by construction, and never inside the list.
        let wide = Rect::new(0, 0, 110, 24);
        let mut app = preview_click_app(wide, 6);
        app.set_selected(1);
        let buffer = render_frame(&mut app, wide.width, wide.height);
        let loading_row = buffer_row_containing(&buffer, "加载中");
        let preview_column = (0..wide.width)
            .find(|&x| buffer[(x, loading_row)].symbol() == "加")
            .expect("loading glyph on the preview row");
        click(&mut app, preview_column, loading_row);
        assert_eq!(app.selected_index, 1);
    }

    #[test]
    fn mouse_click_below_the_last_row_of_a_short_tail_page_keeps_the_selection() {
        let full = Rect::new(0, 0, 80, 24);
        let list = independently_derived_list_area(full);
        let mut app = list_render_app(full, 3);
        app.set_selected(1);
        render_frame(&mut app, full.width, full.height);
        // The page holds 3 of 19 rows; the row under the last populated one is
        // inside the terminal but past the results.
        click(&mut app, list.x + 4, list.y + 3);
        assert_eq!(app.selected_index, 1);

        // The same guard when the geometry is a frame behind the pool: the
        // results shrank after the last draw, so the hit passes the area check
        // but lands past the end of the filtered set.
        let mut app = list_render_app(full, 6);
        render_frame(&mut app, full.width, full.height);
        app.filtered_results.truncate(2);
        click(&mut app, list.x + 4, list.y + 3);
        assert_eq!(app.selected_index, 0);
    }

    #[test]
    fn restore_screen_emits_mouse_cleanup_before_leaving_the_alternate_screen() {
        // The panic hook passes `true` unconditionally because it cannot see the
        // guard; `Drop` passes the real flag. The mouse bytes must be the only
        // difference, and they must come first -- if they trailed the screen
        // exit they would be written to the primary screen instead.
        let mut with_mouse = Vec::new();
        let mut without_mouse = Vec::new();
        restore_screen(&mut with_mouse, true).unwrap();
        restore_screen(&mut without_mouse, false).unwrap();

        assert!(!without_mouse.is_empty());
        assert!(with_mouse.len() > without_mouse.len());
        assert!(
            with_mouse.ends_with(&without_mouse),
            "mouse cleanup must precede the screen restore"
        );
    }

    #[test]
    fn restore_screen_returns_to_the_primary_screen_last() {
        // The panic message is printed after `restore_screen` returns, so the
        // alternate-screen exit has to be the final thing written or the message
        // is discarded along with the screen.
        // (Mutation check: move `LeaveAlternateScreen` ahead of `Show` and this fails.)
        let mut emitted = Vec::new();
        restore_screen(&mut emitted, false).unwrap();

        let mut leave = Vec::new();
        crossterm::queue!(leave, LeaveAlternateScreen).unwrap();

        assert!(
            emitted.ends_with(&leave),
            "expected the output to end with LeaveAlternateScreen"
        );
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

    /// One full frame of `draw` on a `TestBackend`, for assertions that need the
    /// composed screen rather than a single widget's `Line`s. The cube angle is
    /// pinned (see `TEST_CUBE_ANGLE`) so a frame is reproducible.
    fn render_buffer(app: &App, width: u16, height: u16, color: bool) -> Buffer {
        render_buffer_at(app, width, height, color, unix_now())
    }

    /// `render_buffer` with the frame clock supplied, for assertions that pin
    /// time-dependent copy.
    fn render_buffer_at(app: &App, width: u16, height: u16, color: bool, now_unix: i64) -> Buffer {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                draw(
                    frame,
                    &app.view(now_unix),
                    &Theme::new(color),
                    TEST_CUBE_ANGLE,
                );
            })
            .unwrap();
        terminal.backend().buffer().clone()
    }

    /// Derive the cube gutter from the terminal contract rather than from the
    /// production `ScreenLayout`. The layout has one side-padding column, three
    /// fixed rows above content, and two fixed rows below it; the cube constants
    /// then determine the reserved right-hand footprint.
    fn independently_derived_cube_gutter(full: Rect) -> Rect {
        const SIDE_PADDING: u16 = 1;
        const CONTENT_TOP_ROWS: u16 = 3;
        const CONTENT_BOTTOM_ROWS: u16 = 2;
        assert!(full.width >= 81);
        assert!(full.height >= 12);
        let content_width = full.width - SIDE_PADDING * 2;
        let content_height = full.height - CONTENT_TOP_ROWS - CONTENT_BOTTOM_ROWS;
        assert!(content_height >= cube::HEIGHT);
        assert!(content_width >= CORNER_3D_GUTTER + CORNER_3D_MIN_CONTENT);
        Rect::new(
            full.x + SIDE_PADDING + content_width - CORNER_3D_GUTTER + 1,
            full.y + CONTENT_TOP_ROWS + content_height - cube::HEIGHT,
            cube::WIDTH,
            cube::HEIGHT,
        )
    }

    /// Derive the list area from the terminal contract rather than from the
    /// production `ScreenLayout`: one side-padding column, three fixed rows
    /// above the content, two fixed rows below it, and no preview or cube.
    /// The click tests below turn rows into expected selections through this
    /// derivation, so a renderer that moved the list fails them.
    fn independently_derived_list_area(full: Rect) -> Rect {
        const SIDE_PADDING: u16 = 1;
        const CONTENT_TOP_ROWS: u16 = 3;
        const CONTENT_BOTTOM_ROWS: u16 = 2;
        Rect::new(
            full.x + SIDE_PADDING,
            full.y + CONTENT_TOP_ROWS,
            full.width.saturating_sub(SIDE_PADDING * 2),
            full.height
                .saturating_sub(CONTENT_TOP_ROWS + CONTENT_BOTTOM_ROWS),
        )
    }

    /// A full frame plus the mouse-hit geometry a real frame publishes: the
    /// production loop stores the list geometry after drawing, and the click
    /// tests exercise exactly that render-to-mouse channel end to end. This
    /// is `render_buffer` for callers that go on to click.
    fn render_frame(app: &mut App, width: u16, height: u16) -> Buffer {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        let now_unix = unix_now();
        let mut list_geometry = None;
        terminal
            .draw(|frame| {
                list_geometry = draw(
                    frame,
                    &app.view(now_unix),
                    &Theme::new(true),
                    TEST_CUBE_ANGLE,
                );
            })
            .unwrap();
        if let Some(geometry) = list_geometry {
            app.last_list_area = geometry.area;
            app.last_list_start = geometry.start;
        }
        terminal.backend().buffer().clone()
    }

    /// A left-button press at `column`/`row`, as crossterm would deliver it.
    fn click(app: &mut App, column: u16, row: u16) {
        handle_mouse(
            app,
            event::MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column,
                row,
                modifiers: KeyModifiers::NONE,
            },
        );
    }

    fn cube_render_app(full: Rect) -> App {
        let mut app = app_with_paths(&[("/home/jason/workspace/project", 0.9)]);
        app.color_enabled = true;
        app.corner_3d_env = true;
        app.home = Some("/home/jason".to_string());
        app.set_page_size(page_size_for(full, false, true));
        app
    }

    fn gutter_cells(buffer: &Buffer, gutter: Rect) -> Vec<BufferCell> {
        (gutter.y..gutter.y + gutter.height)
            .flat_map(|y| (gutter.x..gutter.x + gutter.width).map(move |x| buffer[(x, y)].clone()))
            .collect()
    }

    /// The whole buffer as text, one line per row.
    fn buffer_text(buffer: &Buffer) -> String {
        let area = buffer.area;
        (area.y..area.y + area.height)
            .map(|y| buffer_row(buffer, y))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// One buffer row as text. A double-width symbol occupies two cells and
    /// ratatui leaves the second one blank, so that filler is dropped to keep the
    /// string aligned with what the terminal shows.
    fn buffer_row(buffer: &Buffer, y: u16) -> String {
        let area = buffer.area;
        buffer_row_in(buffer, y, area.x, area.x + area.width)
    }

    /// `buffer_row` restricted to the columns `x0..x1`, so an assertion can read
    /// the list or an overlay without picking up the chrome beside it.
    ///
    /// Contrast `buffer_row_range`, which keeps one entry per cell: use that one
    /// for geometry (widths, truncation), this one for text.
    fn buffer_row_in(buffer: &Buffer, y: u16, x0: u16, x1: u16) -> String {
        let mut text = String::new();
        let mut previous_was_wide = false;
        for x in x0..x1 {
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

    /// The text inside a rectangle, one line per row.
    fn box_text(buffer: &Buffer, area: Rect) -> String {
        (area.y..area.y + area.height)
            .map(|y| buffer_row_in(buffer, y, area.x, area.x + area.width))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Row index of the first row containing `needle`. Panics rather than
    /// returning an `Option` so a missing row fails at the assertion site.
    fn buffer_row_containing(buffer: &Buffer, needle: &str) -> u16 {
        let area = buffer.area;
        (area.y..area.y + area.height)
            .find(|y| buffer_row(buffer, *y).contains(needle))
            .unwrap_or_else(|| panic!("missing rendered row containing {needle:?}"))
    }

    /// Bounding box of the overlay panel on screen, found by its elevated fill.
    ///
    /// All four overlays open the same way -- `centered`, then `Clear`, then a
    /// `theme.panel()`-styled `Block` -- and nothing else on screen uses that
    /// background, so the panel-filled cells are exactly the panel. Deriving the
    /// box from the pixels rather than recomputing `centered` is deliberate: a
    /// renderer that swapped `centered` for fixed coordinates would still agree
    /// with a recomputed expectation.
    fn panel_box(buffer: &Buffer, theme: &Theme) -> Rect {
        let panel_bg = theme.panel().bg.expect("panel fill needs a color");
        let area = buffer.area;
        let filled = (area.y..area.y + area.height)
            .flat_map(|y| (area.x..area.x + area.width).map(move |x| (x, y)))
            .filter(|(x, y)| buffer[(*x, *y)].bg == panel_bg)
            .collect::<Vec<_>>();
        assert!(!filled.is_empty(), "no panel-filled cell on screen");
        let x0 = filled.iter().map(|(x, _)| *x).min().unwrap();
        let x1 = filled.iter().map(|(x, _)| *x).max().unwrap();
        let y0 = filled.iter().map(|(_, y)| *y).min().unwrap();
        let y1 = filled.iter().map(|(_, y)| *y).max().unwrap();
        Rect::new(x0, y0, x1 - x0 + 1, y1 - y0 + 1)
    }

    /// The 1-based result number a rendered list row shows, read back out of its
    /// index column. Rows are numbered across the whole result set rather than
    /// per page, which is what makes this worth asserting.
    fn row_result_number(row: &str) -> Option<usize> {
        row.trim_start_matches(['›', ' '])
            .split_whitespace()
            .next()?
            .parse()
            .ok()
    }

    /// A main-screen app pinned for full-frame assertions: a fixed candidate set,
    /// a fixed `$HOME` instead of the ambient one, and the page size the real
    /// layout would pick for `full`.
    ///
    /// The cube remains visible while an overlay is open, so this helper keeps
    /// the cube off to isolate ordinary overlay rendering tests from chrome.
    /// Dedicated cube/overlay guardrails below enable it explicitly.
    fn list_render_app(full: Rect, count: usize) -> App {
        let paths = (0..count)
            .map(|index| format!("/home/jason/workspace/project-{index:02}"))
            .collect::<Vec<_>>();
        let records = paths
            .iter()
            .enumerate()
            .map(|(index, path)| (path.as_str(), 0.9 - index as f64 * 0.001))
            .collect::<Vec<_>>();
        let mut app = app_with_paths(&records);
        app.color_enabled = true;
        app.corner_3d_env = false;
        app.home = Some("/home/jason".to_string());
        app.set_page_size(page_size_for(full, false, app.corner_3d_enabled()));
        app
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
                text.push_str(&buffer_text(&render_buffer(&app, 80, 24, true)));
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

        let text = buffer_text(&render_buffer(&app, 80, 24, true));

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
            .draw(|frame| {
                draw(frame, &app.view(0), &theme, TEST_CUBE_ANGLE);
            })
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
        let buffer = render_buffer(&app, 80, 24, true);
        let y = buffer_row_containing(&buffer, "Preview on startup");
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
        let buffer = render_buffer(&app, 80, 24, false);
        let y = buffer_row_containing(&buffer, "Color");
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
            let buffer = render_buffer(&app, width, height, true);
            assert_eq!(buffer.area.width, width);
            assert_eq!(buffer.area.height, height);
        }
        let narrow = buffer_text(&render_buffer(&app, 24, 12, true));
        assert!(narrow.contains("设置"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn settings_panel_f2_copy_is_distinct_in_footer_and_help() {
        for language in [Language::ZhCn, Language::En] {
            let footer = language.text(TextKey::FooterPrimary);
            assert!(footer.contains("F1"));
            assert!(footer.contains("F2"));

            let help = overlays::help_lines(language, &Theme::new(false));
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
        let buffer = render_buffer(&app, full.width, full.height, true);
        let text = buffer_text(&buffer);
        assert!(text.contains("cdh"));
        assert!(
            !text.chars().any(|c| ('\u{2800}'..='\u{28FF}').contains(&c)),
            "CDH_CORNER_3D=0 must draw no cube: {text:?}"
        );
    }

    #[test]
    fn corner_gutter_is_reserved_outside_the_list_and_preview() {
        let full = Rect::new(0, 0, 100, 24);
        let plain = screen_layout(full, false, false).expect("roomy terminal");
        assert!(plain.corner.is_none());

        let layout = screen_layout(full, false, true).expect("roomy terminal");
        let corner = layout.corner.expect("cube gutter");
        assert_eq!(corner.width, cube::WIDTH);
        assert_eq!(corner.height, cube::HEIGHT);
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
        let buffer = render_buffer(&app, full.width, full.height, true);

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
            let buffer = render_buffer(app, full.width, full.height, true);
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

    /// Where the cube's ink actually lands. Every other corner assertion is about
    /// what the cube must *not* disturb (row widths, the selection bar, colorless
    /// mode), all of which stay true if the cube renders a cell off from its
    /// gutter -- the drift would just eat the blank separator column and leave the
    /// screen's last column empty. This is the one test that pins the position.
    #[test]
    fn corner_cube_ink_lands_flush_in_its_reserved_gutter() {
        let full = Rect::new(0, 0, 100, 24);
        let corner = screen_layout(full, false, true)
            .unwrap()
            .corner
            .expect("100x24 is roomy enough for the gutter");
        let mut app = app_with_paths(&[("/tmp/cdh-corner-alpha", 0.9)]);
        app.color_enabled = true;
        app.corner_3d_env = true;
        let buffer = render_buffer(&app, full.width, full.height, true);

        let ink: Vec<(u16, u16)> = (0..full.height)
            .flat_map(|y| (0..full.width).map(move |x| (x, y)))
            .filter(|(x, y)| {
                buffer[(*x, *y)]
                    .symbol()
                    .chars()
                    .any(|c| ('\u{2800}'..='\u{28FF}').contains(&c))
            })
            .collect();
        assert!(!ink.is_empty(), "the cube should have drawn braille");
        let bounds = (
            ink.iter().map(|(x, _)| *x).min().unwrap(),
            ink.iter().map(|(_, y)| *y).min().unwrap(),
            ink.iter().map(|(x, _)| *x).max().unwrap(),
            ink.iter().map(|(_, y)| *y).max().unwrap(),
        );
        // Three of the four bounds are the gutter's own edges: the cube fills its
        // rect flush right and flush bottom, so a one-cell drift in any direction
        // moves at least one of them. The left inset is one column because that is
        // the silhouette at `TEST_CUBE_ANGLE`, not a layout rule -- change the
        // pinned angle or the projection constants and this number moves with it.
        assert_eq!(
            bounds,
            (
                corner.x + 1,
                corner.y,
                corner.x + corner.width - 1,
                corner.y + corner.height - 1
            ),
            "cube ink is at {bounds:?}, gutter is {corner:?}"
        );
    }

    #[test]
    fn corner_3d_render_is_a_no_op_when_colorless() {
        let mut app = app_with_paths(&[("/tmp/cdh-corner-alpha", 0.9)]);
        app.color_enabled = false;
        // Deliberately a terminal roomy enough for the gutter. At the 60 columns
        // this used to render at, `reserve_corner_gutter` drops the cube on width
        // alone (60 < CORNER_3D_GUTTER + CORNER_3D_MIN_CONTENT), so the assertion
        // held whether or not the color gate worked and pinned nothing. Only
        // `App::corner_3d_enabled` can keep the cube off a terminal this size,
        // which is what makes this the far-side guard for `cube::render` emitting
        // real `Color::Rgb` unconditionally.
        let buffer = render_buffer(&app, 100, 24, false);
        let text = buffer_text(&buffer);
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
            "↑↓ Select · Ctrl+↑↓ Page · Ctrl+H Hidden dirs · Enter Jump · Tab Preview · F1 Help · F2 Settings · Esc Exit"
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
        let lines = overlays::help_lines(Language::En, &theme);
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
        assert_eq!(
            fit_footer(full, compact, short, 80),
            "Ctrl+H Hidden dirs · Enter Jump · F1 Help · F2 Settings · Esc Exit"
        );

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
            .draw(|frame| {
                render_input(frame, &app.view(0), &Theme::new(true), frame.area());
            })
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
            "↑↓ 选择 · Ctrl+↑↓ 翻页 · Ctrl+H 隐藏目录 · Enter 跳转 · Tab 预览 · F1 帮助 · F2 设置 · Esc 退出"
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
        let text = overlays::help_lines(Language::ZhCn, &theme)
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
            "Ctrl+H / F5",
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
    fn ctrl_h_and_f5_toggle_hidden_directories() {
        let mut app = app_with_paths(&[
            ("/workspace/visible", 0.9),
            ("/workspace/.cache/project", 0.8),
        ]);
        assert_eq!(app.filtered_results.len(), 2);

        handle_key(&mut app, KeyCode::Char('h'), KeyModifiers::CONTROL, None);
        assert_eq!(app.filtered_results.len(), 1);
        assert_eq!(app.selected_raw().as_deref(), Some("/workspace/visible"));
        assert_eq!(app.notice.as_deref(), Some("已过滤隐藏目录"));

        handle_key(&mut app, KeyCode::F(5), KeyModifiers::NONE, None);
        assert_eq!(app.filtered_results.len(), 2);
        assert_eq!(app.notice.as_deref(), Some("已显示隐藏目录"));

        // Some terminals encode Ctrl+H as Ctrl+Backspace instead of Char('h').
        handle_key(&mut app, KeyCode::Backspace, KeyModifiers::CONTROL, None);
        assert_eq!(app.filtered_results.len(), 1);
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
    fn hidden_directory_toggle_filters_every_hidden_path_component() {
        let candidates = build_candidates(&recs(&[
            ("/workspace/visible", 0.9),
            ("/workspace/.cache/project", 0.8),
            ("/workspace/project/.git", 0.7),
            ("/workspace/project/deep", 0.6),
        ]));
        let mut filter = Filter::new();

        assert_eq!(filter.run(&candidates, "").len(), 4);
        assert!(filter.toggle_hidden());
        let visible = filter
            .run(&candidates, "")
            .into_iter()
            .map(|matched| candidates[matched.idx].raw.as_str())
            .collect::<Vec<_>>();
        assert_eq!(visible, ["/workspace/visible", "/workspace/project/deep"]);

        assert!(!filter.toggle_hidden());
        assert_eq!(filter.run(&candidates, "").len(), 4);
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
        assert_eq!(overlays::excludes_layout(12), (9, true));
        assert_eq!(overlays::excludes_layout(4), (1, true));
        assert_eq!(overlays::excludes_layout(3), (1, false));
        assert_eq!(overlays::excludes_layout(2), (0, false));
        assert_eq!(overlays::excludes_layout(0), (0, false));
        // Rows must never reach the footer row.
        for height in 4..40u16 {
            let (rows, _) = overlays::excludes_layout(height);
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
        assert_eq!(overlays::excludes_window_start(3, 8, 2), 0);
        // Cursor drags the window down one row at a time...
        assert_eq!(overlays::excludes_window_start(20, 5, 4), 0);
        assert_eq!(overlays::excludes_window_start(20, 5, 5), 1);
        // ...and the last page stays full instead of scrolling past the end.
        assert_eq!(overlays::excludes_window_start(20, 5, 19), 15);
        // Degenerate heights must not underflow.
        assert_eq!(overlays::excludes_window_start(0, 1, 0), 0);
        assert_eq!(overlays::excludes_window_start(4, 1, 3), 3);
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
        let empty = buffer_text(&render_buffer(&app, 60, 20, true));
        assert!(empty.contains("排除清单"));
        assert!(empty.contains("清单为空"));

        app.excludes = crate::excludes::Excludes::from_paths(["/x/one", "/y/two", "/z/three"]);
        app.mode = Mode::Excludes { selected: 2 };
        let listed = buffer_text(&render_buffer(&app, 60, 20, true));
        assert!(listed.contains("/x/one"));
        assert!(listed.contains("/z/three"));

        // Terminal sizes that leave no room for the panel body must not panic.
        for (width, height) in [(24, 12), (10, 4), (3, 3), (1, 1)] {
            let buffer = render_buffer(&app, width, height, true);
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

    // ---------------------------------------------------------------------
    // Full-frame render assertions (roadmap 4.1).
    //
    // Everything above this point asserts on what a *helper* returns: a
    // `Line`'s spans, a `PageWindow`'s arithmetic, a `ScreenLayout`'s rects.
    // Those stay green when `draw` wires the helpers together wrongly, which is
    // precisely the failure mode splitting this file can introduce. The tests
    // below therefore go through `draw` onto a `TestBackend` and assert on the
    // cells: which row is highlighted, which columns are underlined, which
    // results are on the page, and where an overlay lands.
    // ---------------------------------------------------------------------

    #[test]
    fn overlays_leave_the_independently_derived_cube_gutter_free_of_panel_content() {
        let full = Rect::new(0, 0, 81, 12);
        let gutter = independently_derived_cube_gutter(full);
        let theme = Theme::new(true);
        let surface_bg = theme.surface().bg.expect("surface fill needs a color");

        for mode in [
            Mode::Help,
            Mode::Settings { selected: 2 },
            Mode::Excludes { selected: 0 },
            Mode::ConfirmDelete { candidate_idx: 0 },
        ] {
            let mut app = cube_render_app(full);
            app.mode = mode;
            let buffer = render_buffer(&app, full.width, full.height, true);
            let cells = gutter_cells(&buffer, gutter);

            assert!(
                cells.iter().all(|cell| {
                    let symbol = cell.symbol();
                    let is_cube_glyph = symbol.is_empty()
                        || symbol == " "
                        || symbol
                            .chars()
                            .all(|ch| ('\u{2800}'..='\u{28ff}').contains(&ch));
                    cell.bg == surface_bg && is_cube_glyph
                }),
                "{mode:?} painted overlay content into the cube gutter"
            );
        }
    }

    #[test]
    fn overlays_preserve_cube_gutter_cells_byte_for_byte() {
        let full = Rect::new(0, 0, 81, 12);
        let gutter = independently_derived_cube_gutter(full);
        let mut normal_app = cube_render_app(full);
        normal_app.mode = Mode::Normal;
        let normal = render_buffer(&normal_app, full.width, full.height, true);
        let normal_cells = gutter_cells(&normal, gutter);
        assert!(
            normal_cells.iter().any(|cell| cell
                .symbol()
                .chars()
                .any(|ch| ('\u{2800}'..='\u{28ff}').contains(&ch))),
            "the enabled cube must draw into the independently derived gutter"
        );

        for mode in [
            Mode::Help,
            Mode::Settings { selected: 4 },
            Mode::Excludes { selected: 0 },
            Mode::ConfirmDelete { candidate_idx: 0 },
        ] {
            let mut overlay_app = cube_render_app(full);
            overlay_app.mode = mode;
            let overlay = render_buffer(&overlay_app, full.width, full.height, true);
            assert_eq!(
                gutter_cells(&overlay, gutter),
                normal_cells,
                "{mode:?} changed cells in the independently derived cube gutter"
            );
        }
    }
    #[test]
    fn selected_row_is_the_only_highlighted_row_and_carries_the_marker() {
        let full = Rect::new(0, 0, 80, 24);
        let list = screen_layout(full, false, false)
            .expect("roomy terminal")
            .list;
        let mut app = list_render_app(full, 25);
        app.set_selected(4);

        let theme = Theme::new(true);
        let selected_bg = theme.selected().bg.unwrap();
        let buffer = render_buffer(&app, full.width, full.height, true);

        let highlighted = (list.y..list.y + list.height)
            .filter(|y| buffer[(list.x, *y)].bg == selected_bg)
            .collect::<Vec<_>>();
        assert_eq!(
            highlighted,
            vec![list.y + 4],
            "exactly one list row may carry the selection background"
        );
        // Edge to edge, so the eye reads one band rather than a ragged stripe.
        assert!(
            (list.x..list.x + list.width).all(|x| buffer[(x, list.y + 4)].bg == selected_bg),
            "selection background must span the whole row: {:?}",
            buffer_row_in(&buffer, list.y + 4, list.x, list.x + list.width)
        );
        let marked = (list.y..list.y + list.height)
            .filter(|y| buffer[(list.x, *y)].symbol() == "›")
            .collect::<Vec<_>>();
        assert_eq!(
            marked,
            vec![list.y + 4],
            "the caret marker belongs to the selected row alone"
        );
    }

    #[test]
    fn selected_row_sits_at_its_page_offset_not_its_absolute_index() {
        let full = Rect::new(0, 0, 80, 24);
        let list = screen_layout(full, false, false)
            .expect("roomy terminal")
            .list;
        let mut app = list_render_app(full, 60);
        // Result 41 lives on page 3. Its absolute index is far below the bottom
        // of the list area, so a renderer that dropped `page.start` would draw
        // off-screen or highlight the wrong row instead of row 3 of the page.
        app.set_selected(40);
        let page = app.page();
        assert_eq!((page.start, page.end, app.current_page), (38, 57, 3));

        let theme = Theme::new(true);
        let buffer = render_buffer(&app, full.width, full.height, true);
        let highlighted = (list.y..list.y + list.height)
            .filter(|y| buffer[(list.x, *y)].bg == theme.selected().bg.unwrap())
            .collect::<Vec<_>>();
        assert_eq!(highlighted, vec![list.y + (40 - page.start) as u16]);

        let row = buffer_row_in(&buffer, highlighted[0], list.x, list.x + list.width);
        assert_eq!(row_result_number(&row), Some(41), "row was {row:?}");
        assert!(row.contains("project-40"), "row was {row:?}");
    }

    #[test]
    fn search_highlight_underlines_exactly_the_matched_columns() {
        // `Modifier::UNDERLINED` is added in exactly one place -- `Theme::matched`
        // -- so the underlined cells of a row *are* its search highlight. The
        // assertion is set equality rather than containment: that is what makes a
        // highlight drifting one column fail here instead of passing quietly.
        let full = Rect::new(0, 0, 80, 24);
        let list = screen_layout(full, false, false)
            .expect("roomy terminal")
            .list;
        let theme = Theme::new(true);
        for raw in [
            "/home/jason/workspace/zebra-tool",
            // A double-width component ahead of the match separates character
            // index from screen column, which is where a mapping bug surfaces.
            "/home/jason/工作区/zebra-tool",
        ] {
            let mut app = app_with_paths(&[(raw, 0.9)]);
            app.color_enabled = true;
            app.corner_3d_env = false;
            app.home = Some("/home/jason".to_string());
            app.set_page_size(page_size_for(full, false, false));
            app.query = "zebra".to_string();
            app.recompute_after_query_change();
            assert_eq!(app.filtered_results.len(), 1, "{raw}");

            let buffer = render_buffer(&app, full.width, full.height, true);
            let symbols = (list.x..list.x + list.width)
                .map(|x| buffer[(x, list.y)].symbol())
                .collect::<Vec<_>>();
            let start = symbols
                .windows(5)
                .position(|window| window.concat() == "zebra")
                .unwrap_or_else(|| panic!("no rendered `zebra` for {raw}: {symbols:?}"));
            let expected = (0..5)
                .map(|offset| list.x + (start + offset) as u16)
                .collect::<Vec<_>>();

            let underlined = (list.x..list.x + list.width)
                .filter(|x| buffer[(*x, list.y)].modifier.contains(Modifier::UNDERLINED))
                .collect::<Vec<_>>();
            assert_eq!(underlined, expected, "highlighted columns for {raw}");
            assert!(
                expected
                    .iter()
                    .all(|x| buffer[(*x, list.y)].fg == theme.match_color()),
                "highlighted cells must use the match color for {raw}"
            );
        }
    }

    #[test]
    fn last_page_renders_its_short_tail_and_blanks_the_remainder() {
        let full = Rect::new(0, 0, 80, 24);
        let list = screen_layout(full, false, false)
            .expect("roomy terminal")
            .list;
        let mut app = list_render_app(full, 25);
        assert_eq!(app.page_size, list.height as usize);

        let numbers = |app: &App| -> Vec<usize> {
            let buffer = render_buffer(app, full.width, full.height, true);
            (list.y..list.y + list.height)
                .filter_map(|y| {
                    row_result_number(&buffer_row_in(&buffer, y, list.x, list.x + list.width))
                })
                .collect()
        };

        // A full page fills the area edge to edge...
        assert_eq!(
            numbers(&app),
            (1..=list.height as usize).collect::<Vec<_>>()
        );

        // ...and the last page picks up where it left off, without repeating the
        // boundary row and without padding the tail out to a full page. Any row
        // left over must be blank rather than stale, so a single equality here
        // covers the window start, the clamped end, and the numbering.
        app.set_selected(list.height as usize);
        assert_eq!(app.current_page, 2);
        assert_eq!(numbers(&app), (20..=25).collect::<Vec<_>>());

        // And the header spells out the same window for the user.
        let buffer = render_buffer(&app, full.width, full.height, true);
        assert!(
            buffer_row(&buffer, full.y).contains("20–25 / 25 · 第 2/2 页"),
            "header was {:?}",
            buffer_row(&buffer, full.y)
        );
    }

    #[test]
    fn empty_result_notice_is_centered_in_an_otherwise_blank_list() {
        let full = Rect::new(0, 0, 80, 24);
        let list = screen_layout(full, false, false)
            .expect("roomy terminal")
            .list;
        let mut app = list_render_app(full, 25);
        app.query = "zzz-no-such-directory".to_string();
        app.recompute_after_query_change();
        assert!(app.filtered_results.is_empty());

        let buffer = render_buffer(&app, full.width, full.height, true);
        let notice = buffer_row_containing(&buffer, "未找到匹配目录");
        assert_eq!(notice, list.y + (list.height - 1) / 2);
        // Every other list row must be empty: a leftover row here would read as
        // a result the filter already rejected.
        for y in list.y..list.y + list.height {
            if y == notice {
                continue;
            }
            assert_eq!(
                buffer_row_in(&buffer, y, list.x, list.x + list.width).trim(),
                "",
                "row {y} should be blank"
            );
        }
        assert!(buffer_row(&buffer, full.y).contains("0 / 0 · 第 0/0 页"));
    }

    #[test]
    fn every_overlay_is_centered_opaque_and_leaves_the_rest_of_the_frame_alone() {
        // Tall enough that all four panels sit strictly inside the screen, so the
        // rows above and below one are real evidence that nothing reflowed.
        let full = Rect::new(0, 0, 100, 40);
        let theme = Theme::new(true);
        let panel_bg = theme.panel().bg.unwrap();
        let surface_bg = theme.surface().bg.unwrap();
        let mut app = list_render_app(full, 60);
        app.excludes = crate::excludes::Excludes::from_paths(["/x/one", "/y/two"]);
        let normal = render_buffer(&app, full.width, full.height, true);

        for (mode, expected) in [
            (Mode::Help, "快捷键"),
            (Mode::Settings { selected: 1 }, "设置"),
            (Mode::Excludes { selected: 1 }, "/y/two"),
            (Mode::ConfirmDelete { candidate_idx: 3 }, "确认排除"),
        ] {
            app.mode = mode;
            let buffer = render_buffer(&app, full.width, full.height, true);
            let panel = panel_box(&buffer, &theme);
            let label = format!("{mode:?}");

            // `centered` leaves at most one column/row of rounding slack.
            let right = full.width - (panel.x + panel.width);
            let bottom = full.height - (panel.y + panel.height);
            assert!(
                panel.x.abs_diff(right) <= 1 && panel.y.abs_diff(bottom) <= 1,
                "{label}: {panel:?} is not centered in {full:?}"
            );
            assert!(
                panel.y > full.y && panel.y + panel.height < full.height,
                "{label}: {panel:?} should not reach the top or bottom edge"
            );

            // Opaque: `Clear` plus the panel `Block` must leave no cell inside
            // the box on the main surface, or list text bleeds through.
            for y in panel.y..panel.y + panel.height {
                for x in panel.x..panel.x + panel.width {
                    assert_ne!(
                        buffer[(x, y)].bg,
                        surface_bg,
                        "{label}: ({x},{y}) still shows the main surface"
                    );
                }
            }
            // The panel's frame -- side columns below the rule, plus the bottom
            // row -- sits outside every renderer's `inner` rect, so only `Clear`
            // can have put anything there. A stray glyph in these cells is list
            // text showing through, which is how a dropped `Clear` reads on
            // screen even though the fill above still looks right.
            for y in panel.y + 1..panel.y + panel.height {
                for x in [panel.x, panel.x + panel.width - 1] {
                    assert_eq!(
                        buffer[(x, y)].symbol(),
                        " ",
                        "{label}: ({x},{y}) is inside the panel frame, not content"
                    );
                }
            }
            assert_eq!(
                buffer_row_in(
                    &buffer,
                    panel.y + panel.height - 1,
                    panel.x,
                    panel.x + panel.width
                )
                .trim(),
                "",
                "{label}: the panel's last row is below every renderer's content"
            );
            // Flat chrome: a rule across the top instead of a box border.
            assert_eq!(
                buffer_row_range(&buffer, panel.y, panel.x, panel.x + panel.width),
                "─".repeat(panel.width as usize),
                "{label}: top rule"
            );
            assert!(
                buffer[(panel.x, panel.y)].bg == panel_bg,
                "{label}: rule row must sit on the panel fill"
            );
            // The mode's own renderer ran, i.e. `draw` dispatched where the mode
            // says. Searched inside the panel because some of this copy also
            // appears in the footer hints underneath.
            assert!(
                box_text(&buffer, panel).contains(expected),
                "{label}: panel is missing {expected:?}:\n{}",
                box_text(&buffer, panel)
            );

            // Opening an overlay must not disturb anything around it: the list
            // underneath keeps its width, contents and selection.
            for y in full.y..full.y + full.height {
                for x in full.x..full.x + full.width {
                    let inside = (panel.x..panel.x + panel.width).contains(&x)
                        && (panel.y..panel.y + panel.height).contains(&y);
                    if inside {
                        continue;
                    }
                    assert_eq!(
                        buffer[(x, y)],
                        normal[(x, y)],
                        "{label}: ({x},{y}) changed outside the panel"
                    );
                }
            }
        }
    }

    /// A preview-visible app with the side panel laid out for `full`, ready
    /// for frame-level assertions about the panel's contents.
    fn preview_panel_app(full: Rect, raw: &str) -> App {
        let mut app = App::with_preview_worker(build_candidates(&recs(&[(raw, 0.9)])), None, true);
        app.home = Some("/home/jason".to_string());
        app.set_page_size(page_size_for(full, true, false));
        app
    }

    #[test]
    fn preview_panel_frame_renders_directory_entries_from_current_data() {
        // Data state: the selected row's preview arrived, so the panel shows
        // its entries -- and no loading placeholder.
        let full = Rect::new(0, 0, 110, 24);
        let mut app = preview_panel_app(full, "/home/jason/target");
        app.preview_current = Some((
            "/home/jason/target".to_string(),
            preview_data(&["alpha", "beta"]),
        ));
        let text = buffer_text(&render_buffer(&app, full.width, full.height, true));
        assert!(text.contains("alpha"), "entry names missing:\n{text}");
        assert!(text.contains("beta"), "entry names missing:\n{text}");
        assert!(!text.contains("加载中"), "data state must not show loading");
    }

    #[test]
    fn preview_panel_frame_renders_loading_placeholder_while_pending() {
        // Loading state: a request for the selected row is in flight, so the
        // panel shows the placeholder instead of whatever it last showed.
        let full = Rect::new(0, 0, 110, 24);
        let mut app = preview_panel_app(full, "/home/jason/target");
        app.preview_loading = Some("/home/jason/target".to_string());
        app.preview_current = Some((
            "/home/jason/target".to_string(),
            preview_data(&["stale-entry"]),
        ));
        let text = buffer_text(&render_buffer(&app, full.width, full.height, true));
        assert!(
            text.contains("加载中…"),
            "loading placeholder missing:\n{text}"
        );
        assert!(
            !text.contains("stale-entry"),
            "loading must win over the previous contents"
        );
    }

    #[test]
    fn preview_last_visit_line_tracks_the_frame_clock() {
        // The "last visit" wording must come from the clock the frame was
        // drawn with, not from a hidden wall-clock read inside the render
        // path: two frames of the same state, two different verdicts.
        let full = Rect::new(0, 0, 110, 24);
        let mut app = preview_panel_app(full, "/home/jason/clock");
        let last_visit = 1_700_000_000;
        app.candidates[0].last_visit = Some(last_visit);
        app.preview_current = Some(("/home/jason/clock".to_string(), preview_data(&["entry"])));

        let recent = buffer_text(&render_buffer_at(
            &app,
            full.width,
            full.height,
            true,
            last_visit + 2 * 60,
        ));
        assert!(recent.contains("2 分钟前"), "recent frame:\n{recent}");
        assert!(!recent.contains("3 天前"), "recent frame:\n{recent}");

        let older = buffer_text(&render_buffer_at(
            &app,
            full.width,
            full.height,
            true,
            last_visit + 3 * 24 * 3600,
        ));
        assert!(older.contains("3 天前"), "older frame:\n{older}");
        assert!(!older.contains("2 分钟前"), "older frame:\n{older}");
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
