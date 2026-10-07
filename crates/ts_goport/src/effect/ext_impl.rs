//! The `crate::ext::Extension` implementation of the Effect language service
//! (Go `etscheckerhooks`, `etslshooks`, `etsexecutehooks`; lane eff-ls fills
//! it). Every hook not written here keeps the plain tsgo default.

use super::Mode;
use crate::ext::Extension;

/// The Effect extension.
pub struct EffectExtension;

impl Extension for EffectExtension {
    // Effect patch 017. The LSP oracle compares the initialize answer, so the
    // kind is listed only with `TSGO_EFFECT=all`.
    fn advertises_refactor_rewrite(&self) -> bool {
        super::mode() == Mode::All
    }
}
