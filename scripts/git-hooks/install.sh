#!/usr/bin/env bash
# Installs the repo's git hooks (scripts/git-hooks) into the shared git dir, for the main checkout and
# all its worktrees, and sets this repo's commit email to the GitHub noreply address.
# usage: scripts/git-hooks/install.sh
set -euo pipefail
[[ ${1:-} != help ]] || { sed -n '2,4p' "$0" >&2; exit 2; }
dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
hooks=$(git rev-parse --git-common-dir)/hooks
mkdir -p "$hooks"
install -m 755 "$dir/pre-push" "$hooks/pre-push"
git config user.email 6751787+t3dotgg@users.noreply.github.com
echo "installed $hooks/pre-push; user.email $(git config user.email)"
