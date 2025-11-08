#!/usr/bin/env bash
# scripts/installers/fish/uninstall.sh
set -Eeuo pipefail

FISH_FUNC="${HOME}/.config/fish/functions/cdh.fish"
FISH_CONF="${HOME}/.config/fish/conf.d/cdh_log.fish"
BIN="${HOME}/.local/bin/cdh"
RAW="${HOME}/.cd_history_raw"

printf "[cdh] 开始卸载 fish 集成...\n" >&2

# 安全删除（存在即删，不报错）
rm -f "${FISH_FUNC}" || true
rm -f "${FISH_CONF}" || true
rm -f "${BIN}" || true
rm -f "${RAW}" || true

printf "[cdh] 卸载完成：\n" >&2
printf " - 已移除 %s（若存在）\n" "${FISH_FUNC}" >&2
printf " - 已移除 %s（若存在）\n" "${FISH_CONF}" >&2
printf " - 已移除 %s（若存在）\n" "${BIN}" >&2
printf " - 已移除 %s（若存在）\n" "${RAW}" >&2
printf "[cdh] 建议：exec fish -l 以刷新 shell\n" >&2
