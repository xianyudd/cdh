#!/usr/bin/env bash
set -euo pipefail

REPO="xianyudd/cdh"
APP="cdh"
PREFIX="${HOME}/.local"
BINDIR="${PREFIX}/bin"
OS="$(uname -s)"
ARCH="$(uname -m)"

# 自动检测当前运行的 Shell（更可靠 than $SHELL）
# - fish 下执行脚本时会检测到 "fish"
# - bash/zsh 下亦能正确识别
# - 某些非交互 shell（如 /bin/sh）下 fallback 为 login shell
detect_shell() {
  local current_shell
  current_shell="$(ps -p $$ -o comm= | head -n1 | xargs basename 2>/dev/null || echo sh)"
  if [ -z "$current_shell" ] || [ "$current_shell" = "sh" ]; then
    current_shell="$(basename "${SHELL:-sh}")"
  fi
  echo "$current_shell"
}

SHELL_BASENAME="$(detect_shell)"


color() { printf "\033[%sm%s\033[0m\n" "$1" "$2"; }
info()  { color "36" "==> $*"; }
ok()    { color "32" "✔ $*"; }
warn()  { color "33" "⚠ $*"; }
err()   { color "31" "✘ $*" >&2; }

need_cmd() { command -v "$1" >/dev/null 2>&1 || { err "缺少依赖：$1"; exit 1; }; }

detect_target() {
  case "$OS" in
    Linux)  os_tag=linux ;;
    Darwin) os_tag=darwin ;;
    *) err "不支持的系统：$OS"; exit 1;;
  esac

  case "$ARCH" in
    x86_64|amd64) arch_tag=x86_64 ;;
    arm64|aarch64) arch_tag=aarch64 ;;
    *) err "不支持的架构：$ARCH"; exit 1;;
  esac

  if [ "$os_tag" = "linux" ] && [ "$arch_tag" = "aarch64" ]; then
    # 你当前 CI 只构建了 x86_64-unknown-linux-gnu；若以后补上 aarch64 就能自动生效
    err "暂未提供 Linux aarch64 构建资产"
    exit 1
  fi

  case "${os_tag}-${arch_tag}" in
    linux-x86_64)   TARGET="x86_64-unknown-linux-gnu" ;;
    darwin-x86_64)  TARGET="x86_64-apple-darwin" ;;
    darwin-aarch64) TARGET="aarch64-apple-darwin" ;;
  esac
}

get_latest_asset_url() {
  # 允许用户指定版本：CDH_VERSION=v0.1.0
  local version="${CDH_VERSION:-}"
  if [ -z "${version}" ]; then
    need_cmd curl
    # 取最新 release
    local api="https://api.github.com/repos/${REPO}/releases/latest"
    info "查询最新版本…"
    # 尽量不用 jq；用 grep/sed 抽取
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

ensure_bindir() {
  mkdir -p "$BINDIR"
  case ":$PATH:" in
    *":$BINDIR:"*) ;; # already
    *)
      warn "你的 PATH 中尚无 ${BINDIR}"
      case "$SHELL_BASENAME" in
        fish)
          if command -v fish >/dev/null 2>&1; then
            fish -lc "set -Ux fish_user_paths ${BINDIR} \$fish_user_paths" || true
            ok "已为 fish 加入 PATH：${BINDIR}"
          fi
          ;;
        zsh)
          echo "export PATH=\"${BINDIR}:\$PATH\"" >> "${HOME}/.zshrc"
          ok "已写入 ~/.zshrc：PATH+=${BINDIR}"
          ;;
        bash|sh|*)
          echo "export PATH=\"${BINDIR}:\$PATH\"" >> "${HOME}/.bashrc"
          ok "已写入 ~/.bashrc：PATH+=${BINDIR}"
          ;;
      esac
      ;;
  esac
}

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
  # 包内目录形如 cdh-vX.Y.Z-TARGET/cdh
  bin_path="$(find "$tmpdir" -type f -name "${APP}" -perm -111 | head -n1)"
  [ -n "$bin_path" ] || { err "未在压缩包中找到可执行文件 ${APP}"; exit 1; }

  info "安装到 ${BINDIR}/${APP}"
  install -m 0755 "$bin_path" "${BINDIR}/${APP}"
  ok "二进制安装完成：$(command -v ${APP} || echo ${BINDIR}/${APP})"
}

install_shell_integration() {
  case "$SHELL_BASENAME" in
    fish)
      # 1) 交互调用器：cdf（TUI 走 stderr；stdout 只有目录）
      funcdir="${HOME}/.config/fish/functions"
      mkdir -p "$funcdir"
      cat > "${funcdir}/cdf.fish" <<'FISH'
function cdf -d "cd via cdh (Rust TUI: stderr UI, stdout path)"
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
      ok "已安装 fish 函数：cdf"

      # 2) 目录日志：覆盖 cd（轻量版，产生 ~/.cd_history 与 ~/.cd_history_raw）
      cat > "${funcdir}/cd.fish" <<'FISH'
