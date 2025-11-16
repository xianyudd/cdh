# cdh.zsh - zsh wrapper for the `cdh` binary

# 允许用户通过环境变量显式指定二进制路径：
#   export CDH_BIN="$HOME/.local/bin/cdh"
: "${CDH_BIN:=}"

_cdh_resolve_bin() {
  local bin

  # 1) 优先使用显式设置的 CDH_BIN
  if [[ -n "$CDH_BIN" ]]; then
    bin="$CDH_BIN"
    if [[ -x "$bin" ]]; then
      echo "$bin"
      return 0
    else
      echo "cdh: \$CDH_BIN 指向的文件不可执行: $bin" >&2
      return 1
    fi
  fi

  # 2) 回退到 PATH 中的 cdh
  bin="$(command -v cdh 2>/dev/null)"
  if [[ -n "$bin" ]]; then
    echo "$bin"
    return 0
  fi

  # 3) 都找不到，输出调试信息
  echo "cdh: 未找到可执行文件。" >&2
  echo "  - 建议：" >&2
  echo "    1) 确认 ~/.local/bin/cdh 是否存在且可执行" >&2
  echo "    2) 将 ~/.local/bin 加入 PATH，或设置 CDH_BIN 变量" >&2
  return 1
}

cdh() {
  local bin
  bin="$(_cdh_resolve_bin)" || return $?

  # 在 zsh 里 status 是只读变量，这里用 rc 保存退出码
  local dest rc
  dest="$("$bin" "$@")"
  rc=$?

  # 非 0 退出则不切换目录（视为用户取消或错误）
  if (( rc != 0 )); then
    return $rc
  fi

  # 没有输出就不改变目录
  if [[ -z "$dest" ]]; then
    return 0
  fi

  # 去掉末尾换行
  dest="${dest%%$'\n'}"

  # 确认目标是目录
  if [[ ! -d "$dest" ]]; then
    echo "cdh: target is not a directory: $dest" >&2
    return 1
  fi

  builtin cd -- "$dest"
}

