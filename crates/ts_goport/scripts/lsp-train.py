#!/usr/bin/env python3
"""Editor sessions for the BOLT training of build-release.sh (perf samples them; PGO has no
editor sessions).

Runs `<tsgo> --lsp --stdio` through the long session of scripts/goport/ls_edit_bench.py (typing,
errfix, imports and mix edits, each followed by a VS Code-like request burst) on each named
project, one server at a time. The plan is ls_edit_bench's own (build_plan), so each run sends the
same messages. No Go server and no limits: this only makes the server do editor work.

usage: lsp-train.py <tsgo> <inputs-dir> <project>:<edits>...
  inputs-dir  the target/project-inputs dir that holds the project sources (read-only)
  project     query-core, hono or effect (ls_edit_bench.PROJECTS)

Prints one line per session (with ls_edit_bench's plan digest) and "lsp-train: ok" when every
server answered every round and exited 0. Else exits 1, and build-release.sh stops the BOLT
training.
"""

import os
import sys
import types

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "../../../scripts/goport"))
import ls_edit_bench as bench  # noqa: E402


def main():
    if len(sys.argv) < 4:
        print(__doc__, file=sys.stderr)
        return 2
    tsgo, inputs, specs = os.path.abspath(sys.argv[1]), os.path.abspath(sys.argv[2]), sys.argv[3:]
    # ls_edit_bench finds the inputs next to itself; a worktree has none.
    for p in bench.PROJECTS.values():
        p["root"] = p["root"].replace(bench.INPUTS, inputs, 1)
    cpus = ",".join(map(str, sorted(os.sched_getaffinity(0))))
    args = types.SimpleNamespace(cpus=cpus, rss_cap_mib=12288, timeout=600.0)
    failed = 0
    for spec in specs:
        project, edits = spec.split(":")
        rounds, digest = bench.build_plan(project, "long", int(edits))
        rec, _ = bench.run_session(args, project, rounds, tsgo, 0)
        ok = rec["error"] is None and rec["exit"] == 0 and len(rec["rows"]) == len(rounds)
        failed += not ok
        print(f"lsp-train: {project} long {edits} edits, plan {digest}: {len(rec['rows'])} of {len(rounds)} "
              f"rounds, exit {rec['exit']}{', ' + rec['error'] if rec['error'] else ''}", flush=True)
    if failed:
        print(f"lsp-train: {failed} sessions failed", flush=True)
        return 1
    print("lsp-train: ok", flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
