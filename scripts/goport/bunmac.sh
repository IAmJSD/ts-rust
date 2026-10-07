#!/usr/bin/env bash
# bunmac: one timing run of Bun check, tsc-rs and tsgo on T3 Code, made for a Mac (lane macbench1, plan
# studies/bunperf1/plan.md section 2.3). One file with no repo dependencies: copy it to the Mac and run it
# with bash (macOS /bin/bash 3.2 works). Linux works too, for dry runs (GNU time -v in place of time -l).
#
# usage: bunmac.sh run <t3code> <bun> <tsc-rs> <tsgo> [<out>]
#   <t3code>  a T3 Code checkout with its deps installed. Read only: the script checks a copy in
#             <out>/t3.noindex (the files without .git, each node_modules and .repos a symlink to the
#             checkout's; Spotlight skips a .noindex dir), and at the end lists the checkout files that changed
#             during the run (checkout-changes.txt).
#   <bun>     a bun that has `bun check` (path or command name).
#   <tsc-rs>  the native tsc of tsc-rs 0.1.0, not the launcher in .bin: node_modules/@tsc-rs/darwin-arm64/lib/tsc
#   <tsgo>    the native tsc of typescript 7.0.2: node_modules/@typescript/typescript-darwin-arm64/lib/tsc
#             Get both with: npm i --prefix ~/bunmac-tools tsc-rs@0.1.0 typescript@7.0.2
#   <out>     a new results dir (default ~/bunmac-<host>-<date>), not inside <t3code>.
# Needs hyperfine (brew install hyperfine). With samply on PATH, it also records one tsc-rs profile.
#
# Steps: system and tool facts (env.txt); the copy and its no-Effect configs (the t3run method of
# studies/bunperf1/t3.md: tsconfig.base.noeffect.json is the base without plugins, each workspace gets
# tsconfig.noeffect.json); per workspace hyperfine -N -w1 -r10 of the 6 commands below, then one run of each
# under /usr/bin/time -l (RSS, page faults) with --extendedDiagnostics (bun: --timing); a samply profile of
# tsc-rs on the first workspace; summary.md (one table); <out>.tar.gz to send back (without the copy).
# Commands, each in the workspace dir, with the build info removed before each run (apps/web is composite):
#   bun             bun check -p tsconfig.noeffect.json --no-pretty --all
#   tscrs, tsgo     <tsc> -p tsconfig.noeffect.json --noEmit --pretty false
#   tscrs-checkers8 tscrs with --checkers 8
#   tscrs-parse4    tscrs with GOPORT_PARSE_THREADS=4 (4 parse workers; Bun reads with 4 threads on macOS)
#   tscrs-jemalloc  tscrs with _RJEM_MALLOC_CONF=$JEMALLOC_AB (the Mac build has no built-in jemalloc config)
# About 10 minutes on the Mac of Theo's chart (tsgo is a third of it). On macOS, caffeinate keeps it awake.
# env: RUNS (10), WARMUP (1), WORKSPACES ("apps/server apps/web apps/mobile packages/client-runtime
#      packages/shared"), JEMALLOC_AB (narenas:4,metadata_thp:disabled,cache_oblivious:false: the Linux
#      JEMALLOC_CONF of bin/tsgo.rs without thp:always, which macOS lacks), SAMPLY (1; 0 skips the profile).
#
#        bunmac.sh help
set -euo pipefail

RUNS=${RUNS:-10}
WARMUP=${WARMUP:-1}
WORKSPACES=${WORKSPACES:-apps/server apps/web apps/mobile packages/client-runtime packages/shared}
JEMALLOC_AB=${JEMALLOC_AB:-narenas:4,metadata_thp:disabled,cache_oblivious:false}
SAMPLY=${SAMPLY:-1}
NAMES="bun tscrs tscrs-checkers8 tscrs-parse4 tscrs-jemalloc tsgo"
# Linux only: tsc-rs then does the work in the process that wait4 sees (no launcher on macOS).
export GOPORT_LAUNCH=0

die() { echo "bunmac: $*" >&2; exit 1; }

