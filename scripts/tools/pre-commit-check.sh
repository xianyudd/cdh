#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(git rev-parse --show-toplevel 2> /dev/null || pwd)"
cd "$REPO_ROOT"

if command -v just > /dev/null 2>&1; then
  just fmt-check
  just shell-lint
  cargo test --locked --all
else
  cargo fmt --check
  shfmt -d -i 2 -ci -bn -sr docs/install.sh $(find scripts -type f -name '*.sh' | sort)
  for f in $(find scripts -type f -name '*.sh' | sort) docs/install.sh; do
    bash -n "$f"
  done
  cargo test --locked --all
fi