functions --erase cd 2>/dev/null
function cd --wraps=cd -d "cd + log to ~/.cd_history(_raw)"
    builtin cd -- $argv; or return
    set -l now (date +%s)
    set -l raw ~/.cd_history_raw
    set -l uniq ~/.cd_history
    test -e $raw; or touch $raw
    test -e $uniq; or touch $uniq
    # 去抖：同路径 2 秒内不重复
    if test "$__CDH_LAST_DIR" = (pwd) -a (math "$now - $__CDH_LAST_TS" 2>/dev/null) -lt 2
        return
    end
    printf "%s\t%s\n" $now (pwd) >> $raw
    printf "%s\n" (pwd) >> $uniq
    set -g __CDH_LAST_DIR (pwd)
    set -g __CDH_LAST_TS $now
end
FISH
      ok "已安装 fish 目录日志（覆盖 cd）"
      ;;

    zsh)
      rc="${HOME}/.zshrc"
      # 调用器：cdf
      if ! grep -q "__cdh_cdf" "$rc" 2>/dev/null; then
        cat >> "$rc" <<'ZSH'
# --- cdh: cdf 调用器（TUI->stderr, stdout->path） ---
__cdh_cdf() {
  local bin
  bin="$(command -v cdh)" || { print -u2 -- "cdh: not found"; return 127; }
  local sel
  sel="$("$bin" "$@" 2>/dev/tty)"
  [ -n "$sel" ] && builtin cd -- "$sel"
}
alias cdf="__cdh_cdf"
ZSH
        ok "已写入 ~/.zshrc：cdf 调用器"
      fi
      # 目录日志：chpwd hook
      if ! grep -q "__cdh_log_chpwd" "$rc" 2>/dev/null; then
        cat >> "$rc" <<'ZSH'
# --- cdh: 目录日志（~/.cd_history_raw & ~/.cd_history） ---
__cdh_log_chpwd() {
  local now raw uniq
  now="$(date +%s)"
  raw="${HOME}/.cd_history_raw"
  uniq="${HOME}/.cd_history"
  : > /dev/null
  [ -f "$raw" ] || : > "$raw"
  [ -f "$uniq" ] || : > "$uniq"
  # 去抖：同路径 2 秒内不重复
  if [ "${__CDH_LAST_DIR:-}" = "$PWD" ] && [ $(( now - ${__CDH_LAST_TS:-0} )) -lt 2 ]; then
    return
  fi
  printf "%s\t%s\n" "$now" "$PWD" >> "$raw"
  printf "%s\n" "$PWD" >> "$uniq"
  __CDH_LAST_DIR="$PWD"
  __CDH_LAST_TS="$now"
}
autoload -Uz add-zsh-hook 2>/dev/null || true
add-zsh-hook chpwd __cdh_log_chpwd
ZSH
        ok "已写入 ~/.zshrc：目录日志 hook"
      fi
      ;;

    bash|sh|*)
      rc="${HOME}/.bashrc"
      # 调用器：cdf
      if ! grep -q "__cdh_cdf" "$rc" 2>/dev/null; then
        cat >> "$rc" <<'BASH'
# --- cdh: cdf 调用器（TUI->stderr, stdout->path） ---
__cdh_cdf() {
  local bin
  bin="$(command -v cdh)" || { echo "cdh: not found" >&2; return 127; }
  local sel
  sel="$("$bin" "$@" 2>/dev/tty)"
  [ -n "$sel" ] && builtin cd -- "$sel"
}
alias cdf="__cdh_cdf"
BASH
        ok "已写入 ~/.bashrc：cdf 调用器"
      fi
      # 目录日志：PROMPT_COMMAND（检测目录变化）
      if ! grep -q "__cdh_log_prompt" "$rc" 2>/dev/null; then
        cat >> "$rc" <<'BASH'
# --- cdh: 目录日志（~/.cd_history_raw & ~/.cd_history） ---
__cdh_log_prompt() {
  local now raw uniq cur
  cur="$PWD"
  now="$(date +%s)"
  raw="${HOME}/.cd_history_raw"
  uniq="${HOME}/.cd_history"
  [ -f "$raw" ] || : > "$raw"
  [ -f "$uniq" ] || : > "$uniq"
  # 去抖：同路径 2 秒内不重复
  if [ "${__CDH_LAST_DIR:-}" = "$cur" ] && [ $(( now - ${__CDH_LAST_TS:-0} )) -lt 2 ]; then
    return
  fi
  printf "%s\t%s\n" "$now" "$cur" >> "$raw"
  printf "%s\n" "$cur" >> "$uniq"
  __CDH_LAST_DIR="$cur"
  __CDH_LAST_TS="$now"
}
case ":$PROMPT_COMMAND:" in
  *:"__cdh_log_prompt":*) ;;
  *) PROMPT_COMMAND="__cdh_log_prompt${PROMPT_COMMAND:+; $PROMPT_COMMAND}";;
esac
BASH
        ok "已写入 ~/.bashrc：目录日志 PROMPT_COMMAND"
      fi
      ;;
  esac
}

post_message() {
  cat <<'TXT'
----------------------------------------
安装完成 🎉

• 重新打开一个终端（或手动 source rc）后可用：
    cdf             # 打开 TUI 选择目录（界面走 stderr，选中的目录写到 stdout 并 cd）

• 目录日志：
  已为你的 Shell 装好轻量日志（~/.cd_history_raw / ~/.cd_history），
  cdh 推荐会基于这些数据工作。

• 验证：
    cdh --help   # 看二进制是否就绪
    cdf          # 是否能弹出 TUI（有历史时）
----------------------------------------
TXT
}

ensure_bindir
install_binary
install_shell_integration
post_message
