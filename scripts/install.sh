#!/usr/bin/env bash
set -euo pipefail

# =====================================
# cdh 一键安装/卸载脚本
# 支持 Linux / macOS；集成 fish / bash / zsh
# - TUI 全走 stderr；stdout 仅输出选中目录
# =====================================

REPO="xianyudd/cdh"
APP="cdh"

# ---------- 参数 & 环境 ----------
PREFIX_DEFAULT="${HOME}/.local"
BIN_DIR_DEFAULT="${PREFIX_DEFAULT}/bin"

CDH_VERSION="${CDH_VERSION:-}"               # vX.Y.Z，空则取最新
CDH_BIN_DIR="${CDH_BIN_DIR:-$BIN_DIR_DEFAULT}"
CDH_SHELL="${CDH_SHELL:-auto}"               # auto|fish|bash|zsh|none
CDH_INSTALL_LOGGER="${CDH_INSTALL_LOGGER:-auto}"  # auto|none
CDH_YES="${CDH_YES:-0}"                      # 1=不询问
CDH_FORCE="${CDH_FORCE:-0}"                  # 1=覆盖 fish 的 cdh 函数
DO_UNINSTALL=0

# 简单参数解析（可被 env 覆盖）
while [[ $# -gt 0 ]]; do
  case "$1" in
    --version) CDH_VERSION="$2"; shift 2 ;;
    --bin-dir) CDH_BIN_DIR="$2"; shift 2 ;;
    --shell)   CDH_SHELL="$2"; shift 2 ;;
    --logger)  CDH_INSTALL_LOGGER="$2"; shift 2 ;;
    -y|--yes)  CDH_YES=1; shift ;;
    --force)   CDH_FORCE=1; shift ;;
    --uninstall) DO_UNINSTALL=1; shift ;;
    *) echo "未知参数：$1" >&2; exit 2 ;;
  esac
done

# ---------- 彩色输出 ----------
color() { printf "\033[%sm%s\033[0m\n" "$1" "$2"; }
info()  { color "36" "==> $*"; }
ok()    { color "32" "✔ $*"; }
warn()  { color "33" "⚠ $*"; }
err()   { color "31" "✘ $*" >&2; }

need_cmd() { command -v "$1" >/dev/null 2>&1 || { err "缺少依赖：$1"; exit 1; }; }

confirm() {
  [[ "$CDH_YES" = "1" ]] && return 0
  read -r -p "$* [y/N]: " ans
  [[ "${ans:-N}" =~ ^[Yy]$ ]]
}

detect_shell() {
  local s
  s="$(ps -p $$ -o comm= | head -n1 | xargs basename 2>/dev/null || true)"
  [[ -z "$s" || "$s" = "sh" ]] && s="$(basename "${SHELL:-sh}")"
  echo "$s"
}

OS="$(uname -s)"
ARCH="$(uname -m)"
SHELL_BASENAME="$(detect_shell)"

detect_target() {
  case "$OS" in
    Linux)  os_tag=linux ;;
    Darwin) os_tag=darwin ;;
    *) err "不支持的系统：$OS"; exit 1 ;;
  esac
  case "$ARCH" in
    x86_64|amd64) arch_tag=x86_64 ;;
    arm64|aarch64) arch_tag=aarch64 ;;
    *) err "不支持的架构：$ARCH"; exit 1 ;;
  esac
  case "${os_tag}-${arch_tag}" in
    linux-x86_64)   TARGET="x86_64-unknown-linux-gnu" ;;
    darwin-x86_64)  TARGET="x86_64-apple-darwin" ;;
    darwin-aarch64) TARGET="aarch64-apple-darwin" ;;
  esac
}

