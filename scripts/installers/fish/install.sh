#!/usr/bin/env bash
set -Eeuo pipefail
FISH_FUNC_DIR="${HOME}/.config/fish/functions"
FISH_CONF_DIR="${HOME}/.config/fish/conf.d"
MARK_BEGIN="# >>> cdh installer >>>"
MARK_END="# <<< cdh installer <<<"

mkdir -p "${FISH_FUNC_DIR}" "${FISH_CONF_DIR}"

{
  echo "${MARK_BEGIN}"
  cat "scripts/installers/fish/payload/cdh.fish"
  echo "${MARK_END}"
} > "${FISH_FUNC_DIR}/cdh.fish"

{
  echo "${MARK_BEGIN}"
  cat "scripts/installers/fish/payload/cdh_log.fish"
  echo "${MARK_END}"
} > "${FISH_CONF_DIR}/cdh_log.fish"

printf "[cdh] 写入完成：\n - %s\n - %s\n" "${FISH_FUNC_DIR}/cdh.fish" "${FISH_CONF_DIR}/cdh_log.fish" >&2
printf "[cdh] 刷新当前会话：exec fish -l\n" >&2
