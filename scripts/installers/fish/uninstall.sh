#!/usr/bin/env bash
# scripts/installers/fish/uninstall.sh
# fish 卸载器：移除函数/钩子/二进制/历史；不保存日志；配合入口的阶段目录清理
set -Eeuo pipefail

unset LC_ALL || true
unset LANG   || true

# -------- 阶段目录（若父脚本未提供则自建并自行清理） --------
if [[ -n "${STAGE_DIR:-}" ]]; then
  CLEAN_STAGE=0
else
  STAGE_DIR="$(mktemp -d "${TMPDIR:-/tmp}/cdh-stage.XXXXXX")"
  CLEAN_STAGE=1
fi
_cleanup() { [[ "${CLEAN_STAGE}" -eq 1 ]] && rm -rf "${STAGE_DIR}" || true; }
trap _cleanup EXIT

FISH_FUNC="${HOME}/.config/fish/functions/cdh.fish"
FISH_CONF="${HOME}/.config/fish/conf.d/cdh_log.fish"
BIN="${HOME}/.local/bin/cdh"
HIST="${HOME}/.cd_history_raw"

# 逐项删除（存在即删）
rm -f "${FISH_FUNC}" "${FISH_CONF}" "${BIN}" "${HIST}" || true

echo "[cdh] 已卸载（若存在则已删除）："
echo " - ${FISH_FUNC}"
echo " - ${FISH_CONF}"
echo " - ${BIN}"
echo " - ${HIST}"
echo "[cdh] 建议执行：exec fish -l"
