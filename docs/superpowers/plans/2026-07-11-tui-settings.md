# TUI Settings Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add an F2 settings overlay that persistently controls TUI language, preview startup, color, and mouse capture without changing non-interactive behavior.

**Architecture:** A focused nested `picker::settings` module owns TOML parsing, per-setting environment precedence, and atomic persistence. `App` owns the loaded settings and extends the existing `Mode` state machine; rendering remains IO-free. A mutable `TermGuard` owns the actual mouse-capture state so runtime toggles and exit restoration use one source of truth.

**Tech Stack:** Rust 1.74, crossterm 0.28, ratatui 0.29, TOML 0.8.23, built-in Rust tests.

**Constraint:** Do not commit or push; the user requested manual verification from the working tree.

---

## Chunk 1: Persistent Settings Core

### Task 1: Add the settings schema and parser

**Files:**
- Modify: `Cargo.toml`
- Modify: `Cargo.lock`
- Create: `src/tui_settings.rs`
- Modify: `src/picker.rs:1-85`

- [ ] **Step 1: Add failing parser/default tests**

Add tests in `src/tui_settings.rs` for:

```rust
#[test]
fn settings_parser_missing_file_uses_defaults_without_warning() { /* ... */ }

#[test]
fn settings_parser_valid_toml_loads_all_preferences() { /* ... */ }

#[test]
fn settings_parser_invalid_known_values_fall_back_independently() { /* ... */ }

#[test]
fn settings_parser_malformed_toml_returns_defaults_with_warning() { /* ... */ }

#[test]
fn settings_parser_unknown_keys_are_ignored() { /* ... */ }
```

The core types are:

```rust
enum LanguagePreference { Auto, ZhCn, En }
enum SettingKey { Language, Preview, Color, Mouse }
struct UiPreferences { language: LanguagePreference, preview: bool, color: bool, mouse: bool }
struct UiEnvironment { language: Option<LanguagePreference>, preview: Option<bool>, color: Option<bool>, mouse: Option<bool> }
struct UiSettings { path: PathBuf, saved: UiPreferences, environment: UiEnvironment }
struct SettingsLoad { settings: UiSettings, warning: Option<String> }
```

- [ ] **Step 2: Run the focused tests and verify RED with nonzero selection**

Run: `cargo test settings_parser_`

Expected: compilation/test failure because implementation does not exist, or a
test failure naming at least one `settings_parser_` test. A successful run with
zero selected tests is not acceptable.

- [ ] **Step 3: Add the exact TOML dependency**

Declare the nested module exactly in `src/picker.rs`:

```rust
#[path = "tui_settings.rs"]
mod settings;
```

Add `toml = "=0.8.23"` and update the lock file. Convert `Cargo.lock` from
format version 4 to version 3, then run
`cargo +1.74.0 check --locked --all-targets`. Cargo 1.74 is installed locally;
this check must pass before UI work starts.

- [ ] **Step 4: Implement parsing and environment resolution**

Load `language`, `preview`, `color`, and `mouse` independently from a TOML
table. Preserve the existing boolean environment semantics (`1`/`true` means
true; any other present value means false). Recognize existing language tags
and lock only recognized language overrides. Unit tests inject a `UiEnvironment`
value directly; they must not mutate process-global environment variables.

Expose methods for:

```rust
fn effective(&self) -> UiPreferences;
fn is_locked(&self, key: SettingKey) -> bool;
fn candidate(&self, key: SettingKey, direction: isize) -> Option<UiPreferences>;
fn path(&self) -> &Path;
```

- [ ] **Step 5: Run parser tests and verify GREEN**

Run: `cargo test settings_parser_`

Expected: output reports at least five selected tests and all pass.

### Task 2: Add atomic persistence

**Files:**
- Modify: `src/tui_settings.rs`

- [ ] **Step 1: Add failing persistence tests**

Use `settings_persistence_` prefixes. Cover TOML round trip, unique temporary
names under concurrent writes, and a write failure that leaves in-memory
settings unchanged.

- [ ] **Step 2: Run focused tests and verify RED**

Run: `cargo test settings_persistence_`

Expected: named failures with at least one selected test, never zero tests.

- [ ] **Step 3: Implement candidate persistence**

Serialize all four known fields, open a unique `.<name>.<pid>.<counter>.tmp` with `create_new`, flush/sync it, and rename it over `tui.toml`. Remove failed candidates best-effort. Update `saved` only after rename succeeds.

- [ ] **Step 4: Run focused and baseline tests**

Run: `cargo test settings_persistence_`

Run: `cargo test --all-targets --all-features`

Expected: all tests pass before UI integration.

### Task 3: Prove non-interactive isolation

**Files:**
- Create: `tests/tui_settings_non_tty.rs`

- [ ] **Step 1: Add a failing integration test**

Launch `CARGO_BIN_EXE_cdh` with piped stdin/stderr and isolated HOME/XDG
directories containing valid history plus malformed `config/cdh/tui.toml`.
Assert the command prints the first recommendation, emits no settings warning,
and does not rewrite `tui.toml`.

- [ ] **Step 2: Run the exact test**

Run: `cargo test --test tui_settings_non_tty settings_non_tty_`

Expected: one selected test. It should pass with the existing post-TTY picker
boundary; if it fails, move loading into `run_ui` before proceeding.

- [ ] **Step 3: Architecture review checkpoint**

