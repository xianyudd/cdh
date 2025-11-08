#!/usr/bin/env bash
# scripts/install.sh
# 简单交互：必须选择 shell；未实现则提示；已实现则下载子安装脚本到临时目录后本地执行；最后清理临时资源
set -Eeuo pipefail

# 避免 locale 警告
unset LC_ALL || true
unset LANG   || true

OWNER="xianyudd"
REPO="cdh"
BRANCH="main"
RAW_BASE="https://raw.githubusercontent.com/${OWNER}/${REPO}/${BRANCH}/scripts"

# ---------- 阶段目录（退出时统一清理） ----------
STAGE_DIR="$(mktemp -d "${TMPDIR:-/tmp}/cdh-stage.XXXXXX")"
_cleanup() { rm -rf "${STAGE_DIR}" || true; }
trap _cleanup EXIT

# ---------- 简单下载器 ----------
_fetch() {
  local url="$1" out="$2"
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL --retry 1 --connect-timeout 2 -o "$out" "$url"
  elif command -v wget >/dev/null 2>&1; then
    wget -q -O "$out" "$url"
  else
    echo "[cdh] 需要 curl 或 wget 以下载远端脚本。" >&2
    return 127
  fi
}

# ---------- 列出检测到的 shell ----------
_list_installed_shells() {
  local out=()
  command -v fish >/dev/null 2>&1 && out+=("fish")
  command -v zsh  >/dev/null 2>&1 && out+=("zsh")
  command -v bash >/dev/null 2>&1 && out+=("bash")
  printf "%s\n" "${out[@]}"
}

# ---------- 选择 shell（有 TTY 必须明确选择；无 TTY 默认 fish） ----------
_choose_shell() {
  local installed; installed="$(_list_installed_shells | tr '\n' ' ')"
  local def="fish"  # 必选策略：无 TTY 时自动选择 fish
  if [ ! -t 0 ]; then
    echo "[cdh] 非交互环境，自动选择: ${def}" >&2
    echo "${def}"
    return 0
  fi

  # 交互：必须选择一个有效项或 q 退出（空回车不接受）
  while :; do
    echo "[cdh] 检测到 shell: ${installed:-<none>}"
    echo "[cdh] 请选择要安装到的 shell："
    local idx=1
    for s in ${installed}; do
      if [[ "$s" == "fish" ]]; then
        printf "  %d) %s\n" "$idx" "$s"
      else
        printf "  %d) %s  （未实现安装器）\n" "$idx" "$s"
      fi
      idx=$((idx+1))
    done
    echo "  q) 退出"
    printf "[cdh] 输入序号或名称："
    local ans=""
    # 始终从 /dev/tty 读取，避免被管道占用
    if [ -r /dev/tty ]; then
      # shellcheck disable=SC2162
      read -r ans < /dev/tty || true
    else
      read -r ans || true
    fi
    case "${ans}" in
      q|Q) echo ""; return 0 ;;
      1)   echo ${installed} | awk '{print $1}'; return 0 ;;
      2)   echo ${installed} | awk '{print $2}'; return 0 ;;
      3)   echo ${installed} | awk '{print $3}'; return 0 ;;
      fish|zsh|bash) echo "${ans}"; return 0 ;;
      *)   echo "[cdh] 无效输入，请重新选择。" >&2 ;;
    esac
  done
}

# ---------- 运行指定 shell 的安装器（下载到临时目录后本地执行） ----------
_run_installer_staged() {
  local sh="$1" dst="${STAGE_DIR}/${sh}-install.sh"
  local url="${RAW_BASE}/installers/${sh}/install.sh"
  echo "[cdh] 下载 ${sh} 安装脚本 ..."
  _fetch "$url" "$dst"
  chmod +x "$dst"
  echo "[cdh] 执行安装 ..."
  # 把 STAGE_DIR 透传给子脚本，使其依赖文件也落到同一临时目录；由父脚本统一清理
  STAGE_DIR="${STAGE_DIR}" env -u LC_ALL -u LANG bash "$dst"
}

# ================= 主流程 =================
SEL_SHELL="$(_choose_shell)"
[[ -z "${SEL_SHELL}" ]] && { echo "[cdh] 已取消。"; exit 0; }

if [[ "${SEL_SHELL}" != "fish" ]]; then
  echo "[cdh] ${SEL_SHELL} 的安装器暂未实现。" >&2
  exit 10
fi

_run_installer_staged "fish"

echo "[cdh] 安装完成。请执行：exec fish -l"
