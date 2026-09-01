set shell := ["bash", "-euo", "pipefail", "-c"]

default:
    @just --list

fmt:
    cargo fmt
    just shellfmt

fmt-check:
    cargo fmt --check
    just shellfmt-check

# 下面三条 shell 配方都用 `while IFS= read -r` 收集文件列表，不要「简化」回 mapfile：
# macOS 自带的是 bash 3.2.57（Apple 因 GPLv3 停在这个版本），mapfile 是 bash 4.0 才有的
# builtin，换回去会让 macOS 的 CI job 在第一条 just fmt-check 就死在
# "mapfile: command not found"，一个测试都跑不到，Mac 开发者本地也跑不了 just check。
# 同理 docs/install.sh 放在数组初值里而不是后面单独传参：这样数组恒非空，
# 避开 bash 3.2 在 set -u 下展开空数组直接报 unbound variable 的行为。
shellfmt:
    #!/usr/bin/env bash
    set -euo pipefail
    files=("docs/install.sh")
    while IFS= read -r f; do files+=("$f"); done < <(find scripts -type f -name '*.sh' | sort)
    shfmt -w -i 2 -ci -bn -sr "${files[@]}"

shellfmt-check:
    #!/usr/bin/env bash
    set -euo pipefail
    # while read 而非 mapfile：见 shellfmt 上方注释（macOS 是 bash 3.2）
    files=("docs/install.sh")
    while IFS= read -r f; do files+=("$f"); done < <(find scripts -type f -name '*.sh' | sort)
    shfmt -d -i 2 -ci -bn -sr "${files[@]}"

shell-lint:
    #!/usr/bin/env bash
    set -euo pipefail
    # while read 而非 mapfile：见 shellfmt 上方注释（macOS 是 bash 3.2）
    files=("docs/install.sh")
    while IFS= read -r f; do files+=("$f"); done < <(find scripts -type f -name '*.sh' | sort)
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
