#!/usr/bin/env bash
# scripts/tools/git-add-guard.sh
# 用法（推荐作为别名而非覆盖内置 add）：
#   git config alias.gadd '!scripts/tools/git-add-guard.sh'
#   git gadd . && gdiffc
set -Eeuo pipefail

unset LC_ALL || true
unset LANG || true

REPO_ROOT="$(git rev-parse --show-toplevel 2> /dev/null || pwd)"
cd "$REPO_ROOT"

# 解析参数：提取 pathspec，识别 -A/--all，其它选项原样透传
PATHS=()
ALL_FLAG=0
ARGS=("$@")
i=0
while [[ $i -lt $# ]]; do
  a="${ARGS[$i]}"
  case "$a" in
    -A | --all) ALL_FLAG=1 ;;
    --)
      ((i++))
      while [[ $i -lt $# ]]; do
        PATHS+=("${ARGS[$i]}")
        ((i++))
      done
      break
      ;;
    -*) ;; # 其它选项忽略，由最终 git add 处理
    *) PATHS+=("$a") ;;
  esac
  ((i++))
done

# 若 -A 或未给 pathspec，则用当前目录
if ((ALL_FLAG)) || ((${#PATHS[@]} == 0)); then
  PATHS=(".")
fi

# 在给定 pathspec 范围内取“已修改 + 未跟踪”，再过滤为 *.sh（交集）
mapfile -t SH_FILES < <(
  {
    git ls-files -m -- "${PATHS[@]}" || true
    git ls-files -o --exclude-standard -- "${PATHS[@]}" || true
  } | grep -E '\.sh$' | sort -u
)

# 没有 .sh
if ((${#SH_FILES[@]} == 0)); then
  if ((${#ARGS[@]} == 0)); then
    echo "[add-guard] 无需处理：没有变更中的 *.sh，且未指定 pathspec。"
    exit 0
  else
    exec git add "$@"
  fi
fi

echo "[add-guard] 检测到 shell 脚本：${#SH_FILES[@]} 个"

# --- 格式化（可自定义）---
# 若设置 SHFMT_OPTS 则使用之；否则采用安全默认：-i 2 -ci -bn -sr
if command -v shfmt > /dev/null 2>&1; then
  if [[ -n "${SHFMT_OPTS:-}" ]]; then
    # shellcheck disable=SC2086
    shfmt ${SHFMT_OPTS} -w -- "${SH_FILES[@]}"
  else
    shfmt -w -i 2 -ci -bn -sr -- "${SH_FILES[@]}"
  fi
else
  if [[ "${REQUIRE_SHFMT:-0}" == "1" ]]; then
    echo "[add-guard] 缺少 shfmt，且已设置 REQUIRE_SHFMT=1，终止。" >&2
    exit 127
  else
    echo "[add-guard] 未找到 shfmt，跳过格式化（可设置 REQUIRE_SHFMT=1 强制依赖）。" >&2
  fi
fi

# --- 语法检查 ---
FAILS=()
for f in "${SH_FILES[@]}"; do
  if ! bash -n "$f" 2> /dev/null; then
    FAILS+=("$f")
  fi
done

if ((${#FAILS[@]})); then
  echo "[add-guard] 语法检查失败的文件：" >&2
  printf ' - %s\n' "${FAILS[@]}" >&2
  echo "[add-guard] 已阻止 git add。" >&2
  exit 1
fi

# 全部通过 -> 根据是否传参选择 add 策略
if ((${#ARGS[@]} == 0)); then
  git add -- "${SH_FILES[@]}"
  echo "[add-guard] 已添加到暂存区："
  printf '  - %s\n' "${SH_FILES[@]}"
  exit 0
else
  exec git add "$@"
fi
