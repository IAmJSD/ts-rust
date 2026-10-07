#!/usr/bin/env bash
# Remote measurement runners. zbook builds; alvin, cup2, dbook and the minis run gates, corpus suites, sweeps and
# oracle checks.
# Each host keeps a mirror at the same absolute paths as zbook: this repo's tooling, target/project-inputs*,
# the target/continuation-r97-goport runners, oracle caches and default binaries, ~/.local/bin/tsgo-oracle and
# ~/.explore/repos/microsoft__typescript-go (see target/continuation-r97-goport/remote/*-manifest.txt, *-setup.md).
# usage: remote.sh help                       this text
#        remote.sh status [host...]            per host: zbook lock (holder, age, waiters), load, free RAM, top process
#        remote.sh look <host> <command...>    a quick look (logs, files, tools): no lock, 120 s limit
#        remote.sh run <host> <command...>     a job in the repo root with a login shell, under the host lock;
#                                              output and stdin pass through: run <host> bash -s <<'EOF' works
#        remote.sh job <host> <command...>     run a zbook command (a script that syncs, runs and fetches) under
#                                              the host lock, with REMOTE_HOST set to the host
#        remote.sh sync-bins <host> <dir>...   copy the top-level files of binary dirs (not deps/ or build/)
#        remote.sh sync-scripts <host>         copy repo scripts, tools, .git, runner scripts under target/, /tmp/port
#        remote.sh sync-pin <host> [pin]       scripts/upstream/pin.py sync over this host's route
#        remote.sh push <host> <path>...       copy files or dir trees to the same absolute path (never deletes)
#        remote.sh fetch <host> <dir>...       copy result dirs back to zbook; adds new files, never replaces one
# <host> is alvin, cup2, dbook-lan, mini-743d, "all" (sync-*: every host at the same time) or "auto"
# (run and job: the first host whose lock is free and whose load is under half its cores; when none is free,
# it checks again every 30 s). Host order is alvin, cup2, dbook-lan, mini-743d (REMOTE_HOSTS overrides it).
# Locks: run and job hold the zbook lock /tmp/goport-remote-<host>.lock until the command ends, and wait for it.
# A caller that holds the lock already (flock FILE cmd, or exec 9>FILE; flock 9) passes the open lock file on to
# its children; run sees it and does not lock again. So a script under `job`, and the older
# `flock /tmp/goport-remote-<host>.lock remote.sh run <host> ...` form, both work. A script under `job` must not
# take the lock itself: it would wait for its own lock.
# Reserved: dbook-lan is kept for revision evidence (candidate.sh side runs the gate and the oracles there). auto
# skips it, and run and job refuse it unless REMOTE_USE_RESERVED=1 (candidate.sh sets it; the auditor and reviewer
# set it to repeat a revision run). Time on mini-743d. On 2026-09-30 the 11-minute R148 and R149
# gates waited 104 and 45 minutes for skeptic timing jobs on dbook. REMOTE_RESERVED overrides the list ("" for none).
# Retired: mini-abf9 is Theo's machine since 2026-10-04. Every command refuses it.
# dbook and the minis are on zbook's LAN and always go over it, never Tailscale. Relative dirs are relative to the repo root.
set -uo pipefail
REPO=/home/theo/Code/sandbox/ts-rust
T=$REPO/target
# The ssh config names the minis over Tailscale. These options set the LAN name, and HostKeyAlias checks
# the host key that ~/.ssh/known_hosts has for the Tailscale name (the config's HostName, from ssh -G, so
# the tailnet name stays out of the repo). dbook-lan is a LAN alias in the ssh config.
ts_name() { ssh -G "$1" 2> /dev/null | awk '$1 == "hostname" { print $2 }'; }
declare -A SSH_OPTS=(
  [mini-743d]="-o HostName=mini-743d.local -o HostKeyAlias=$(ts_name mini-743d)"
)
# The name remote.sh uses for a host. Tailscale names of LAN hosts map to their LAN route.
canon() {
  case $1 in
    dbook|dbook-ts) echo dbook-lan ;;
    mini-743d-ts) echo mini-743d ;;
    mini-abf9-1|mini-abf9-1-ts|mini-abf9-ts) echo mini-abf9 ;;
    *) echo "$1" ;;
  esac
}
read -ra HOSTS <<< "${REMOTE_HOSTS:-alvin cup2 dbook-lan mini-743d}"
for i in "${!HOSTS[@]}"; do HOSTS[i]=$(canon "${HOSTS[i]}"); done
read -ra RESERVED <<< "${REMOTE_RESERVED-dbook-lan}"
# True when host $1 is kept for revision evidence (see Reserved above).
reserved() { local r; for r in "${RESERVED[@]}"; do [[ $(canon "$r") == "$1" ]] && return 0; done; return 1; }
RS=(rsync -aH --mkpath --compress --compress-choice=zstd --info=progress2)
# rsync to or from host $1 with its ssh options.
rs() { local h=$1 e=(); shift; [[ -n ${SSH_OPTS[$h]:-} ]] && e=(-e "ssh ${SSH_OPTS[$h]}"); "${RS[@]}" "${e[@]}" "$@"; }
# ssh options for a short, non-interactive call.
QUICK=(-o ConnectTimeout=5 -o BatchMode=yes)

