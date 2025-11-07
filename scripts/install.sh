#!/usr/bin/env bash
set -euo pipefail

# =====================================
# cdh 一键安装脚本（可检测 fish 自带函数冲突）
# 支持：Linux / macOS，fish / bash / zsh
# =====================================

REPO="xianyudd/cdh"
APP="cdh"
PREFIX="${HOME}/.local"
BINDIR="${PREFIX}/bin"
OS="$(uname -s)"
ARCH="$(uname -m)"

# ---------- 自动检测当前 Shell ----------
detect_shell() {
  local current_shell
  current_shell="$(ps -p $$ -o comm= | head -n1 | xargs basename 2>/dev/null || echo sh)"
  if [ -z "$current_shell" ] || [ "$current_shell" = "sh" ]; then
    current_shell="$(basename "${SHELL:-sh}")"
  fi
  echo "$current_shell"
}
SHELL_BASENAME="$(detect_shell)"

# ---------- 彩色输出 ----------
color() { printf "\033[%sm%s\033[0m\n" "$1" "$2"; }
info()  { color "36" "==> $*"; }
ok()    { color "32" "✔ $*"; }
warn()  { color "33" "⚠ $*"; }
err()   { color "31" "✘ $*" >&2; }

need_cmd() { command -v "$1" >/dev/null 2>&1 || { err "缺少依赖：$1"; exit 1; }; }

# ---------- 平台与架构检测 ----------
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

# ---------- 查询最新版本 ----------
get_latest_asset_url() {
  local version="${CDH_VERSION:-}"
  if [ -z "${version}" ]; then
    need_cmd curl
    local api="https://api.github.com/repos/${REPO}/releases/latest"
    info "查询最新版本…"
    local json
    json="$(curl -fsSL "$api")"
    version="$(printf '%s' "$json" | sed -n 's/ *"tag_name": *"\(v[^"]*\)".*/\1/p' | head -n1)"
    [ -n "$version" ] || { err "无法解析最新版本 tag"; exit 1; }
  fi

  ASSET_NAME="${APP}-${version}-${TARGET}.tar.gz"
  local api_tag="https://api.github.com/repos/${REPO}/releases/tags/${version}"
  local json2
  json2="$(curl -fsSL "$api_tag")" || { err "获取 ${version} 版本信息失败"; exit 1; }
  ASSET_URL="$(printf '%s' "$json2" | sed -n "s# *\"browser_download_url\": *\"\\(.*${ASSET_NAME}\\)\"#\\1#p" | head -n1)"
  [ -n "${ASSET_URL:-}" ] || { err "未找到资产：${ASSET_NAME}"; exit 1; }
}

# ---------- 确保 ~/.local/bin 已加入 PATH ----------
ensure_bindir() {
  mkdir -p "$BINDIR"
  case ":$PATH:" in
    *":$BINDIR:"*) ;; # 已存在
    *)
      warn "你的 PATH 中尚无 ${BINDIR}"
      case "$SHELL_BASENAME" in
        fish)
          fish -lc "set -Ux fish_user_paths ${BINDIR} \$fish_user_paths" || true
          ok "已为 fish 加入 PATH：${BINDIR}" ;;
        zsh)
          echo "export PATH=\"${BINDIR}:\$PATH\"" >> "${HOME}/.zshrc"
          ok "已写入 ~/.zshrc：PATH+=${BINDIR}" ;;
        bash|sh|*)
          echo "export PATH=\"${BINDIR}:\$PATH\"" >> "${HOME}/.bashrc"
          ok "已写入 ~/.bashrc：PATH+=${BINDIR}" ;;
      esac ;;
  esac
}

# ---------- 下载并安装二进制 ----------
install_binary() {
  need_cmd curl
  need_cmd tar
  detect_target
  get_latest_asset_url

  info "下载 ${ASSET_NAME}"
  tmpdir="$(mktemp -d)"
  trap 'rm -rf "$tmpdir"' EXIT
  curl -fL "$ASSET_URL" -o "$tmpdir/$ASSET_NAME"
  info "解压到临时目录"
  tar -C "$tmpdir" -xzf "$tmpdir/$ASSET_NAME"
  bin_path="$(find "$tmpdir" -type f -name "${APP}" -perm -111 | head -n1)"
  [ -n "$bin_path" ] || { err "未在压缩包中找到可执行文件 ${APP}"; exit 1; }

  info "安装到 ${BINDIR}/${APP}"
  install -m 0755 "$bin_path" "${BINDIR}/${APP}"
  ok "二进制安装完成：$(command -v ${APP} || echo ${BINDIR}/${APP})"
}

# ---------- 安装 Shell 集成 ----------
install_shell_integration() {
  case "$SHELL_BASENAME" in
    fish)
      funcdir="${HOME}/.config/fish/functions"
      mkdir -p "$funcdir"

      # 检查 fish 自带 cdh 函数是否存在
      if fish -c "functions cdh" >/dev/null 2>&1; then
        warn "检测到 fish 自带函数 cdh（Change Directory History）"
        read -p "是否覆盖它以启用 Rust cdh？(y/N): " ans
        ans="${ans:-N}"
        if [[ "$ans" =~ ^[Yy]$ ]]; then
          fish -c "functions --erase cdh" || true
          ok "已移除 fish 内置 cdh"
        else
          ok "保留 fish 原有 cdh，不安装覆盖函数"
          return 0
        fi
      fi

      # 定义 cdh 函数（覆盖或新增）
      cat > "${funcdir}/cdh.fish" <<'FISH'
function cdh -d "cd via Rust cdh (TUI)"
    set -l bin (command -v cdh)
    if not test -x "$bin"
        echo "cdh: not found" >&2
        return 127
    end
    set -l sel (command $bin $argv)
    if test -n "$sel"
        builtin cd -- "$sel"
    end
end
FISH
      ok "已安装 fish 函数：cdh（含自动 cd）"

      # 安装目录日志功能
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
      ok "已安装 fish 目录日志"
      ;;

    bash|zsh|*)
      rc="${HOME}/.${SHELL_BASENAME}rc"
      if ! grep -q "__cdh_func" "$rc" 2>/dev/null; then
        cat >> "$rc" <<'SH'
# --- cdh: 调用 Rust 版并自动 cd ---
__cdh_func() {
  local bin sel
  bin="$(command -v cdh)" || { echo "cdh: not found" >&2; return 127; }
  sel="$("$bin" "$@" 2>/dev/tty)"
  [ -n "$sel" ] && builtin cd -- "$sel"
}
alias cdh="__cdh_func"
SH
        ok "已写入 ${rc}：cdh 自动切换目录"
      fi
      ;;
  esac
}

# ---------- 安装完成提示 ----------
post_message() {
  cat <<'TXT'
----------------------------------------
安装完成

• 重新打开一个终端（或手动 source rc）后可用：
    cdh             # 打开 TUI 并自动切换到选中目录

• 目录日志：
  已为你的 Shell 装好轻量日志（~/.cd_history_raw / ~/.cd_history），
  cdh 将基于这些数据提供推荐。

• 验证：
    cdh --help
----------------------------------------
TXT
}

# ---------- 主流程 ----------
ensure_bindir
install_binary
install_shell_integration
post_message
