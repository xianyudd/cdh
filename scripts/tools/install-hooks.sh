#!/usr/bin/env bash
set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"
hooks_dir="$repo_root/.git/hooks"
mkdir -p "$hooks_dir"

cat > "$hooks_dir/pre-commit" << 'EOF'
#!/usr/bin/env bash
repo_root="$(git rev-parse --show-toplevel)"
exec "$repo_root/scripts/tools/pre-commit-check.sh" "$@"
EOF
chmod +x "$hooks_dir/pre-commit"

cat > "$hooks_dir/commit-msg" << 'EOF'
#!/usr/bin/env bash
repo_root="$(git rev-parse --show-toplevel)"
exec "$repo_root/scripts/tools/commit-msg-check.sh" "$1"
EOF
chmod +x "$hooks_dir/commit-msg"

echo "Installed pre-commit and commit-msg hooks."
