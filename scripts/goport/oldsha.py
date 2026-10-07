#!/usr/bin/env python3
"""Map commit SHAs from before the 2026-10-07 history rewrite to the new history.

usage: oldsha.py help
       oldsha.py <sha>...          print "old new" for each SHA (7 to 40 hex); "old ?" when it is not in the map
       oldsha.py text [FILE]       copy FILE (or stdin) to stdout with each 9-to-40-hex token that is an old
                                   commit in the map replaced by the new SHA of the same length

The map is target/repo-rewrite-2026-10-07/commit-map ("old new", full SHAs). It holds the commits of main,
checker-port and the kept goport-* lanes. Old SHAs that are not in it resolve only in the `archive` remote.
Tree hashes, evidence keys and sha256 values are not commits, so `text` leaves them as they are.
"""
import re
import sys
from pathlib import Path

MAP = Path(__file__).resolve().parents[2] / 'target/repo-rewrite-2026-10-07/commit-map'


def load():
    pairs = [line.split() for line in MAP.read_text().splitlines()[1:] if line.strip()]
    return {old: new for old, new in pairs}


def lookup(full, sha):
    hits = [old for old in full if old.startswith(sha)] if len(sha) < 40 else [sha] if sha in full else []
    return full[hits[0]][: len(sha)] if len(hits) == 1 else None


def main(argv):
    if not argv or argv[0] in ('help', '-h', '--help'):
        print(__doc__.strip())
        return 0
    full = load()
    if argv[0] == 'text':
        src = open(argv[1]).read() if len(argv) > 1 else sys.stdin.read()
        sys.stdout.write(re.sub(r'\b[0-9a-f]{9,40}\b', lambda m: lookup(full, m.group()) or m.group(), src))
        return 0
    for sha in argv:
        print(sha, lookup(full, sha.lower()) or '?')
    return 0


if __name__ == '__main__':
    sys.exit(main(sys.argv[1:]))
