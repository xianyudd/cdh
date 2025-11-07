#!/usr/bin/env bash
# scripts/tools/git-add-guard.sh
# 用法：git add ...   （通过 git alias 覆盖内置 add）
set -Eeuo pipefail

REPO_ROOT="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
cd "$REPO_ROOT"

# 解析参数：提取 pathspec，识别 -A/--all，其它选项原样透传
PATHS=()
ALL_FLAG=0
ARGS=("$@")
i=0
while [[ $i -lt $# ]]; do
  a="${ARGS[$i]}"
  case "$a" in
    -A|--all) ALL_FLAG=1 ;;
    --) ((i++)); while [[ $i -lt $# ]]; do PATHS+=("${ARGS[$i]}"); ((i++)); done; break ;;
    -*) ;;   # 其它选项忽略，由最终 git add 处理
    *) PATHS+=("$a") ;;
  esac
  ((i++))
done

# 需要检查的 *.sh（已跟踪修改 + 未跟踪）
if (( ALL_FLAG )) || ((${#PATHS[@]}==0)); then
  PATHS=(".")
fi
mapfile -t SH_FILES < <(
  git ls-files -m -- "${PATHS[@]}" "*.sh"
  git ls-files -o --exclude-standard -- "${PATHS[@]}" "*.sh"
)

# 没有 .sh，直接转交给真正的 git add
if ((${#SH_FILES[@]}==0)); then
  exec git add "$@"
fi

echo "[add-guard] 检测到 shell 脚本：${#SH_FILES[@]} 个"

# --- 格式化（可自定义）---
# 自定义：导出 SHFMT_OPTS（默认：2空格、case 体缩进、换行二元操作符、重定向右结合）
SHFMT_OPTS="${SHFMT_OPTS:- -i 2 -ci -bn -sr }"
if command -v shfmt >/dev/null 2>&1; then
  shfmt ${SHFMT_OPTS} -w "${SH_FILES[@]}"
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
  if ! bash -n "$f"; then
    FAILS+=("$f")
  fi
done

if ((${#FAILS[@]})); then
  echo "[add-guard] 语法检查失败的文件：" >&2
  printf ' - %s\n' "${FAILS[@]}" >&2
  echo "[add-guard] 已阻止 git add。" >&2
  exit 1
fi

# 全部通过 -> 交给真正的 git add
exec git add "$@"
