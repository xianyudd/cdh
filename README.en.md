# cdh — frecency-driven directory jumping, with a TUI

[中文](./README.md) | **English**

`cdh` fuses several signals over your directory history — visit frequency with
time decay, recency, and the context of the directory you are in right now — and
gives you a terminal TUI that lists candidates by score so you can pick one and
jump.

> Install and uninstall integration is available for **fish / bash / zsh**.

* Repository: [https://github.com/xianyudd/cdh](https://github.com/xianyudd/cdh)
* Install script: [https://xianyudd.github.io/cdh/install.sh](https://xianyudd.github.io/cdh/install.sh)

> **This is a condensed English version.** The Chinese [`README.md`](./README.md)
> is the complete reference — full key and environment-variable tables, the
> logging format, the repository layout. Each section below links back to its
> Chinese counterpart for whatever is trimmed here.

---

## Install and uninstall

### One-liner (detects your current shell)

```bash
curl -fsSL https://xianyudd.github.io/cdh/install.sh | bash
```

> The short URL needs GitHub Pages enabled for the `/docs` directory of the `main`
> branch. Until then, use
> `https://raw.githubusercontent.com/xianyudd/cdh/main/scripts/install.sh`.

The installer picks fish / bash / zsh from `$SHELL`. Reload your shell afterwards:

```bash
exec fish -l   # fish
exec bash -l   # bash
exec zsh -l    # zsh
```

You can also name the shell yourself, or force the interactive chooser:

```bash
curl -fsSL https://xianyudd.github.io/cdh/install.sh | bash -s -- --shell zsh
curl -fsSL https://xianyudd.github.io/cdh/install.sh | bash -s -- --interactive
```

If the GitHub `latest` redirect is unreliable on your network, pin a version:

```bash
curl -fsSL https://xianyudd.github.io/cdh/install.sh \
  | CDH_VERSION=v0.2.8 bash
```

Or install from a release tarball, which helps when `raw.githubusercontent.com` is
flaky but release assets download fine:

```bash
curl -fsSL https://github.com/xianyudd/cdh/releases/download/v0.2.8/cdh-v0.2.8-x86_64-unknown-linux-gnu.tar.gz \
  | tar -xz
cd cdh-v0.2.8-x86_64-unknown-linux-gnu
bash --noprofile --norc install.sh
```

From a clone of this repository:

```bash
bash --noprofile --norc scripts/install.sh
```

### Uninstall

Remote — removes the shell integration, the binary and the history files:

```bash
curl -fsSL https://xianyudd.github.io/cdh/install.sh | bash -s -- --action uninstall
```

Local:

```bash
bash --noprofile --norc scripts/install.sh --action uninstall
```

---

## Usage

### Basics

Type this in your shell:

```bash
cdh
```

By default `cdh`:

* reads `history_raw` and `history_uniq` from the XDG history directory;
* scores and sorts the candidates (frequency + recency + context + recent-unique);
* opens a TUI list for you to choose from;
* hands your choice to the shell wrapper function, which `cd`s into it.

History lives under `DATA/history/`, where
`DATA = ${XDG_DATA_HOME:-$HOME/.local/share}/cdh`. Every shell records a visit
through a lightweight hook that calls `cdh log --dir "$PWD"`. The hook mechanism
per shell, the `history_raw` line format and the `STATE` directory are in
[日志采集 — logging](./README.md#日志采集).

### The TUI

A compact, keyboard-first picker: the first line shows the current result range and
page number, the second line takes an fzf-style fuzzy query, and only the current
page is rendered. Every row shows the full path with `$HOME` abbreviated to `~`,
the prefix dimmed and the last component bold. Directories that no longer exist are
flagged (`missing` in English, `失效` in Chinese) and can be deleted after a
confirmation. The interface language follows `LC_ALL`, `LC_MESSAGES` and `LANG`;
`CDH_LANG` overrides it.

The keys you will use most:

| Key | Action |
| --- | --- |
| any character | insert at the query cursor and re-filter |
| `↑` / `↓`, `Ctrl+P` / `Ctrl+N` | move up/down; crossing a page edge moves to the neighbouring page |
| `PageUp` / `PageDown`, `Ctrl+↑` / `Ctrl+↓` | previous / next page (page size follows terminal height) |
| `Enter` | jump to the selection; a missing directory offers deletion instead |
| `Tab` | toggle the preview pane for this TUI session only, without writing config |
| `Ctrl+U` | clear the query |
| `Ctrl+H` / `F5` | hide / show hidden directories (this session only, no config write) |
| `Ctrl+D` | exclude the selected directory and its subtree (press again to confirm) |
| `F1` | help overlay (`?` and fullwidth `？` work too) |
| `F2` | settings overlay |
| `F4` | exclusion list (select a row, `Ctrl+D` un-excludes it) |
| `Esc` | close the preview, then clear the query, then quit |
| `Ctrl+C` / `Ctrl+G` | quit |

`Home` / `End`, `←` / `→`, `Backspace` / `Delete` and the mouse (click to select,
double-click to jump, wheel to scroll) are bound as well — the complete table is in
[TUI 操作 — the TUI](./README.md#tui-操作).

The candidate pool is "history ∪ directory tree": besides the directories you have
`cd`'d into, a background thread streams a scan of the tree (siblings of history
entries, all of `$HOME`, and so on) so that directories you have never visited can
still be found by fuzzy search. Discovered entries rank after history entries with
the same fuzzy score. With an empty history the picker still opens and bootstraps
from `$PWD`.

`Ctrl+D` **excludes** rather than simply deletes: once confirmed, the directory *and
its entire subtree* go into the exclusion list, so it stops appearing among the
candidates and the scanner prunes it — the subtree is never `read_dir`'d again (a
history row also has its record removed from the history file). The list is
`$XDG_DATA_HOME/cdh/excludes` (default `~/.local/share/cdh/excludes`), one absolute
path per line, `#` for comments, safe to hand-edit. `F4` is the only way back,
because an excluded directory has no row left to press a key on. `Ctrl+H` / `F5` is
a different thing: a temporary view filter that hides candidates containing a hidden
path segment (`~/.cache/pip` and friends) for this session only and writes nothing.
Why both exist, and how a hidden segment is decided, is explained in
[TUI 操作 — the TUI](./README.md#tui-操作).

### Environment variables

The ones you are most likely to touch:

| Variable | Default | Meaning |
| --- | --- | --- |
| `CDH_LANG` | auto | TUI language; accepts `zh-CN` / `zh` and `en` / `en-US`; unrecognised values are ignored |
| `CDH_COLOR` | `1` | `0` turns colour off (the selected row falls back to reverse video) |
| `CDH_MOUSE` | `1` | `0` turns mouse capture off |
| `CDH_PREVIEW` | `0` | `1` opens the preview pane at startup; `Tab` still toggles it |
| `CDH_DISCOVER` | `1` | `0` uses history candidates only and never scans the directory tree |
| `CDH_IGNORE_RE` | unset | regex of paths to ignore, same as `--ignore-re` |
| `CDH_HALF_LIFE` | `604800` (7 days) | frecency half-life in seconds |

`CDH_CORNER_3D`, `CDH_SCAN_ROOTS`, `CDH_SCAN_DEPTH` and `CDH_ANIM` are documented in
[TUI 操作 — the TUI](./README.md#tui-操作); the scoring knobs
`CDH_RECENCY_HALF_LIFE`, `CDH_DEBOUNCE_SECS`, `CDH_UNIQ_DECAY` and the four weight
variables `CDH_W_FRECENCY` / `CDH_W_RECENCY` / `CDH_W_CONTEXT` / `CDH_W_UNIQ` are in
[排序算法 — scoring](./README.md#排序算法).

`F2` persists four TUI settings — language, preview at startup, colour and mouse
capture — to `$XDG_CONFIG_HOME/cdh/tui.toml`:

```toml
language = "auto" # auto, zh-CN or en
preview = false
color = true
mouse = true
```

Values resolve as **environment variable > config file > built-in default**, and a
setting whose variable is in effect is shown read-only in the overlay (`CDH_LANG`
counts only when its value is recognised). Toggling the preview with `Tab` never
rewrites `preview`. Writes go through a temporary file in the same directory plus an
atomic rename, guaranteed on Unix and WSL — native Windows filesystem semantics are
not promised.

### Command line

```bash
cdh -h
```

* `-v, --version` — print the version and exit;
* `-l, --limit <N>` — cap the number of candidates (no cap by default, so the TUI search covers the whole history; `CDH_LIMIT` does the same);
* `--half-life <sec>` — frecency half-life in seconds (defaults to `CDH_HALF_LIFE`, otherwise 7 days);
* `--threshold <f64>` — drop candidates scoring below this value (default `0`, i.e. disabled);
* `--ignore-re <re>` — regex of paths to ignore (defaults to `CDH_IGNORE_RE`, e.g. to skip `.git`);
* `--no-check-dir` — do not check that a directory still exists (handy when history is shared across machines).

Note that `cdh -h` prints its help text in Chinese; the list above is all of it.

Exit codes: `0` — a directory was selected and printed; `1` — cancelled (`Esc` /
`Ctrl+C`) or a TUI render error; `2` — no candidates available (empty history, or
everything filtered out).

Examples — the top 80 recommendations, and ignoring paths that contain `.git`:

```bash
cdh -l 80
CDH_IGNORE_RE='\.git($|/)' cdh
```

### Scoring in one paragraph

The score is a linear blend of four signals normalised to `[0,1]`: **frecency**
(time-decayed visit score from the raw log, compressed with `ln(1+s)` so that
high-traffic directories like `$HOME` do not flatten the rest, weight 0.40),
**recency** (a separate short half-life signal, 0.30), **context** (first-order
history transitions out of the current `pwd`, plus a small bonus for direct
children, 0.20) and **recent-unique** (geometrically decayed rank in the uniq file,
0.10). Two extra rules: repeat visits to the same directory within
`CDH_DEBOUNCE_SECS` (default 600 s) count once, cancelling the inflation from "every
new shell logs one `$HOME`" (`0` disables it); and `pwd` itself is never a candidate.
Per-signal table and weight variables: [排序算法 — scoring](./README.md#排序算法).

---

## Development

The tooling runs through [`just`](https://github.com/casey/just):

```bash
cargo install just
```

```bash
just fmt          # format Rust + shell scripts
just fmt-check    # check formatting
just shell-lint   # bash -n over the shell scripts
just lint         # cargo clippy
just lint-strict  # cargo clippy with warnings denied
just test         # cargo test --locked --all
just check        # fmt-check + shell-lint + lint + test
just build-release
just release-dry-run
just hooks-install # install the pre-commit / commit-msg hooks
```

`pre-commit` runs the formatting / shell-syntax / test checks before a commit and
`commit-msg` validates the message convention; both prefer `just` and fall back to
`cargo fmt --check` / `shfmt` / `bash -n` / `cargo test` when it is not installed.
More, including how to verify the hooks, is in
[开发者说明 — for developers](./README.md#开发者说明).

Known issues:

* [Shell Path Migration Issue](./docs/issues/2026-03-20-shell-path-migration.md)

The repository layout is annotated file by file — `scripts/installers/<shell>/`
with its `payload/` wrappers and logging hooks, and `src/controller.rs`,
`src/frecency.rs`, `src/recommend.rs`, `src/picker.rs` — in
[目录结构（节选） — layout](./README.md#目录结构节选).

---

## License

[MIT](./LICENSE)
