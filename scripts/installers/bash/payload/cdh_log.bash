# ~/.config/cdh/bash/cdh_log.bash
: "${__CDH_LAST_DIR:=}"
: "${__CDH_LAST_TS:=0}"

__cdh_resolve_bin() {
  local bin=""

  if [ -n "${CDH_BIN:-}" ] && [ -x "$CDH_BIN" ]; then
    printf '%s\n' "$CDH_BIN"
    return 0
  fi

  bin="$(type -P cdh 2>/dev/null || true)"
  if [ -n "$bin" ] && [ -x "$bin" ]; then
    printf '%s\n' "$bin"
    return 0
  fi

  if [ -x "$HOME/.local/bin/cdh" ]; then
    printf '%s\n' "$HOME/.local/bin/cdh"
    return 0
  fi

  return 1
}

__cdh_log() {
  local bin="" now cur
  now="$(date +%s 2>/dev/null)" || return 0
  cur="$PWD"; [ -z "$cur" ] && return 0

  # 2 秒去抖
  if [ "$cur" = "$__CDH_LAST_DIR" ] && [ $(( now - __CDH_LAST_TS )) -lt 2 ]; then
    return 0
  fi

  bin="$(__cdh_resolve_bin)" || return 0
  "$bin" log --dir "$cur" >/dev/null 2>&1 || true

  __CDH_LAST_DIR="$cur"
  __CDH_LAST_TS="$now"
}

# 挂到 PROMPT_COMMAND（不导出）
if [ -n "${PROMPT_COMMAND:-}" ]; then
  case ";$PROMPT_COMMAND;" in
    *";__cdh_log;"*) : ;;
    *) PROMPT_COMMAND="__cdh_log; ${PROMPT_COMMAND}" ;;
  esac
else
  PROMPT_COMMAND="__cdh_log"
fi
