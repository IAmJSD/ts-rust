#!/usr/bin/env bash
set -euo pipefail

# Locks prepared project inputs read-only so no checker, oracle or agent can
# write into them. Unlock only to re-run a prepare-*-inputs script, then lock.
# Usage: scripts/project-inputs.sh lock|unlock|status [project...]

repo="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
inputs="${TS_PROJECT_INPUTS_DIR:-$(git -C "$repo" rev-parse --path-format=absolute --git-common-dir)/../target/project-inputs}"
inputs="$(realpath -- "$inputs")"
action="${1:-status}"
shift || true
if (($# == 0)); then
  mapfile -t projects < <(find "$inputs" -mindepth 2 -maxdepth 2 \( -name source -o -name project \) -type d -printf '%h\n' | xargs -rn1 basename | sort -u)
else
  projects=("$@")
fi

for project in "${projects[@]}"; do
  # Most inputs keep their files in source/; the wave202 inputs use project/.
  source_dir="$inputs/$project/source"
  [[ -d "$source_dir" ]] || source_dir="$inputs/$project/project"
  if [[ ! -d "$source_dir" ]]; then
    echo "No prepared source: $source_dir" >&2
    exit 2
  fi
  case "$action" in
    lock) chmod -R a-w -- "$source_dir" ;;
    unlock) chmod -R u+w -- "$source_dir" ;;
    status) ;;
    *) echo "Usage: $0 lock|unlock|status [project...]" >&2; exit 2 ;;
  esac
  writable="$(find "$source_dir" -perm /222 -not -type l -print -quit)"
  printf '%-16s %s\n' "$project" "$([[ -z "$writable" ]] && echo read-only || echo WRITABLE)"
done
