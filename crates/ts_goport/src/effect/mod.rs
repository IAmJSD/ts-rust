//! The Effect language service: the port of Effect-TS/tsgo (`@effect/tsgo`,
//! Go code in `internal/` and `ets*`), built with the `effect` feature
//! (default on). This module uses only `crate::ext` and the public checker
//! and LS APIs, so it can move to its own crate later.
//!
//! Activation: `install` puts `EffectExtension` in `crate::ext` unless
//! `TSGO_EFFECT=off`. Its hooks act only for a project whose tsconfig lists
//! the `@effect/language-service` plugin. With `TSGO_EFFECT=editor` (the
//! default) they act in the LSP and the API language service paths, and
//! tsc and the API answers stay plain tsgo. `TSGO_EFFECT=all` also turns
//! them on in tsc and the API answers, and lists `refactor.rewrite` in the
//! LSP code action kinds (Effect patch 017).

pub mod directives;
pub mod ext_impl;
pub mod fixable;
pub mod fixables;
pub mod messages;
pub mod options;
pub mod rule;
pub mod rules;
pub mod runner;
pub mod typeparser;

use std::sync::OnceLock;

/// The `TSGO_EFFECT` switch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// `off`: no extension is installed; every output is plain tsgo.
    Off,
    /// `editor` (default, also for an unset or unknown value): the LSP and
    /// the API language service paths.
    Editor,
    /// `all`: also tsc and the API answers.
    All,
}

/// The `TSGO_EFFECT` switch, read once per process.
pub fn mode() -> Mode {
    static MODE: OnceLock<Mode> = OnceLock::new();
    *MODE.get_or_init(|| match std::env::var("TSGO_EFFECT").as_deref() {
        Ok("off") => Mode::Off,
        Ok("all") => Mode::All,
        _ => Mode::Editor,
    })
}

/// Installs the Effect extension (Go: the `init()` of the `ets*hooks`
/// packages that `cmd/tsc/main.go` imports, Effect patch 001), unless
/// `TSGO_EFFECT=off`.
pub fn install() {
    if mode() != Mode::Off {
        crate::ext::install(&ext_impl::EffectExtension);
    }
}
