#!/usr/bin/env sh
# Short GitHub Pages entrypoint for cdh.
set -eu

INSTALL_URL="${CDH_INSTALL_URL:-https://raw.githubusercontent.com/xianyudd/cdh/main/scripts/install.sh}"

if command -v curl >/dev/null 2>&1; then
  curl -fsSL "$INSTALL_URL" | bash --noprofile --norc -s -- "$@"
elif command -v wget >/dev/null 2>&1; then
  wget -qO- "$INSTALL_URL" | bash --noprofile --norc -s -- "$@"
else
  echo "[cdh] need curl or wget to download installer: $INSTALL_URL" >&2
  exit 127
fi
