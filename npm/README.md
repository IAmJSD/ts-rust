# npm packages

Local npm packages for the port, in the layout of the Go packages at the pin, and the port's own
published set `tsc-rs` (see "tsc-rs releases" below).

- `typescript`: Go's JS launcher (`bin/tsc`, `lib/tsc.js`, `lib/getExePath.js`) and JS API (`dist`).
- `@typescript/typescript-linux-x64`: `lib/tsc` (a noembed build) and the lib files next to it.
- `wasm` (`ts-rust-wasm`): the WebAssembly build for Node, Deno, Bun and browsers. See
  [wasm/README.md](wasm/README.md).

The port adds one file to Go's layout: `install.js`, the postinstall (`lib/install.js`). On POSIX it
rewrites `bin/tsc` as a sh and JS polyglot. sh runs it as an exec of the platform package's
`lib/tsc` by a relative path, so `tsc` runs without Node, which saves about 20 ms on every run.
Node (`node node_modules/typescript/bin/tsc`) runs it as Go's JS launcher. The exec keeps the real
path of the binary in the platform package, where it reads the lib files. When the rewrite cannot
happen, `bin/tsc` stays Go's JS launcher, which still works.

The relative path is fixed at install. pnpm keeps a postinstall's result in its store (the
side-effects cache) and gives it to a later install of the same package in another layout
(hoisted, isolated or the global virtual store), where the path can name no file. Then `bin/tsc`
runs Go's JS launcher with Node, as Go's package does, about 20 ms slower per run.

Files:

- `pack.mjs` writes the package dirs. It follows the Go checkout's `Herebyfile.mjs`
  (`buildNativePreviewPackages`, release profile `typescript`).
- `install.js` is the postinstall.
- `getExePath.js` and `tsc-rs-readme.md` replace Go's launcher lookup and README in `tsc-rs`.

Build, test and time (see the header of each script):

```sh
GOPORT_BUILD_VERSION=7.1.0-dev.goport.1 scripts/run-cargo-capped.sh build --release -p ts_goport --bin tsgo --features noembed
GOPORT_PIN=<pin> scripts/goport/npm-pack.sh <out>/rs target/release/tsgo
GOPORT_PIN=<pin> scripts/goport/npm-pack.sh --go 7.1.0-dev.goport.1 <out>/go
scripts/goport/npm-test.sh <out>/rs <out>/test-rs
scripts/goport/perf-npm.sh <label> <out>/test-rs/proj <out>/test-go/proj   # on a quiet host
```

A shipped build uses `RELEASE_VERSION=<v> crates/ts_goport/scripts/build-release.sh` (noembed,
PGO and BOLT) in place of the cargo line.

## Linux libc

The platform package has no `libc` field, as Go's has none, so npm installs it on glibc and musl
systems alike. Go's tsc is static and starts on both. The default release tsc needs glibc 2.28 or
later: on Alpine, or on a distro with an older glibc, it does not start, and the JS launcher fails
too. `RELEASE_LIBC=musl RELEASE_PIE=0` in front of the build-release.sh line builds a static tsc
that starts on any x86-64 Linux. It is 0.5 to 1.9% slower (see the build-release.sh header), so it
is not the default.

## tsc-rs releases

`tsc-rs` is the same set under the port's own names: the main package `tsc-rs` (bin `tsc-rs`, so it
does not clash with the `tsc` of a `typescript` install) and one platform package per platform,
`@tsc-rs/linux-x64` and `@tsc-rs/darwin-arm64`. Its `lib/getExePath.js` finds
`@tsc-rs/<platform>-<arch>`.

The npm version (`--package-version`) is the port's own. The tsc is not stamped with it: it reports
the TypeScript version of the source (`7.1.0-dev`), because the compiler matches `typesVersions`
against that version. A tsc that reported 0.1.0 picked the `<=5.6` typings of zod's dependencies
and lost 2 of Go's 21 zod errors. The main package records the tsc version as `tscVersion` for the
postinstall check.

The release workflow (`.github/workflows/release.yml`) makes a release:

1. Push a tag `v<version>`, for example `git tag v0.1.0-preview.1 && git push origin v0.1.0-preview.1`.
   The npm version is the tag without the `v`.
2. It builds the tsc on Linux (static musl, non-PIE: it starts on any x86-64 Linux) and on a Mac
   (jemalloc does not cross-build for macOS with zig), from the same source.
3. It packs the set with `npm-pack.sh --name tsc-rs --package-version <v> --also darwin-arm64=<tsc>`,
   makes one archive per platform (the tsc, the lib files, LICENSE and NOTICE.txt) and installs
   the packages in a fresh project on each platform.
4. It publishes the platform packages, then `tsc-rs`, under the dist-tag `next`, and creates a
   GitHub prerelease with the archives, the .tgz files and `SHA256SUMS`. The publish uses npm
   trusted publishing (see below), so there is no npm token in the repo.

Pull requests that change the release files run steps 2 and 3 with the version `0.0.0-ci.<run>`.
After a check of `npx tsc-rs@next` on each platform, `npm dist-tag add tsc-rs@<v> latest` (and the
same for each platform package) makes it the default.

The CI builds are the plain shipped profile (fat LTO). They have no PGO and BOLT, which give 14 to
15% fewer cycles (build-release.sh header), because those need a host with BOLT and the project
inputs. For a faster build, make the Linux tsc on zbook with
`RELEASE_FEATURES=noembed RELEASE_LIBC=musl RELEASE_PIE=0 crates/ts_goport/scripts/build-release.sh <out>/linux-x64`
and pack and test it by hand:

```sh
GOPORT_PIN=<pin> scripts/goport/npm-pack.sh --name tsc-rs --package-version <v> \
  --also darwin-arm64=<darwin tsc from the release workflow> <out>/pkg <out>/linux-x64/bin/tsgo
scripts/goport/npm-test.sh --name tsc-rs <out>/pkg <out>/test
```

### Trusted publishing

npm trusts the workflow file `release.yml` of `pingdotgg/ts-rust`, in the GitHub environment
`npm`, to publish each of the 3 packages (OIDC, no token). Set it up once with
`npm/trust-setup.sh`, logged in to npm with 2FA and npm 11.15.0 or later. It publishes a
`0.0.0-placeholder` version of a package that is not on npm yet, because npm can only trust a
workflow for a package that exists. The workflow refuses `0.0.x` tags, so a release never
collides with a placeholder. npm drops a new trust that publishes nothing in 2 days, so run it
shortly before the first tag. Add required reviewers to the `npm` environment (repo settings,
Environments) to approve each publish by hand.

While the repo is private, npm publishes with no provenance. From a public repo it adds
provenance by itself.

The packages carry the port's MIT LICENSE and a NOTICE.txt with the licenses of TypeScript
(Apache-2.0) and Go (BSD-3-Clause), from `NOTICE.md` and `licenses/` at the repo root.