ensure_bindir() {
  mkdir -p "$CDH_BIN_DIR"
  case ":$PATH:" in *":$CDH_BIN_DIR:"*) ;; *)
    warn "PATH 中没有 $CDH_BIN_DIR，将尝试写入 rc"
    case "$SHELL_BASENAME" in
      fish)
        if command -v fish >/dev/null 2>&1; then
          fish -lc "set -Ux fish_user_paths $CDH_BIN_DIR \$fish_user_paths" || true
          ok "已为 fish 加 PATH：$CDH_BIN_DIR"
        fi
        ;;
      zsh)
        echo "export PATH=\"$CDH_BIN_DIR:\$PATH\"" >> "$HOME/.zshrc"
        ok "已写入 ~/.zshrc：PATH+=${CDH_BIN_DIR}"
        ;;
      bash|sh|*)
        echo "export PATH=\"$CDH_BIN_DIR:\$PATH\"" >> "$HOME/.bashrc"
        ok "已写入 ~/.bashrc：PATH+=${CDH_BIN_DIR}"
        ;;
    esac
  esac
}

get_latest_asset_url() {
  need_cmd curl
  local version="$CDH_VERSION"
  if [[ -z "$version" ]]; then
    info "查询最新版本…"
    # 若有 GH_TOKEN 则带上以避免 rate limit
    local auth=()
    [[ -n "${GH_TOKEN:-}" ]] && auth=(-H "Authorization: Bearer $GH_TOKEN")
    local json
    json="$(curl -fsSL "${auth[@]}" "https://api.github.com/repos/${REPO}/releases/latest")"
    version="$(printf '%s' "$json" | sed -n 's/ *"tag_name": *"\(v[^"]*\)".*/\1/p' | head -n1)"
    [[ -n "$version" ]] || { err "无法获取最新版本 tag"; exit 1; }
  fi
  VERSION="$version"
  ASSET_NAME="${APP}-${VERSION}-${TARGET}.tar.gz"
  local json2
  json2="$(curl -fsSL ${auth:+${auth[*]}} "https://api.github.com/repos/${REPO}/releases/tags/${VERSION}")" \
    || { err "获取 $VERSION 版本信息失败"; exit 1; }
  ASSET_URL="$(printf '%s' "$json2" | sed -n "s# *\"browser_download_url\": *\"\\(.*${ASSET_NAME}\\)\"#\\1#p" | head -n1)"
  [[ -n "${ASSET_URL:-}" ]] || { err "未找到资产：${ASSET_NAME}"; exit 1; }
}

install_binary() {
  need_cmd tar
  detect_target
  get_latest_asset_url

  info "下载 ${ASSET_NAME}"
  local tmpdir; tmpdir="$(mktemp -d)"
  trap 'rm -rf "$tmpdir"' EXIT
  curl -fL "$ASSET_URL" -o "$tmpdir/$ASSET_NAME"

  info "解压到临时目录"
  tar -C "$tmpdir" -xzf "$tmpdir/$ASSET_NAME"

  local bin_path
  bin_path="$(find "$tmpdir" -type f -name "${APP}" -perm -111 | head -n1 || true)"
  [[ -n "$bin_path" ]] || { err "未在压缩包中找到可执行文件 ${APP}"; exit 1; }

  info "安装到 ${CDH_BIN_DIR}/${APP}"
  install -m 0755 "$bin_path" "${CDH_BIN_DIR}/${APP}"
  ok "二进制安装完成：$(command -v ${APP} || echo ${CDH_BIN_DIR}/${APP})"
}

# ---------------- Shell 集成 ----------------
install_fish_wrapper() {
  local funcdir="${HOME}/.config/fish/functions"
  mkdir -p "$funcdir"

  # 冲突检测：已有 cdh 函数
  if fish -c "functions -q cdh" >/dev/null 2>&1; then
    warn "检测到 fish 已存在函数 cdh（可能与 Rust 版重名）"
    if [[ "$CDH_FORCE" = "1" ]] || confirm "覆盖现有 cdh 函数以使用 Rust 版？"; then
      fish -c "functions --erase cdh" || true
    else
      warn "跳过覆盖 cdh 函数。你仍可直接运行外部二进制：$(command -v cdh || echo ${CDH_BIN_DIR}/cdh)"
      return 0
    fi
  fi

  # 包装函数：TUI 在 stderr；stdout 捕获目录后 cd
  cat > "${funcdir}/cdh.fish" <<'FISH'
function cdh -d "cd via Rust cdh (TUI)"
    set -l bin (command -v cdh)
    if not test -x "$bin"
        echo "cdh: not found" >&2
        return 127
    end
    if contains -- '--help' $argv; or contains -- '--version' $argv
        command cdh $argv
        return $status
    end
    set -l sel (command $bin $argv)
    set -l st $status
    if test $st -eq 0 -a -n "$sel"
        builtin cd -- "$sel"
    else
        return $st
    end
end
FISH
  ok "已安装 fish 函数：cdh（自动 cd）"
}

install_fish_logger() {
  [[ "$CDH_INSTALL_LOGGER" = "none" ]] && { warn "跳过目录日志（CDH_INSTALL_LOGGER=none）"; return 0; }
  local funcdir="${HOME}/.config/fish/functions"
  mkdir -p "$funcdir"
  # 轻量日志（不依赖外部命令；去抖）
  cat > "${funcdir}/cd.fish" <<'FISH'
functions --erase cd 2>/dev/null
function cd --wraps=cd -d "cd + log to ~/.cd_history(_raw)"
    builtin cd -- $argv; or return
    set -l now (date +%s)
    set -l raw ~/.cd_history_raw
    set -l uniq ~/.cd_history
    test -e $raw; or touch $raw
    test -e $uniq; or touch $uniq
    if test "$__CDH_LAST_DIR" = (pwd) -a (math "$now - $__CDH_LAST_TS" 2>/dev/null) -lt 2
        return
    end
    printf "%s\t%s\n" $now (pwd) >> $raw
    printf "%s\n" (pwd) >> $uniq
    set -g __CDH_LAST_DIR (pwd)
    set -g __CDH_LAST_TS $now
end
FISH
  ok "已安装 fish 目录日志（~/.cd_history_raw / ~/.cd_history）"
}

install_zsh_wrapper_and_logger() {
  local rc="$HOME/.zshrc"
  if ! grep -q "# >>> cdh (installer marker)" "$rc" 2>/dev/null; then
    cat >> "$rc" <<'ZSH'

# >>> cdh (installer marker)
__cdh_run() {
  local bin sel
  bin="$(command -v cdh)" || { echo "cdh: not found" >&2; return 127; }
  sel="$("$bin" "$@" 2>/dev/tty)"
  [ -n "$sel" ] && builtin cd -- "$sel"
}
alias cdh="__cdh_run"

# 日志：使用 chpwd 钩子（不覆写 cd）
__cdh_log() {
  local now raw uniq
  now="$(date +%s)"
  raw="$HOME/.cd_history_raw"
  uniq="$HOME/.cd_history"
  touch "$raw" "$uniq"
  # 简单去抖：2 秒
  if [ "$PWD" = "${__CDH_LAST_DIR:-}" ] && [ $(( now - ${__CDH_LAST_TS:-0} )) -lt 2 ]; then
    return
  fi
  printf "%s\t%s\n" "$now" "$PWD" >> "$raw"
  printf "%s\n" "$PWD" >> "$uniq"
  __CDH_LAST_DIR="$PWD"; __CDH_LAST_TS="$now"
}
typeset -ga chpwd_functions
chpwd_functions+=(__cdh_log)
# <<< cdh (installer marker)
ZSH
    ok "已写入 ~/.zshrc：cdh 包装 + chpwd 日志"
  else
    warn "~/.zshrc 已包含 cdh 标记，跳过重复写入"
  fi
}

install_bash_wrapper_and_logger() {
  local rc="$HOME/.bashrc"
  if ! grep -q "# >>> cdh (installer marker)" "$rc" 2>/dev/null; then
    cat >> "$rc" <<'BASH'

# >>> cdh (installer marker)
__cdh_run() {
  local bin sel
  bin="$(command -v cdh)" || { echo "cdh: not found" >&2; return 127; }
  sel="$("$bin" "$@" 2>/dev/tty)"
  [ -n "$sel" ] && builtin cd -- "$sel"
}
alias cdh="__cdh_run"

# 日志：通过 PROMPT_COMMAND 监控目录变化（不覆写 cd）
__cdh_log() {
  local now raw uniq
  now="$(date +%s)"
  raw="$HOME/.cd_history_raw"
  uniq="$HOME/.cd_history"
  mkdir -p "$(dirname "$raw")" "$(dirname "$uniq")"
  touch "$raw" "$uniq"
  # 去抖 2 秒 + 仅在目录变化时记录
  if [ "$PWD" = "${__CDH_LAST_DIR:-}" ] && [ $(( now - ${__CDH_LAST_TS:-0} )) -lt 2 ]; then
    return
  fi
  printf "%s\t%s\n" "$now" "$PWD" >> "$raw"
  printf "%s\n" "$PWD" >> "$uniq"
  __CDH_LAST_DIR="$PWD"; __CDH_LAST_TS="$now"
}
# 把 __cdh_log 挂到 PROMPT_COMMAND（保持已有）
if [[ -n "${PROMPT_COMMAND:-}" ]]; then
  PROMPT_COMMAND="__cdh_log;$PROMPT_COMMAND"
else
  PROMPT_COMMAND="__cdh_log"
fi
# <<< cdh (installer marker)
BASH
    ok "已写入 ~/.bashrc：cdh 包装 + 目录日志"
  else
    warn "~/.bashrc 已包含 cdh 标记，跳过重复写入"
  fi
}

install_shell_integration() {
  case "$CDH_SHELL" in
    none) warn "跳过 Shell 集成（CDH_SHELL=none）"; return 0 ;;
    auto)
      case "$SHELL_BASENAME" in
        fish) install_fish_wrapper; install_fish_logger ;;
        zsh)  install_zsh_wrapper_and_logger ;;
        bash|sh|*) install_bash_wrapper_and_logger ;;
      esac ;;
    fish) install_fish_wrapper; install_fish_logger ;;
    zsh)  install_zsh_wrapper_and_logger ;;
    bash) install_bash_wrapper_and_logger ;;
    *) warn "未知 CDH_SHELL=$CDH_SHELL，按 auto 处理"; install_shell_integration "auto" ;;
  esac
}

