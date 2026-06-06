# cdh_log.zsh - zsh hook for logging directory changes via `cdh log`

typeset -g CDH_LOG_LAST_DIR=""
typeset -g CDH_LOG_LAST_TS=0

_cdh_resolve_bin() {
  local bin

  if [[ -n "${CDH_BIN:-}" ]]; then
    bin="$CDH_BIN"
    [[ -x "$bin" ]] && { print -r -- "$bin"; return 0; }
  fi

  bin="$(whence -p cdh 2>/dev/null)"
  [[ -n "$bin" && -x "$bin" ]] && { print -r -- "$bin"; return 0; }

  [[ -x "$HOME/.local/bin/cdh" ]] && { print -r -- "$HOME/.local/bin/cdh"; return 0; }
  return 1
}

_cdh_log_dir_change() {
  local now bin
  now=$(date +%s 2>/dev/null || printf '%s\n' "$EPOCHSECONDS")

  [[ -z "$PWD" ]] && return 0

  # 2 秒去抖
  if [[ "$PWD" == "$CDH_LOG_LAST_DIR" ]] && (( CDH_LOG_LAST_TS > 0 && now - CDH_LOG_LAST_TS < 2 )); then
    return 0
  fi

  bin="$(_cdh_resolve_bin)" || return 0
  "$bin" log --dir "$PWD" >/dev/null 2>&1 || true

  CDH_LOG_LAST_DIR="$PWD"
  CDH_LOG_LAST_TS=$now
}

# 用 chpwd_functions 注册 hook，避免覆写用户自己的 chpwd
typeset -ga chpwd_functions
if [[ "${chpwd_functions[(Ie)_cdh_log_dir_change]}" -eq 0 ]]; then
  chpwd_functions+=(_cdh_log_dir_change)
fi

# 启用时先记录一次当前目录
_cdh_log_dir_change
