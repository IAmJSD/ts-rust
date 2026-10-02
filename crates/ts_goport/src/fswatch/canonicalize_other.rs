//! Go: internal/fswatch/canonicalize_other.go.
//!
//! PORT: Go builds this file on every platform except darwin (amd64 and
//! arm64), which uses canonicalize_darwin.go (NFC path normalization); so
//! does the port (canonicalize_darwin.rs).

use crate::fswatch::prelude::*;

use crate::fswatch::pathcompare::PathComparer;
use crate::fswatch::pathkey::PathComparerExported;

// Go: canonicalize_other.go:5 nativePathFolding (ts#64210)
pub const NATIVE_PATH_FOLDING: bool = false;

// Go: canonicalize_other.go:7 foldNativePath (ts#64210)
pub fn fold_native_path(_: &str) -> String {
    panic!("fswatch: native path folding is only available on Darwin");
}

// Go: canonicalize_other.go:14 canonicalizePath
/// canonicalizePath is a no-op on platforms whose watchers report paths
/// using the same bytes the caller provided. See canonicalize_darwin.go
/// for the rationale on macOS.
pub fn canonicalize_path(p: &str) -> String {
    p.to_string()
}

impl WatcherStruct {
    // Go: canonicalize_other.go:16 watcher.pathComparer (ts#64210)
    pub fn path_comparer(&self, _dir: &str) -> Result<PathComparer, GoError> {
        Ok(PathComparer::default())
    }
}

// Go: canonicalize_other.go:22 PathComparerForPath (ts#64210)
/// PathComparerForPath returns exact comparison on platforms without native
/// Darwin watch aliases. It does not inspect the host filesystem.
pub fn path_comparer_for_path(_path: &str) -> Result<PathComparerExported, GoError> {
    Ok(PathComparerExported::default())
}
