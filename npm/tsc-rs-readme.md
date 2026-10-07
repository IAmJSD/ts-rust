# tsc-rs

A Rust port of the TypeScript 7 compiler (`tsc`).

It is a direct port of Microsoft's native TypeScript compiler, which is written in Go. On the
projects we test, it gives the same diagnostics and output as that compiler, and it is faster.

This is a preview. Report problems at https://github.com/pingdotgg/ts-rust/issues.

## Use

```sh
npm install -D tsc-rs@next
npx tsc-rs -p tsconfig.json
```

`tsc-rs` takes the same options as `tsc`. `tsc-rs --version` prints the TypeScript version that it
ports (7.1.0-dev), not the npm version.

## VS Code

The TypeScript 7 extension looks for the `typescript` package, so it does not find `tsc-rs` by
itself. Point it at the dir of the `tsc` in the platform package, in `.vscode/settings.json`, and
allow it when VS Code asks:

```json
{ "js/ts.tsdk.path": "node_modules/@tsc-rs/linux-x64/lib" }
```

On macOS, use `@tsc-rs/darwin-arm64`.

## Platforms

- Linux x64 (static, any distribution)
- macOS arm64

Windows and Linux arm64 are not available yet.

## Known problems

- In some monorepos, the source files of a workspace package are reachable both through
  `node_modules` and through a direct import. There, `tsc-rs` can write output for more of those
  files than `tsc` does. (The set from `tsc` also changes from run to run there.)
- In `tsc -b`, when one project imports the output of another project without a project reference,
  `tsc-rs` can report TS2307 (cannot find module) where `tsc` happens to build the other project
  first. Add the reference to fix it.
- `tsc -b --watch` can stop with an internal error (exit code 70) after some edits.
- In the editor, memory grows slowly during long edit sessions.

## License

MIT. The port keeps the licenses and notices of TypeScript (Apache-2.0, Copyright (c) Microsoft
Corporation) and Go (BSD-3-Clause). See NOTICE.txt.