die() { echo "remote.sh: $*" >&2; exit 2; }
# Absolute path with symlinks kept, so it names the same place on both sides.
abs() { (cd "$REPO" && realpath -ms "$1"); }
# Never write into project inputs. Their measure/ output dirs are allowed (the sweep scripts write there).
guard() { [[ $1 != "$T"/project-inputs* || $1 == "$T"/project-inputs*/measure/?* ]] || die "refusing to write into project inputs: $1"; }
# The zbook-side lock that serializes jobs on host $1 (dbook-lan uses the dbook lock).
lockfile() { echo "/tmp/goport-remote-${1%-lan}.lock"; }
# True when this process has host $1's lock file open, because a caller that holds the lock passed it on.
inherited() {
  local f l; l=$(lockfile "$1")
  for f in /proc/$$/fd/*; do [[ $(readlink "$f") == "$l" ]] && return 0; done
  return 1
}
# "free", or "held <age> by <command>, <n> waiting" for host $1's lock. The holder is the oldest process with the
# lock file open that is not waiting for it (a flock waiter and the shell that started it are skipped) and is not
# this remote.sh (inside a job it has the lock file open too).
lockstate() {
  local l ma mi ino id w p s cmd pids; l=$(lockfile "$1")
  { [[ ! -e $l ]] || flock -n "$l" true; } && { echo free; return; }
  read -r ma mi ino < <(stat -c '%Hd %Ld %i' "$l")
  id=$(printf '%02x:%02x:%d' "$ma" "$mi" "$ino")
  w=$(awk -v id="$id" '$2 == "->" && $7 == id { print $6 }' /proc/locks)
  pids=$(find /proc/[0-9]*/fd -lname "$l" 2> /dev/null | cut -d/ -f3 | sort -u)
  for p in $w; do pids=$(grep -vxE "$p|$(ps -o ppid= -p "$p" | tr -d ' ')" <<< "$pids"); done
  pids=$(grep -vxE "$$|$BASHPID" <<< "$pids")
  [[ -n $pids ]] && read -r s p cmd < <(ps -o etimes=,pid=,args= -p "$(paste -sd, <<< "$pids")" | sort -k1,1nr -k2,2n | head -1)
  cmd=${cmd//$REPO\//} s=${s:-0}; ((s < 3600)) && s="$((s / 60))m" || s="$((s / 3600))h$(printf %02d $((s % 3600 / 60)))m"
  printf 'held %s by %.100s%s\n' "$s" "${cmd:-?}" "${w:+, $(wc -w <<< "$w") waiting}"
}
# Take host $1's lock for this process and its children (LOCKFD stays open), unless a caller holds it already.
lock() {
  inherited "$1" && return
  exec {LOCKFD}> "$(lockfile "$1")"
  flock -n "$LOCKFD" && return
  echo "remote.sh: waiting for the $1 lock ($(lockstate "$1")). A quick look needs no lock: remote.sh look $1 ..." >&2
  flock "$LOCKFD"
}
# True when host $1's repo path resolves to itself (else tools print other paths) and its load is under half its cores.
quiet() {
  ssh -n "${QUICK[@]}" ${SSH_OPTS[$1]:-} "$1" "{ [ -x ~/.local/bin/zbook-paths ] || [ \"\$(realpath $REPO)\" = $REPO ]; } && awk -v n=\$(nproc) '{exit !(\$1 < n / 2)}' /proc/loadavg" 2> /dev/null
}
# Sets HOST to the first free, quiet host and holds its lock. Checks again every 30 s until one is free.
pick() {
  local h n=0
  while :; do
    for h in "${HOSTS[@]}"; do
      reserved "$h" && continue
      exec {LOCKFD}> "$(lockfile "$h")"
      if flock -n "$LOCKFD"; then
        quiet "$h" && { HOST=$h; echo "remote.sh: auto picked $h" >&2; return; }
        flock -u "$LOCKFD"
      fi
      exec {LOCKFD}>&-
    done
    ((n++)) || echo "remote.sh: no free, quiet host with a correct mirror in ${HOSTS[*]} (reserved: ${RESERVED[*]:-none}); checking every 30 s" >&2
    sleep 30
  done
}
status() {
  local h d; d=$(mktemp -d)
  for h in "$@"; do
    # The busiest process (average CPU since it started) when it uses 20% or more, not the probe itself.
    ssh "${QUICK[@]}" ${SSH_OPTS[$h]:-} "$h" bash -s > "$d/$h" 2> /dev/null <<'EOF' || echo unreachable > "$d/$h" &
read l _ < /proc/loadavg
m=$(awk '/MemAvailable/ { print int($2 / 1048576) }' /proc/meminfo)
t=$(ps -eo pcpu=,comm= --sort=-pcpu | grep -vm1 -E ' (ps|bash|sshd|sshd-session|grep)$' | awk '$1 >= 20 { print ", top", $2, $1 "%" }')
echo "load $l of $(nproc), ${m}G free$t"
EOF
  done
  wait
  for h in "$@"; do printf '%-10s %-44s | %s\n' "$h" "$(< "$d/$h")" "$(lockstate "$h")"; done
  rm -rf "$d"
}
sync_bins() {
  local h=$1 d; shift
  for d in "$@"; do
    d=$(abs "$d"); [[ -d $d ]] || die "not a dir: $d"; guard "$d"
    rs "$h" --exclude='*/' --exclude='*.d' --exclude='*.rlib' --exclude='.*' "$d/" "$h:$d/" || return
  done
}
# Tooling that changes between runs. Data (inputs, oracle caches, corpus cases, goldens) is mirrored once.
sync_scripts() {
  local h=$1 r=continuation-r97-goport f
  # .git is needed: gate.sh resolves --commit with git rev-parse. --delete only acts on the included paths.
  rs "$h" --delete --filter='- /.git/worktrees/' --filter='- __pycache__/' --filter='+ /.git/***' \
    --filter='+ /UPSTREAM.json' --filter='+ /scripts/***' --filter='+ /tools/***' --filter='+ /crates/' --filter='+ /crates/*/' \
    --filter='+ /crates/*/scripts/***' --filter='- *' "$REPO/" "$h:$REPO/" || return
  # Runner scripts under target/. lsp_oracle.py is only in the goport-int7 and goport-ls worktrees.
  cd "$T" || return
  for f in $r/{tools-port,sample-f1,emit,typesyms,typesyms/scale,build-mode,corpus-full,corpus-variants}/*.{py,sh} \
      $r/{corpus-int3,corpus-p5,emit-corpus,compat,compat/p5-corpus,compat/all-configs-p5,all-configs}/*.{py,sh} \
      $r/{cli-complete,tsgo-bin}/audit-r3/*.{py,sh} worktrees/goport-{int7,ls}/scripts/goport project-inputs-extra/sweep-extra2.sh; do
    [[ -e $f ]] && echo "$f"
  done | rs "$h" -r --exclude=__pycache__/ --files-from=- "$T/" "$h:$T/" || return
  # Legacy: compat/p5-corpus and typesyms/scale call /tmp/port/treehash.py. /tmp is tmpfs on alvin and cup2.
  # New tools go in scripts/, never /tmp (scripts/goport/tmp-port.sh restores /tmp/port on zbook).
  [[ ! -d /tmp/port ]] || rs "$h" --include='*.py' --include='*.sh' --include=gate-allow.txt --exclude='*' /tmp/port/ "$h:/tmp/port/"
}
# The pin's oracle, Go checkout, caches and UPSTREAM.json, over the same route as the other commands.
sync_pin() { RSYNC_RSH="ssh ${SSH_OPTS[$1]:-}" python3 "$REPO/scripts/upstream/pin.py" sync "$@"; }
push() {
  local h=$1 p; shift
  for p in "$@"; do
    p=$(abs "$p"); [[ -e $p ]] || die "no such file or dir: $p"; guard "$p"
    if [[ -d $p ]]; then rs "$h" "$p/" "$h:$p/"; else rs "$h" "$p" "$h:$p"; fi || return
  done
}
# The remote shell text that runs "$@" (host $1) in the repo root with zbook's paths and a login shell.
# A host whose home layout differs from zbook (alvin: ~/Code links to ~/code) runs through its
# ~/.local/bin/zbook-paths wrapper, a no-root mount namespace with zbook's paths and a private /tmp.
# dbook logs in as user dbook but has a real /home/theo dir, so HOME=/home/theo gives zbook's ~ paths.
wrap() {
  local h=$1 cmd; shift
  # GOPORT_PIN (scripts/upstream/pin.py) is passed on to the remote command.
  [[ -n ${GOPORT_PIN:-} ]] && set -- "export GOPORT_PIN=${GOPORT_PIN//[^0-9a-f]/};" "$@"
  printf -v cmd %q "$*"
  printf '%s' "cd $REPO || exit 2
    if [ -x ~/.local/bin/zbook-paths ]; then exec ~/.local/bin/zbook-paths bash -lc \"cd $REPO && \"$cmd; fi
    [ \"\$(pwd -P)\" = $REPO ] || { echo \"$h: $REPO resolves to \$(pwd -P); outputs would not match zbook\" >&2; exit 2; }
    exec env HOME=/home/theo bash -lc $cmd"
}
# A tty (when there is one) lets Ctrl-C stop the remote command too. ssh closes every inherited fd above 2 when
# it starts, so this shell stays alive (no exec) and holds the lock until ssh ends.
run() {
  local h=$1 t=(); shift
  [[ -t 0 && -t 1 ]] && t=(-t)
  lock "$h"
  ssh "${t[@]}" -o ServerAliveInterval=60 ${SSH_OPTS[$h]:-} "$h" "$(wrap "$h" "$@")"
}
look() { local h=$1; shift; exec timeout 120 ssh "${QUICK[@]}" ${SSH_OPTS[$h]:-} "$h" "$(wrap "$h" "$@")"; }
job() { lock "$1"; export REMOTE_HOST=$1; shift; "$@"; }
fetch() {
  local h=$1 d; shift
  for d in "$@"; do
    d=$(abs "$d"); guard "$d"
    case $d in "$T"/worktrees*) die "refusing to write into a worktree: $d" ;; "$T"/?*|/tmp/?*) ;; *) die "results live under $T or /tmp: $d" ;; esac
    rs "$h" --ignore-existing "$h:$d/" "$d/" || return
  done
}
usage() { sed -n '/^# usage:/,/^# dbook and the minis/p' "$0"; }
[[ $# -ge 1 ]] || { usage; exit 2; }
cmd=$1; shift
case $cmd in
  help|-h|--help) usage; exit ;;
  status) (($#)) || set -- "${HOSTS[@]}"; hs=(); for h; do hs+=("$(canon "$h")"); done; status "${hs[@]}"; exit ;;
  look|run|job|sync-bins|sync-scripts|sync-pin|push|fetch) ;;
  *) die "unknown command $cmd" ;;
esac
[[ $# -ge 1 ]] || die "$cmd needs a host"
# dbook and the minis are on the same LAN as zbook: always use the LAN route, never the Tailscale name.
host=$(canon "$1"); shift
# mini-abf9 is Theo's own machine again since 2026-10-04: no command may use it.
[[ $host == mini-abf9 ]] && die "mini-abf9 is Theo's machine since 2026-10-04. Do not use it. Time on mini-743d."
[[ $cmd == sync-scripts || $cmd == sync-pin || $# -ge 1 ]] || die "$cmd needs more arguments"
if [[ $host == auto ]]; then
  [[ $cmd == run || $cmd == job ]] || die "'auto' only works with run and job; inside a job, use \$REMOTE_HOST"
  # pick holds the lock of the host it picks, so run and job do not lock again.
  pick; host=$HOST
fi
if [[ ($cmd == run || $cmd == job) && ${REMOTE_USE_RESERVED:-0} != 1 ]] && reserved "$host"; then
  die "$host is kept for revision evidence (candidate.sh side). Use auto, or mini-743d for timing. The auditor and reviewer set REMOTE_USE_RESERVED=1 to repeat a revision run."
fi
if [[ $host == all ]]; then
  [[ $cmd == sync-* ]] || die "'all' only works with sync-bins, sync-scripts and sync-pin"
  pids=(); for h in "${HOSTS[@]}"; do "${cmd//-/_}" "$h" "$@" & pids+=($!); done
  rc=0; for p in "${pids[@]}"; do wait "$p" || rc=1; done; exit $rc
fi
"${cmd//-/_}" "$host" "$@"
