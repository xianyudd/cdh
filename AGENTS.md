# Repository Guidelines

## Project Structure & Module Organization

`cdh` is a Rust CLI/TUI directory jumper. Source code lives in `src/`. The
binary entrypoint is `src/main.rs`, and reusable modules are exported from
`src/lib.rs`. Core modules include `controller.rs` for CLI flow, `history.rs`
for local history files, `recommend.rs` and `frecency.rs` for ranking, and
`picker.rs` for the crossterm UI. Shell installers and payloads live under
`scripts/installers/{fish,bash,zsh}/`. Release notes and helper tools are in
`scripts/release_notes/` and `scripts/tools/`. Tests are inline `#[cfg(test)]`
modules inside Rust source files.

## Build, Test, and Development Commands

- `cargo build`: compile the debug binary.
- `cargo build --release`: build the optimized binary.
- `cargo run --`: run the CLI locally.
- `cargo run -- log --dir "$PWD"`: test history logging.
- `cargo test`: run unit tests.
- `cargo test --locked --all`: match the release workflow.
- `cargo fmt --check`: verify formatting.
- `cargo clippy --all-targets --all-features`: run Rust lints.
- `bash --noprofile --norc scripts/install.sh`: test the installer.

## Coding Style & Naming Conventions

Use standard Rust formatting via `rustfmt`. Prefer small, focused functions and
clear module responsibilities. Use snake_case for functions, variables, modules,
and tests. Public structs and types use UpperCamelCase, for example
`AppContext`, `RecommendOpt`, and `FrecencyIndex`. Shell scripts should remain
bash/fish/zsh appropriate and pass `bash -n` where applicable.
`scripts/tools/git-add-guard.sh` can format changed shell scripts with `shfmt`.

## Testing Guidelines

Rust tests use the built-in test framework. Place focused unit tests near the
module they cover with `#[cfg(test)]`. Name tests by behavior, such as
`log_visit_writes_raw_and_uniq`. Coverage is strongest for frecency, history,
and recommendation logic; changes to `controller`, `picker`, or shell payloads
should add tests or a manual verification plan. Always run `cargo test`.

## Commit & Pull Request Guidelines

Commit messages follow:

```text
<type>(<scope>): <summary>
```

Allowed types include `feat`, `fix`, `docs`, `refactor`, `test`, `chore`, `ci`,
and `perf`. Preferred scopes include `install`, `bash`, `fish`, `zsh`,
`history`, `paths`, `recommend`, `controller`, `readme`, `release`, `ci`, and
`tui`. Keep commits focused with concise, action-oriented summaries.

Pull requests should explain the user-facing change, list verification commands,
and call out shell-specific behavior when touching installer or payload files.
Link relevant docs or issues, such as `docs/issues/`.

## Security & Configuration Tips

Runtime state is local file-based: XDG data and state directories contain
`history_raw`, `history_uniq`, and the lock file. Do not hardcode user paths or
secrets. Respect `CDH_LIMIT`, `CDH_IGNORE_RE`, `CDH_CHECK_DIR`, `CDH_BIN`, and
XDG variables. Be careful with uninstall logic because it can remove history.
