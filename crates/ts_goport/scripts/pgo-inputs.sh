#!/usr/bin/env bash
# Fetches the realworld inputs of the release PGO and BOLT training that are
# not project inputs (build-release.sh, RELEASE_EXTRA_INPUTS) into
# <data-root>/target/pgo-inputs/<name>, so a release build needs no network.
#
# Each input is a git checkout at a pinned commit, installed from its lockfile
# with the pinned package manager and no install scripts, then made read-only.
# <name>/INPUT.txt records the commit and a digest of the installed tree.
# The digest leaves out .git, the package manager's own state files (pnpm
# writes timestamps and the store path there) and the node_modules/.bin shims
# (they hold the install path), so two fetches of one input give the same
# digest. `check` computes it again; build-release.sh runs
# `check` and writes its lines to BUILD.txt.
#
# Usage: pgo-inputs.sh fetch|check|help [out-root]
#   fetch     fetch each input that is missing (needs network), then check
#   check     print "<name> <commit> <digest>" per input; exit 1 when one is
#             missing or its tree changed since the fetch
#   out-root  default: <data-root>/target/pgo-inputs (GOPORT_DATA_ROOT as in
#             build-release.sh). Another root gives a fresh fetch to compare
#             digests with.
#
# Inputs:
#   eslint-plugin-svelte  union narrowing heavy: realworld4 repo 14, 0.80x Go
#     with the R169 release build (rwtime1), cliperf1 rank 3 (union sort and
#     type facts). Config packages/eslint-plugin-svelte/tsconfig.pgo.json:
#     tsconfig.build.json without baseUrl, as realworld4's fix (TypeScript 7
#     removed baseUrl).
set -euo pipefail

case "${1:-}" in
  fetch | check) action=$1 ;;
  *)
    sed -n '2,/^set -euo/{/^set -euo/d;s/^# \{0,1\}//;p}' "${BASH_SOURCE[0]}"
    [[ ${1:-} == help || ${1:-} == -h || ${1:-} == --help ]] && exit 0
    exit 2
    ;;
esac

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd -- "$script_dir/../../.." && pwd)"
data_root="${GOPORT_DATA_ROOT:-$(cd -- "$(git -C "$repo" rev-parse --path-format=absolute --git-common-dir)/.." && pwd)}"
root="${2:-$data_root/target/pgo-inputs}"

# name, git URL, commit, package manager
inputs=(
  "eslint-plugin-svelte https://github.com/sveltejs/eslint-plugin-svelte.git 18339c886320151148568063c5801bf69cb51027 pnpm@10.34.6"
)

# digest <dir>: sha256 (16 hex) of every file's content and every link target
# under <dir>, without .git, the pnpm state files and node_modules/.bin.
digest() {
  (
    cd "$1"
    local skip=(-path ./.git -o -name .modules.yaml -o -name '.pnpm-workspace-state*.json' -o -path '*/node_modules/.bin')
    {
      find . \( "${skip[@]}" \) -prune -o -type f -print0 | LC_ALL=C sort -z | xargs -0 sha256sum
      find . \( "${skip[@]}" \) -prune -o -type l -printf '%p -> %l\n' | LC_ALL=C sort
    } | sha256sum | cut -c1-16
  )
}

# fetch_one <name> <url> <commit> <pm>: checkout, install, fix config, lock.
fetch_one() {
  local name=$1 url=$2 commit=$3 pm=$4 dir="$root/$1" tmp pnpm
  [[ -e $dir.part ]] && chmod -R u+w "$dir.part"
  rm -rf "$dir.part"
  mkdir -p "$dir.part"
  git -C "$dir.part" init -q source
  git -C "$dir.part/source" fetch -q --depth 1 "$url" "$commit"
  git -C "$dir.part/source" -c advice.detachedHead=false checkout -q FETCH_HEAD
  # The store and caches live in a temp dir on the same file system (the
  # install links or reflinks its files), removed after the install.
  tmp="$(mktemp -d "$root/.store.XXXXXX")"
  case $pm in
    pnpm@*)
      pnpm=(pnpm)
      [[ $(pnpm --version 2> /dev/null) == "${pm#pnpm@}" ]] || pnpm=(env COREPACK_ENABLE_DOWNLOAD_PROMPT=0 COREPACK_HOME="$tmp/corepack" corepack "$pm")
      (cd "$dir.part/source" && env XDG_CACHE_HOME="$tmp/cache" "${pnpm[@]}" install --frozen-lockfile --ignore-scripts --store-dir "$tmp/store") \
        > "$dir.part/install.log" 2>&1 || { tail -20 "$dir.part/install.log" >&2; rm -rf "$tmp"; exit 1; }
      ;;
    *) echo "error: no install rule for $pm" >&2; exit 1 ;;
  esac
  rm -rf "$tmp"
  case $name in
    eslint-plugin-svelte)
      printf '{\n  "extends": "./tsconfig.build.json",\n  "compilerOptions": { "baseUrl": null }\n}\n' \
        > "$dir.part/source/packages/eslint-plugin-svelte/tsconfig.pgo.json"
      ;;
  esac
  printf '%s %s %s\n' "$name" "$commit" "$(digest "$dir.part/source")" > "$dir.part/INPUT.txt"
  chmod -R a-w "$dir.part/source"
  [[ -e $dir ]] && chmod -R u+w "$dir"
  rm -rf "$dir"
  mv "$dir.part" "$dir"
  echo "fetched $(cat "$dir/INPUT.txt") ($pm)"
}

mkdir -p "$root"
rc=0
for line in "${inputs[@]}"; do
  read -r name url commit pm <<< "$line"
  if [[ $action == fetch && ! -f $root/$name/INPUT.txt ]]; then
    fetch_one "$name" "$url" "$commit" "$pm"
  fi
  if [[ ! -f $root/$name/INPUT.txt ]]; then
    echo "error: $root/$name is missing (run $script_dir/pgo-inputs.sh fetch)" >&2
    rc=1
    continue
  fi
  want="$(cat "$root/$name/INPUT.txt")"
  have="$name $(git -C "$root/$name/source" rev-parse HEAD) $(digest "$root/$name/source")"
  if [[ $have != "$want" || $want != "$name $commit "* ]]; then
    echo "error: $root/$name changed: INPUT.txt says '$want', the tree is '$have' (pinned $commit)" >&2
    rc=1
    continue
  fi
  echo "$have"
done
exit $rc
