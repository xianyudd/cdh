# TUI Settings Design

## Goal

Add a keyboard-first, persistent settings panel to the existing picker without
changing recommendation behavior, shell jump semantics, pagination, search, or
the asynchronous preview architecture.

The first version manages four TUI preferences:

- language: automatic locale detection, Simplified Chinese, or English;
- preview visibility at startup;
- color output;
- mouse capture.

## User Interface

`F2` opens a centered settings panel over the existing directory list. The list
remains visible as context, following the established Help and ConfirmDelete
overlay pattern.

Controls:

- `Up` / `Down`: select a setting;
- `Left` / `Right`, `Enter`, or `Space`: change the selected value;
- `Esc` or `F2`: close the panel.

Normal characters do not enter the search query while Settings mode is active.
The footer, Help panel, and README expose `F2` as the settings shortcut.

The panel contains four rows:

```text
Settings

> Language             Automatic  < >
  Preview on startup   Off
  Color                On
  Mouse capture        On

Up/Down Select - Left/Right Change - Esc Done
```

The selected row uses one continuous background. Boolean values use concise
On/Off copy. A setting controlled by an environment variable is marked as such
and is read-only, so the panel never pretends that a lower-precedence value will
win on the next launch.

The panel is bilingual and shrinks safely in narrow terminals using terminal
display width rather than UTF-8 byte length.

## State Model

Extend the picker state machine with:

```rust
Mode::Settings { selected: usize }
```

Settings receives its own key handler and renderer. It does not add conditions
to the Normal or Help handlers beyond the `F2` transition.

The picker owns the effective UI settings in memory. Drawing only reads these
values. File loading happens inside `run_ui`, after `pick` / `pick_with_history`
have passed their TTY checks and before the event loop starts. Non-interactive
fallbacks therefore do not read the settings file, emit settings notices, or
change their existing first-result behavior. File writes happen only in
response to a settings edit.

## Persistence

Add a focused `src/tui_settings.rs` module. The file is:

```text
$XDG_CONFIG_HOME/cdh/tui.toml
```

or `~/.config/cdh/tui.toml` when `XDG_CONFIG_HOME` is unset.

The initial schema is:

```toml
language = "auto"
preview = false
color = true
mouse = true
```

Use `toml = "=0.8.23"` rather than ad hoc string parsing. This release declares
Rust 1.66 support and therefore remains compatible with the project's Rust 1.74
MSRV. `Cargo.lock` pins its transitive dependency set. Missing files and missing
keys use built-in defaults. Unknown keys are ignored. A known key with an
invalid type or value falls back independently and produces a non-fatal notice.
A syntactically invalid file falls back to defaults and produces a non-fatal
notice.

Writes serialize all four known values to a uniquely named temporary file in
the same directory, opened with `create_new`, and then replace `tui.toml`.
Unique names include the process ID and a per-process monotonic counter so two
TUI sessions never share a candidate file. A failed write leaves the previous
effective value in place and shows a localized footer error; failed candidates
are removed on a best-effort basis.

Concurrent settings sessions use last-successful-writer-wins semantics. The
file contains one small preference object and settings edits are rare, so a new
cross-process locking subsystem is not justified for this version.

## Precedence

The precedence for each preference is:

```text
CDH_* environment variable > tui.toml > built-in default
```

The mappings are:

- `CDH_LANG` -> language;
- `CDH_PREVIEW` -> preview;
- `CDH_COLOR` -> color;
- `CDH_MOUSE` -> mouse.

Only the corresponding row is locked. Other settings remain editable. Boolean
environment variables preserve the existing `env_flag` behavior: `1` and
case-insensitive `true` mean enabled; any other present value means disabled and
locks that row. `CDH_LANG` locks the language row only when it contains a tag
already recognized by the picker (`zh`, `zh-*`, `en`, `en-*`, `C`, or `POSIX`,
including existing underscore/encoding normalization). An unrecognized
`CDH_LANG` remains ignored and does not lock the row. This preserves existing
environment-variable behavior while preventing an invalid language value from
making the UI falsely read-only.

## Runtime Effects

- Language changes update all visible copy immediately. Automatic language uses
  the existing locale resolution. The preview worker and localized error cache
  are replaced so responses from the old language generation cannot overwrite
  the new UI.
- Preview changes persist the startup preference and immediately update the
  current panel visibility using the existing preview worker/cache path.
- Color changes rebuild the in-memory theme for the next event-driven redraw.
- Mouse changes update crossterm capture immediately. `TermGuard` becomes a
  mutable terminal-state controller with `set_mouse_capture(bool)`: it updates
  its stored state only after crossterm succeeds, `Drop` disables capture based
  on that current state, and the event loop gates mouse events using the same
  current state instead of an immutable startup flag.

For language, preview, and color, an edit first atomically writes a candidate
settings value and applies it to the running app only after persistence
succeeds. Mouse has an external terminal side effect, so it first asks the
mutable `TermGuard` to transition, then persists the candidate value. A failed
terminal transition changes neither memory nor disk. A failed persistence
attempt requests a terminal rollback before restoring the in-memory value. If
that rollback also fails, `TermGuard` retains the actual capture state so exit
cleanup is still correct, and the footer reports that the mouse change is only
effective for the current session. This explicitly avoids claiming atomicity
across the filesystem and terminal driver.

No setting introduces a frame clock or animation.

## Error Handling

- Configuration read and parse errors never panic or exit the picker.
- Configuration write errors keep the previous value and remain visible in the
  footer after the panel closes, except for the documented session-only mouse
  state when both persistence and terminal rollback fail.
- Environment-controlled rows do not write the configuration file.
- Terminal mouse-control failures preserve the prior capture state.
- Terminal restoration still runs on every exit and error path through
  `TermGuard`.

## Module Changes

- `Cargo.toml` / `Cargo.lock`: pin `toml` 0.8.23 for Rust 1.74 compatibility.
- `src/tui_settings.rs`: nested as `picker::settings` via an explicit `#[path]`
  declaration in `picker.rs`; owns schema, load, precedence, lock metadata, and
  atomic persistence without expanding the public library API.
- `src/picker.rs`: Settings mode, handlers, runtime application, rendering,
  terminal mouse synchronization, and tests.
- `src/picker_i18n.rs`: settings labels, values, hints, and errors in Chinese
  and English.
- `README.md`: F2 shortcut, settings behavior, path, and precedence.

## Testing

Unit tests cover:

- defaults and TOML round trips;
- missing, malformed, partially invalid, and unknown values;
- per-setting environment precedence and lock metadata;
- invalid `CDH_LANG` and non-true boolean environment values;
- failed writes preserving the previous effective value;
- Settings mode opening, closing, navigation, cycling, and search isolation;
- locked rows rejecting edits;
- immediate language, preview, and color updates;
- stale preview responses after a language change;
- bilingual settings copy and Unicode-safe narrow rendering;
- mouse state transition, event gating, rollback, and `TermGuard` cleanup after
  capture is enabled at runtime;
- no settings reads, writes, or notices during non-interactive fallback.

Final verification runs rustfmt, Clippy with warnings denied, all tests,
`git diff --check`, a release build, `cargo +1.74.0 check --locked --all-targets`
when the Rust 1.74 toolchain is available, and a real PTY session for the four
setting changes and terminal restoration. Dependency metadata must continue to
declare an MSRV no newer than Rust 1.74 even when that toolchain is unavailable
locally.

## Non-Goals

- Recommendation weights or history management settings;
- custom keybindings;
- editing shell startup files;
- a general-purpose configuration editor;
- settings scrolling or nested pages;
- synchronization across concurrently open settings panels.
