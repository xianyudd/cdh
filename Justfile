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

    # fish / zsh payload 用各自的解释器做语法检查。抓的是结构性错误（块不闭合、
    # 引号不配对、function/switch 缺 end）；命令名拼错这类错误它们一概放过。
    # 工具缺失时：CI 上硬失败（workflow 预装 fish 与 zsh），本地打印 SKIP —— 一个
    # 只会说 PASS 的检查等于没有检查，所以缺工具绝不能表现成通过。
    lint_payload() {
      local dir="$1"
      shift
      local tool="$1"
      local count f
      count=$(find "$dir" -type f | wc -l | tr -d '[:space:]')
      if [ "$count" -eq 0 ]; then
        echo "shell-lint: FAIL 在 $dir 下没找到任何 payload 文件（目录被改名或搬走了？）" >&2
        return 1
      fi
      if ! command -v "$tool" > /dev/null 2>&1; then
        if [ -n "${CI:-}" ]; then
          echo "shell-lint: FAIL 找不到 $tool，CI 上必须装" >&2
          return 1
        fi
        echo "shell-lint: SKIP $dir 下 $count 个文件未被检查（本机没装 $tool）" >&2
        return 0
      fi
      while IFS= read -r f; do
        "$@" "$f"
      done < <(find "$dir" -type f | sort)
      echo "shell-lint: OK   $tool 检查了 $dir 下 $count 个文件"
    }

    lint_payload scripts/installers/fish/payload fish --no-execute
    lint_payload scripts/installers/zsh/payload zsh -n

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
