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
    // Through the render-to-mouse mapping: a rendered frame publishes
    // the list geometry, and a click inside row N of that geometry
    // selects result N. The expected row comes from the independently
    // derived list area, not from the renderer's output.
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
fn mouse_click_maps_screen_rows_through_the_page_start() {
    // A page-2 row sits at screen offset N but global index
    // page_size + N -- the published geometry's `start` half is what
    // carries that offset. Expected indices come from the independently
    // derived page size (the list area's height), not from `PageWindow`.
    let full = Rect::new(0, 0, 80, 24);
    let list = independently_derived_list_area(full);
    let page_size = list.height as usize;

    let mut app = list_render_app(full, 25);
    app.set_selected(page_size);
    render_frame(&mut app, full.width, full.height);
    click(&mut app, list.x + 4, list.y + 3);
    assert_eq!(app.selected_index, page_size + 3);
    assert_eq!(
        app.selected_raw().as_deref(),
        Some("/home/jason/workspace/project-22")
    );

    // A later page on a bigger pool, so the offset under test is never
    // the first page size.
    let mut app = list_render_app(full, 45);
    app.set_selected(page_size * 2);
    render_frame(&mut app, full.width, full.height);
    click(&mut app, list.x + 2, list.y + 5);
    assert_eq!(app.selected_index, page_size * 2 + 5);
    assert_eq!(
        app.selected_raw().as_deref(),
        Some("/home/jason/workspace/project-43")
    );
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

/// A full frame plus the list geometry stored back the way `run_ui`
/// stores it after a successful draw. This is a test-side mirror of that
/// handoff, not the production path: the geometry comes from the same
/// `draw` return value the event loop consumes, and the click tests then
/// exercise `handle_mouse`'s row-to-result mapping against it.
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
    let (root, app) = settings_mode_app("flat-main", None, UiEnvironment::default(), Language::En);
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
        Some("language = \"en\"\ntheme = \"nord\"\npreview = false\ncolor = true\nmouse = true\n"),
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
    let mut terminal_failure =
        FakeMouseCaptureControl::with_results(true, vec![Err(io::Error::other("terminal failed"))]);
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
            UnicodeWidthStr::width(buffer_row_range(&buffer, y, layout.list.x, list_end).trim_end())
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
    assert!(crate::excludes::Excludes::load(&ctx.paths.excludes).contains(stale.to_str().unwrap()));
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
    assert!(crate::excludes::Excludes::load(&ctx.paths.excludes).contains(&noise.to_string_lossy()));
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
    let mut app = App::with_preview_worker(build_candidates(&recs(&[("/h", 0.9)])), None, false);
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
    let mut app = App::with_preview_worker(build_candidates(&recs(&[("/h", 0.9)])), None, false);
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
    let mut app = App::with_preview_worker(build_candidates(&recs(&[("/a", 0.9)])), None, false);
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
    let mut app = App::with_preview_worker(build_candidates(&recs(&[("/a", 0.9)])), None, false);
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
    let mut app = App::with_preview_worker(build_candidates(&recs(&[("/a", 0.9)])), None, false);
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
    let mut app = App::with_preview_worker(build_candidates(&recs(&[("/a", 0.9)])), None, false);
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
    // Holds the same lock as the injected-GIT_DIR test in `git`: while that
    // test mutates the process environment, its variables must not leak into
    // this test's own `git init` (which has no hardening of its own).
    let _guard = git::GIT_ENV_LOCK.lock().unwrap();
    let (root, _) = test_ctx("git_status");
    let repo = root.join("repo");
    fs::create_dir_all(&repo).unwrap();
    assert!(Command::new("git")
        .args(["init", "-q"])
        .current_dir(&repo)
        .status()
        .unwrap()
        .success());
    assert_eq!(git::read_git_info(&repo).unwrap().dirty, Some(false));
    fs::write(repo.join("note.txt"), "dirty").unwrap();
    assert_eq!(git::read_git_info(&repo).unwrap().dirty, Some(true));
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
    let mut app = App::with_preview_worker(build_candidates(&recs(&[("/a/b", 0.9)])), None, false);
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
    let mut app = App::with_preview_worker(build_candidates(&recs(&[("/hist", 0.9)])), None, false);
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
    let mut app = App::with_preview_worker(build_candidates(&recs(&[("/hist", 0.9)])), None, false);
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