# native <what> <path or command>: the absolute path; stops on a script (a Node or sh launcher adds 20 to 40 ms).
native() {
  local p=$2
  [[ $p == */* ]] || p=$(command -v "$p") || die "$1: $2 not found"
  [[ -f $p && -x $p ]] || die "$1: $p is not an executable file"
  case $(head -c 4 "$p" | od -An -tx1 | tr -d ' \n') in
    cffaedfe | cafebabe | 7f454c46) ;;
    *) die "$1: $p is a script. Pass the native binary (see bunmac.sh help)" ;;
  esac
  echo "$(cd "$(dirname "$p")" && pwd -P)/$(basename "$p")"
}

# cmd <name>: sets CMD to the argv of one check of the workspace in the current dir.
cmd() {
  local tsc=(-p tsconfig.noeffect.json --noEmit --pretty false)
  case $1 in
    bun) CMD=("$BUN" check -p tsconfig.noeffect.json --no-pretty --all) ;;
    tsgo) CMD=("$TSGO" "${tsc[@]}") ;;
    tscrs) CMD=("$TSCRS" "${tsc[@]}") ;;
    tscrs-checkers8) CMD=("$TSCRS" "${tsc[@]}" --checkers 8) ;;
    tscrs-parse4) CMD=(env GOPORT_PARSE_THREADS=4 "$TSCRS" "${tsc[@]}") ;;
    tscrs-jemalloc) CMD=(env "_RJEM_MALLOC_CONF=$JEMALLOC_AB" "$TSCRS" "${tsc[@]}") ;;
  esac
}

facts() {
  echo "date $(date -u +%Y-%m-%dT%H:%M:%SZ) host $(hostname -s) out $OUT"
  uname -a
  if [[ $OS == Darwin ]]; then
    sw_vers
    sysctl hw.model machdep.cpu.brand_string hw.ncpu hw.physicalcpu hw.perflevel0.physicalcpu \
      hw.perflevel1.physicalcpu hw.memsize hw.pagesize vm.loadavg
    pmset -g batt | head -2
    pmset -g | grep -i powermode
  else
    grep -m1 'model name' /proc/cpuinfo; nproc; free -g | head -2; cat /proc/loadavg
  fi
  echo "bun $("$BUN" --revision)"; echo "tscrs $("$TSCRS" --version)"; echo "tsgo $("$TSGO" --version)"
  hyperfine --version; command -v samply > /dev/null && samply --version
  shasum -a 256 "$BUN" "$TSCRS" "$TSGO" 2> /dev/null || sha256sum "$BUN" "$TSCRS" "$TSGO"
  echo "t3code $T3 head $(git -C "$T3" rev-parse HEAD) changed files $(GIT_OPTIONAL_LOCKS=0 git -C "$T3" status --porcelain | wc -l | tr -d ' ')"
  local s=stats_print:true,stats_print_opts:mdablxe
  echo "jemalloc default: $(_RJEM_MALLOC_CONF=$s "$TSCRS" --version 2>&1 | grep -E 'opt\.(narenas|cache_oblivious)' | tr -s ' \n' ' ')"
  echo "jemalloc A/B: $(_RJEM_MALLOC_CONF=$JEMALLOC_AB,$s "$TSCRS" --version 2>&1 | grep -E 'opt\.(narenas|cache_oblivious)' | tr -s ' \n' ' ')"
}

# The no-Effect configs (t3run method). Bun runs this JS: it is the one JS runtime the run surely has.
CONFIGS_JS='
const fs = require("node:fs"), path = require("node:path");
const W = process.env.BUNMAC_W, C = process.env.BUNMAC_OUT + "/configs";
const read = (f) => Function("\"use strict\";return (" + fs.readFileSync(f, "utf8").replace(/^﻿/, "") + "\n)")();
const write = (f, o) => { const s = JSON.stringify(o, null, 2) + "\n"; fs.writeFileSync(f, s);
  fs.writeFileSync(path.join(C, path.relative(W, f).replaceAll("/", "_")), s); };
fs.mkdirSync(C);
const base = path.join(W, "tsconfig.base.json"), noeffect = path.join(W, "tsconfig.base.noeffect.json");
if (fs.existsSync(base)) { const b = read(base); if (b.compilerOptions) delete b.compilerOptions.plugins; write(noeffect, b); }
for (const ws of process.env.BUNMAC_WS.split(" ").filter(Boolean)) {
  const dir = path.join(W, ws), c = read(path.join(dir, "tsconfig.json"));
  let ext = typeof c.extends === "string" && c.extends.startsWith(".") ? path.resolve(dir, c.extends) : "";
  if (ext && !ext.endsWith(".json")) ext += ".json";
  let n;
  if (ext === base) { c.extends = path.relative(dir, noeffect); if (c.compilerOptions) delete c.compilerOptions.plugins; n = c; }
  else if (!c.compilerOptions?.plugins) n = { extends: "./tsconfig.json" };
  else throw new Error(ws + "/tsconfig.json has plugins and does not extend tsconfig.base.json");
  write(path.join(dir, "tsconfig.noeffect.json"), n);
}'

# The table of summary.md, from hf/<ws>.json and detail/<ws>.<name>.{out,err,rc}.
SUMMARY_JS='
const fs = require("node:fs");
const O = process.env.BUNMAC_OUT, names = process.env.BUNMAC_NAMES.split(" ");
const rd = (f) => { try { return fs.readFileSync(f, "utf8"); } catch { return ""; } };
const num = (s, re) => { const m = s.match(re); return m ? Number(m[1]) * (m[2] === "s" ? 1000 : 1) : NaN; };
const f = (x, d) => (Number.isFinite(x) ? x.toFixed(d) : "-");
const out = ["| workspace | command | median s | sd s | x bun | load ms | check ms | rest ms | RSS MB | page reclaims | exit | errors |",
  "|---|---|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|"], notes = [];
for (const ws of process.env.BUNMAC_WS.split(" ").filter(Boolean)) {
  const b = ws.split("/").pop();
  let hf = []; try { hf = JSON.parse(rd(O + "/hf/" + b + ".json")).results; } catch {}
  const bun = hf.find((r) => r.command === "bun");
  for (const n of names) {
    const r = hf.find((x) => x.command === n) || {};
    // Bun prints its --timing line to stderr.
    const so = rd(O + "/detail/" + b + "." + n + ".out"), se = rd(O + "/detail/" + b + "." + n + ".err");
    // rest: the wall time of the run (time -l: 10 ms steps) less the total that the tool reports: start, exit, unmap.
    let wall = num(se, /^\s*([\d.]+) real/m) * 1000;
    const gnu = se.match(/Elapsed \(wall clock\) time \(h:mm:ss or m:ss\): (?:(\d+):)?(\d+):([\d.]+)/);
    if (gnu) wall = ((Number(gnu[1] || 0) * 60 + Number(gnu[2])) * 60 + Number(gnu[3])) * 1000;
    const total = n === "bun" ? num(so + "\n" + se, /files? \[([\d.]+)(ms|s)\]/) : num(so, /^Total time:\s+([\d.]+)(s)$/m);
    // macOS time -l gives bytes and "page reclaims"; GNU time -v gives kbytes and minor faults.
    let rss = num(se, /^\s*(\d+)\s+maximum resident set size/m) / 2 ** 20;
    if (!Number.isFinite(rss)) rss = num(se, /Maximum resident set size \(kbytes\): (\d+)/) / 1024;
    let pf = num(se, /^\s*(\d+)\s+page reclaims/m);
    if (!Number.isFinite(pf)) pf = num(se, /Minor \(reclaiming a frame\) page faults: (\d+)/);
    const load = n === "bun" ? num(se, /files loaded in ([\d.]+)(ms|s)\b/)
      : num(so, /^Parse time:\s+([\d.]+)(s)$/m) + num(so, /^Bind time:\s+([\d.]+)(s)$/m);
    const check = n === "bun" ? num(se, /checked in ([\d.]+)(ms|s)\b/) : num(so, /^Check time:\s+([\d.]+)(s)$/m);
    const errors = (so.match(/error TS\d+:/g) || []).length, effect = (so.match(/TS377\d{3}/g) || []).length;
    const rc = rd(O + "/detail/" + b + "." + n + ".rc").trim();
    const bad = (r.exit_codes || []).filter((c) => c !== 0).length;
    if (effect) notes.push(ws + " " + n + ": " + effect + " Effect diagnostics (TS377xxx): the no-Effect config did not work");
    if (bad) notes.push(ws + " " + n + ": exit code not 0 in " + bad + " of " + r.exit_codes.length + " timed runs");
    out.push(["", ws, n, f(r.median, 3), f(r.stddev, 3), f(r.median / (bun && bun.median), 2), f(load, 0), f(check, 0),
      f(wall - total, 0), f(rss, 0), f(pf, 0), rc || "-", errors, ""].join(" | ").trim());
  }
}
console.log(out.concat(notes.length ? ["", ...notes.map((x) => "- " + x)] : []).join("\n"));'

run() {
  [[ $# -ge 4 && $# -le 5 ]] || die "usage: bunmac.sh run <t3code> <bun> <tsc-rs> <tsgo> [<out>] (bunmac.sh help)"
  OS=$(uname -s)
  T3=$(cd "$1" && pwd -P) || die "no dir $1"
  BUN=$(native bun "$2"); TSCRS=$(native tsc-rs "$3"); TSGO=$(native tsgo "$4")
  command -v hyperfine > /dev/null || die "hyperfine not found (brew install hyperfine)"
  [[ -d $T3/node_modules ]] || die "$T3 has no node_modules: install its deps first"
  local ws b n
  for ws in $WORKSPACES; do [[ -f $T3/$ws/tsconfig.json ]] || die "no $T3/$ws/tsconfig.json"; done
  OUT=${5:-$HOME/bunmac-$(hostname -s)-$(date +%Y%m%d-%H%M%S)}
  [[ ! -e $OUT ]] || die "$OUT exists"
  b=$(cd "$(dirname "$OUT")" && pwd -P) || die "no parent dir of $OUT"
  OUT=$b/$(basename "$OUT")
  case $OUT/ in "$T3"/*) die "$OUT is inside the checkout" ;; esac
  mkdir "$OUT" "$OUT/hf" "$OUT/detail"
  touch "$OUT/.start"
  # In an empty dir: an old bun runs a package.json script named "check".
  (cd "$OUT" && "$BUN" check --help > /dev/null 2>&1) || die "$BUN has no check command"
  (set +e; facts) > "$OUT/env.txt" 2>&1
  echo "bunmac: results in $OUT"
  if [[ $OS == Darwin ]]; then caffeinate -i -w $$ & fi

  local W=$OUT/t3.noindex
  mkdir "$W"
  (cd "$T3" && COPYFILE_DISABLE=1 tar -cf - --exclude node_modules --exclude .git --exclude .repos .) | (cd "$W" && tar -xf -)
  (cd "$T3" && find . \( -name .git -o -name .repos \) -prune -o -name node_modules -prune -print) |
    while IFS= read -r n; do ln -s "$T3/${n#./}" "$W/${n#./}"; done
  [[ ! -e $T3/.repos ]] || ln -s "$T3/.repos" "$W/.repos"
  (cd "$OUT" && BUNMAC_W=$W BUNMAC_OUT=$OUT BUNMAC_WS=$WORKSPACES "$BUN" -e "$CONFIGS_JS")

  local hf=(hyperfine -N -i -w "$WARMUP" -r "$RUNS" --prepare "rm -f tsconfig.noeffect.tsbuildinfo")
  local tflag=-l a s
  [[ $OS == Darwin ]] || tflag=-v
  for ws in $WORKSPACES; do
    b=$(basename "$ws")
    echo "bunmac: $ws"
    local args=()
    for n in $NAMES; do
      cmd "$n"; s=""
      for a in "${CMD[@]}"; do s="$s '$a'"; done
      args+=(-n "$n" "${s# }")
    done
    (cd "$W/$ws" && "${hf[@]}" --export-json "$OUT/hf/$b.json" --export-markdown "$OUT/hf/$b.md" "${args[@]}")
    for n in $NAMES; do
      cmd "$n"; a=--extendedDiagnostics
      [[ $n != bun ]] || a=--timing
      rm -f "$W/$ws/tsconfig.noeffect.tsbuildinfo"
      s=0
      (cd "$W/$ws" && /usr/bin/time "$tflag" "${CMD[@]}" "$a" > "$OUT/detail/$b.$n.out" 2> "$OUT/detail/$b.$n.err") || s=$?
      echo "$s" > "$OUT/detail/$b.$n.rc"
    done
  done

  ws=${WORKSPACES%% *}
  if [[ $SAMPLY != 0 ]] && command -v samply > /dev/null; then
    echo "bunmac: samply, tscrs on $ws"
    # --unstable-presymbolicate writes the symbols next to the profile, for a viewer on another machine.
    a=(--save-only)
    if samply record --help 2>&1 | grep presymbolicate > /dev/null; then a+=(--unstable-presymbolicate); fi
    rm -f "$W/$ws/tsconfig.noeffect.tsbuildinfo"
    cmd tscrs
    (cd "$W/$ws" && samply record "${a[@]}" -o "$OUT/samply-$(basename "$ws")-tscrs.json.gz" -- "${CMD[@]}" > /dev/null) ||
      echo "bunmac: samply failed" | tee -a "$OUT/env.txt"
  fi

  (cd "$T3" && find . -name .git -prune -o -newer "$OUT/.start" -print) > "$OUT/checkout-changes.txt"
  n=$(wc -l < "$OUT/checkout-changes.txt" | tr -d ' ')
  {
    echo "# bunmac: T3 Code on $(hostname -s), $(date +%Y-%m-%d)"
    echo
    if [[ $OS == Darwin ]]; then
      echo "- $(sysctl -n machdep.cpu.brand_string), hw.ncpu $(sysctl -n hw.ncpu)" \
        "(P $(sysctl -n hw.perflevel0.physicalcpu 2> /dev/null), E $(sysctl -n hw.perflevel1.physicalcpu 2> /dev/null))," \
        "hw.memsize $(($(sysctl -n hw.memsize) / 1073741824)) GB, macOS $(sw_vers -productVersion) ($(sw_vers -buildVersion))"
    else
      echo "- $(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2- | sed 's/^ //'), $(nproc) CPUs, $(uname -sr)"
    fi
    echo "- bun $("$BUN" --revision), tsc-rs $("$TSCRS" --version), tsgo $("$TSGO" --version); T3 Code $(git -C "$T3" rev-parse --short HEAD)"
    echo "- hyperfine -N -w$WARMUP -r$RUNS, wall s. Load, check, rest, RSS, page reclaims, exit and errors: one more run under"
    echo "  /usr/bin/time $tflag with --extendedDiagnostics (bun: --timing). rest = that run's wall less the tool's own total"
    echo "  (start, exit, unmap). tscrs-jemalloc: _RJEM_MALLOC_CONF=$JEMALLOC_AB"
    echo "- checkout files changed during the run: $n (checkout-changes.txt)"
    echo
    (cd "$OUT" && BUNMAC_OUT=$OUT BUNMAC_WS=$WORKSPACES BUNMAC_NAMES=$NAMES "$BUN" -e "$SUMMARY_JS")
  } > "$OUT/summary.md"
  COPYFILE_DISABLE=1 tar -czf "$OUT.tar.gz" -C "$(dirname "$OUT")" --exclude "$(basename "$OUT")/t3.noindex" "$(basename "$OUT")"
  cat "$OUT/summary.md"
  echo
  echo "bunmac: send $OUT.tar.gz"
}

case ${1:-help} in
  run) shift; run "$@" ;;
  -h | --help | help) sed -n '2,/^set -euo/p' "$0" | sed '$d' ;;
  *) die "unknown command $1 (bunmac.sh help)" ;;
esac