# ---------------- 卸载 ----------------
uninstall_all() {
  info "卸载 cdh 二进制与 Shell 集成"
  rm -f "${CDH_BIN_DIR}/cdh" || true

  # fish
  rm -f "$HOME/.config/fish/functions/cdh.fish" || true
  # 若用户愿意，移除我们写的 cd 日志（谨慎起见，只删我们创建的文件）
  if grep -q "cd + log to ~/.cd_history" "$HOME/.config/fish/functions/cd.fish" 2>/dev/null; then
    rm -f "$HOME/.config/fish/functions/cd.fish" || true
  fi

  # zsh/bash：按标记删除
  for rc in "$HOME/.zshrc" "$HOME/.bashrc"; do
    [[ -f "$rc" ]] || continue
    if grep -q "# >>> cdh (installer marker)" "$rc"; then
      awk '
        BEGIN{skip=0}
        /# >>> cdh \(installer marker\)/{skip=1; next}
        /# <<< cdh \(installer marker\)/{skip=0; next}
        skip==0{print}
      ' "$rc" > "$rc.tmp" && mv "$rc.tmp" "$rc"
      ok "已清理 $rc 中的 cdh 段落"
    fi
  done

  ok "卸载完成（历史数据 ~/.cd_history*_ 未动）"
}

post_message() {
  cat <<'TXT'
----------------------------------------
安装完成

• 重新打开一个终端（或手动 source rc）后可用：
    cdh             # 打开 TUI；回车选中即切换目录

• 历史文件：
    ~/.cd_history_raw   # timestamp<TAB>path
    ~/.cd_history       # 最近唯一目录（每行一个）
  cdh 将基于这些数据进行推荐。

• 验证：
    cdh --version
    cdh --help
----------------------------------------
TXT
}

# ---------- 主流程 ----------
if [[ "$DO_UNINSTALL" = "1" ]]; then
  uninstall_all
  exit 0
fi

ensure_bindir
install_binary
install_shell_integration
post_message
