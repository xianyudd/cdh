set shell := ["bash", "-euo", "pipefail", "-c"]

default:
    @just --list

fmt:
    cargo fmt
    just shellfmt

fmt-check:
    cargo fmt --check
    just shellfmt-check

shellfmt:
    #!/usr/bin/env bash
    set -euo pipefail
    mapfile -t files < <(find scripts -type f -name '*.sh' | sort)
    shfmt -w -i 2 -ci -bn -sr docs/install.sh "${files[@]}"

shellfmt-check:
    #!/usr/bin/env bash
    set -euo pipefail
    mapfile -t files < <(find scripts -type f -name '*.sh' | sort)
    shfmt -d -i 2 -ci -bn -sr docs/install.sh "${files[@]}"

shell-lint:
    #!/usr/bin/env bash
    set -euo pipefail
    mapfile -t files < <(find scripts -type f -name '*.sh' | sort)
    files+=("docs/install.sh")
    for f in "${files[@]}"; do
      bash -n "$f"
    done

test:
    cargo test --locked --all

lint:
    cargo clippy --all-targets --all-features

lint-strict:
    cargo clippy --all-targets --all-features -- -D warnings

check:
    just fmt-check
    just shell-lint
    just lint
    just test

build:
    cargo build

build-release:
    cargo build --release --locked

install-local:
    bash --noprofile --norc scripts/install.sh

hooks-install:
    bash scripts/tools/install-hooks.sh

release-dry-run:
    just check
    just build-release
