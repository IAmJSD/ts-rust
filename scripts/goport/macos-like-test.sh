#!/usr/bin/env bash
# Runs a test binary on Linux in a file system that acts like macOS's default volume, for
# tests that fail only on macOS. We have no Mac host; CI has one (.github/workflows/ci.yml).
#   - Case-insensitive: a tmpfs with casefold, as the root of a chroot. The port's probe
#     (frontend/vfs/osvfs.rs is_file_system_case_sensitive) stats the test binary's path with
#     its case swapped, so every dir of that path must be case-insensitive.
#   - The temp dir is /var/folders/x/T and /var is a symlink to /private/var, as on macOS.
# It needs no root: unshare makes a user and mount namespace, and the kernel needs tmpfs
# casefold (Linux 6.13 or later). /usr, /etc, /dev, /proc and /home (the checkout, for tests that
# read repo files) are bind mounts in the chroot.
# Known setup failure: go_baselines units_platform::osvfs::test_os asserts that Linux is
# case-sensitive (as Go's os_test.go), so it fails here and not on a Mac.
#
# usage: macos-like-test.sh <test-bin> [test args]...
#   <test-bin>   for example the ts_goport lib tests:
#                scripts/run-cargo-capped.sh test --release -p ts_goport --lib --no-run
#   test args    passed to the binary, for example a name filter
# One test thread (RUST_TEST_THREADS=1), as goport-tests.sh runs them.
set -euo pipefail
[[ $# -ge 1 && $1 != help ]] || { sed -n '2,18p' "$0" >&2; exit 2; }
bin=$(realpath "$1")
shift
mnt=$(mktemp -d "${TMPDIR:-/tmp}/macos-like.XXXXXX")
trap 'rmdir "$mnt"' EXIT
unshare -rm bash -s -- "$mnt" "$bin" "$@" << 'EOF'
set -euo pipefail
mnt=$1 bin=$2
shift 2
mount -t tmpfs -o casefold tmpfs "$mnt"
root=$mnt/root
mkdir "$root"
chattr +F "$root"
mkdir -p "$root"/{usr,etc,dev,proc,home,Bin} "$root/private/var/folders/x/T"
for d in usr etc dev proc home; do mount --rbind "/$d" "$root/$d"; done
ln -s usr/lib "$root/lib"
ln -s usr/lib "$root/lib64"
ln -s usr/bin "$root/bin"
ln -s private/var "$root/var"
cp "$bin" "$root/Bin/tests"
exec chroot "$root" /usr/bin/env TMPDIR=/var/folders/x/T RUST_TEST_THREADS=1 /Bin/tests "$@"
EOF