Review Chunk 1 for the nested module boundary, injected environment inputs,
non-TTY isolation, unique temporary writes, and Rust 1.74 lockfile support.

## Chunk 2: Terminal State, Mode, and Runtime Application

### Task 4: Make mouse capture mutable before exposing its setting

**Files:**
- Modify: `src/picker.rs:369-405`
- Modify: `src/picker.rs:1220-1275`

- [ ] **Step 1: Add failing fake-controller tests**

Use `settings_mouse_` test prefixes. Introduce an internal
`MouseCaptureControl` boundary and test successful enable/disable, terminal
failure, persistence failure with successful rollback, and double failure while
preserving cleanup state.

- [ ] **Step 2: Run focused tests and verify RED**

Run: `cargo test settings_mouse_`

Expected: named failures with nonzero test selection.

- [ ] **Step 3: Implement mutable TermGuard state and event-loop plumbing**

`set_mouse_capture` executes crossterm first and updates `TermGuard.mouse` only
on success. `Drop` disables capture from that current state. Remove the
immutable startup `mouse` local: make the guard mutable, pass current capture
state into mouse event gating, and provide a staged mouse transaction helper
that can be exercised with the fake controller.

- [ ] **Step 4: Run focused and full tests**

Run: `cargo test settings_mouse_`

Run: `cargo test --all-targets --all-features`

### Task 5: Extend App and Mode using TDD

**Files:**
- Modify: `src/picker.rs:749-1420`
- Modify: `src/picker_i18n.rs`

- [ ] **Step 1: Add failing state-machine tests**

Tests cover:

```rust
fn settings_mode_f2_opens_and_closes();
fn settings_mode_navigation_clamps_to_four_rows();
fn settings_mode_characters_do_not_modify_search();
fn settings_mode_unlocked_value_cycles_and_persists();
fn settings_mode_environment_locked_value_rejects_edits();
fn settings_mode_write_failure_preserves_effective_value();
```

- [ ] **Step 2: Run tests and verify RED**

Run: `cargo test settings_mode_`

Expected: named failures with at least six selected tests.

- [ ] **Step 3: Add Settings mode and handler**

Add `Mode::Settings { selected: usize }`, `handle_key_settings`, and F2 transitions. Use Up/Down for rows; Left/Right, Enter, and Space for changes; Esc/F2 closes. Setting edits happen only through App methods so render remains pure.

- [ ] **Step 4: Apply non-mouse settings after successful writes**

- Language: resolve Auto through locale, replace the preview worker, increment generation, clear preview cache, and invalidate selection.
- Preview: persist startup preference and set current visibility explicitly.
- Color: expose current effective color to `Theme` creation on each event-driven redraw.

Add a dedicated `settings_language_change_invalidates_old_preview` test that
asserts worker replacement, generation increment, cache clearing, and rejection
of a response carrying the pre-change generation.

- [ ] **Step 5: Run state-machine tests and baseline suite**

Run: `cargo test settings_mode_`

Run: `cargo test --all-targets --all-features`

- [ ] **Step 6: Reviewer checkpoint**

Review terminal-state ownership, settings transaction ordering, stale preview
generation, and absence of render-path IO before starting rendering work.

## Chunk 3: Rendering, Documentation, and Verification

### Task 6: Render the settings overlay

**Files:**
- Modify: `src/picker.rs:1558-2320`
- Modify: `src/picker_i18n.rs`

- [ ] **Step 1: Add failing render/copy tests**

Prefix tests `settings_panel_`. Cover bilingual labels, selected-row continuous
background, environment-lock marker, colorless mode, terminal-width truncation,
and tiny terminal safety.

- [ ] **Step 2: Run render tests and verify RED**

Run: `cargo test settings_panel_`

Expected: named failures with nonzero test selection.

- [ ] **Step 3: Implement `render_settings` and pure line helpers**

Use the existing centered overlay, border, `Theme::selected`, and display-width helpers. Four rows fit without scrolling. Add F2 to the Help panel and adaptive footer variants.

- [ ] **Step 4: Run render and full tests**

Run: `cargo test settings_panel_`

Run: `cargo test --all-targets --all-features`

### Task 7: Document configuration and verify behavior

**Files:**
- Modify: `README.md`

- [ ] **Step 1: Document F2, `tui.toml`, schema, and precedence**

Explain that environment-controlled rows are read-only and that Tab remains a session-only preview toggle.

- [ ] **Step 2: Run static verification**

Run: `cargo fmt --check`

Run: `cargo clippy --all-targets --all-features -- -D warnings`

Run: `cargo test --all-targets --all-features`

Run: `git diff --check`

Run: `cargo +1.74.0 check --locked --all-targets` (the toolchain is installed;
failure is blocking).

- [ ] **Step 3: Perform real PTY verification**

Use isolated HOME/XDG directories. Verify F2, all four settings, persistence across restart, environment locks, Chinese/English switching, colorless mode, mouse enable/disable, preview behavior, narrow terminal, and clean exit restoration.

- [ ] **Step 4: Build the manual-test binary**

Run: `cargo build --release`

Report the absolute binary path and `sha256sum target/release/cdh`.

- [ ] **Step 5: Final reviewer checkpoint without committing**

Run: `git status --short`, `git diff --stat`, and independent correctness and
architecture reviews focused on config safety, terminal restoration, Unicode
rendering, event-driven redraw, and compatibility. Do not commit or push.
