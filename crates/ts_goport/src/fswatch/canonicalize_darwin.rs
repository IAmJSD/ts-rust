//! Go: internal/fswatch/canonicalize_darwin.go, and `isASCII`,
//! `normalizeNFC`, `nativePathFolding` and `foldNativePath` of
//! fsevents_darwin_ffi.go.
//!
//! PORT: Go builds these files on darwin (amd64 and arm64) only, and
//! `normalizeNFC` calls CoreFoundation (CFStringNormalize with
//! kCFStringNormalizationFormC). The port normalizes with the
//! `unicode-normalization` crate (Unicode NFC, safe code), a dependency on
//! Apple targets only. `normalize_nfc` builds there and in this crate's
//! tests, so its Go unit tests (fsevents_darwin_nfd_test.go, the `tests`
//! module below) run on Linux. `canonicalize_path` builds on darwin only,
//! and the other targets use canonicalize_other.rs.

use crate::fswatch::prelude::*;

#[cfg(any(target_vendor = "apple", test))]
use unicode_normalization::UnicodeNormalization;

#[cfg(all(
    any(target_os = "macos", target_os = "ios"),
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
use crate::fswatch::{pathcompare::PathComparer, pathkey::PathComparerExported};

// Go: canonicalize_darwin.go:15 canonicalizePath
/// canonicalizePath normalizes watch keys, subscribed filenames, and incoming
/// FSEvents paths to NFC. kqueue retains on-disk child spellings for its fd
/// bookkeeping and directory events; on case-insensitive volumes, the native
/// path comparer handles normalization differences when filtering WatchFile.
#[cfg(all(
    any(target_os = "macos", target_os = "ios"),
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
pub fn canonicalize_path(p: &str) -> String {
    normalize_nfc(p)
}

// Go: canonicalize_darwin.go:17 watcher.pathComparer (ts#64210)
#[cfg(all(
    any(target_os = "macos", target_os = "ios"),
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
impl WatcherStruct {
    pub fn path_comparer(&self, dir: &str) -> Result<PathComparer, GoError> {
        if self.name != "fsevents" && self.name != "kqueue" {
            return Ok(PathComparer::default());
        }
        let c = path_comparer_for_path(dir)?;
        Ok(c.comparer)
    }
}

// Go: canonicalize_darwin.go:27 PathComparerForPath (ts#64210)
/// PathComparerForPath queries an existing path's volume. Errors are returned to
/// the caller; a failed query must not silently enable or disable native folding.
// PORT: Go reads `unix.Pathconf(path, _PC_CASE_SENSITIVE)` and ignores case
// on a volume that reports 0. The port has no safe `pathconf` (D-W1: no
// `libc` or `unsafe`, and rustix has none), so it returns exact comparison,
// the behavior before ts#64210.
#[cfg(all(
    any(target_os = "macos", target_os = "ios"),
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
pub fn path_comparer_for_path(_path: &str) -> Result<PathComparerExported, GoError> {
    Ok(PathComparerExported::default())
}

// Go: fsevents_darwin_ffi.go:195 nativePathFolding (ts#64210)
// PORT: Go folds with CoreFoundation `CFStringFold`. The port calls no
// CoreFoundation (D-W1), so it has no native fold and takes Go's non-native
// branches (the simple Unicode fold of pathSuffixFoldUnicode).
#[cfg(all(
    any(target_os = "macos", target_os = "ios"),
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
pub const NATIVE_PATH_FOLDING: bool = false;

// Go: fsevents_darwin_ffi.go:200 foldNativePath (ts#64210)
// PORT: not reached, because `NATIVE_PATH_FOLDING` is false (see there).
#[cfg(all(
    any(target_os = "macos", target_os = "ios"),
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
pub fn fold_native_path(_: &str) -> String {
    panic!("fswatch: native path folding is not ported (CoreFoundation)");
}

// Go: fsevents_darwin_ffi.go:262 isASCII
/// isASCII reports whether every byte in s is below 0x80. Pure-ASCII paths
/// are identical in every Unicode normalization form, so we can skip the
/// CoreFoundation round-trip entirely, which is the overwhelming common case.
pub fn is_ascii(s: &str) -> bool {
    s.bytes().all(|b| b < 0x80)
}

// Go: fsevents_darwin_ffi.go:319 normalizeNFC
/// normalizeNFC returns s in Unicode NFC (canonical composed) form. ASCII
/// inputs are returned unchanged. Non-ASCII inputs go through CoreFoundation;
/// if any step fails (e.g. invalid UTF-8 from a corrupt path), the original
/// string is returned so the caller still sees *something* rather than nothing.
///
/// PORT: `s` is in the port form of a Go string (`scanner_util`). Its Go
/// bytes are normalized; when they are not UTF-8, CFStringCreate fails in
/// Go and `s` is returned unchanged, as here.
#[cfg(any(target_vendor = "apple", test))]
pub fn normalize_nfc(s: &str) -> String {
    if is_ascii(s) {
        return s.to_string();
    }
    let bytes = crate::scanner_util::go_string_bytes(s);
    let Ok(text) = std::str::from_utf8(&bytes) else {
        return s.to_string();
    };
    crate::scanner_util::go_string_from_utf8(text.nfc().collect())
}

// Go: internal/fswatch/fsevents_darwin_nfd_test.go, the three tests of the
// NFC helpers (`TestNormalizeNFC`, `TestNormalizeNFCASCIIFastPath`,
// `TestIsASCII`).
// PORT: Go builds these tests on darwin only, because `normalizeNFC` calls
// CoreFoundation there. Here they run on every target. Go `t.Run` subtests
// are the assert messages. The other tests of the file watch FSEvents and
// are not ported (the FSEvents backend is not).
#[cfg(test)]
mod tests {
    use super::*;

    // "é"
    // Go: fsevents_darwin_nfd_test.go:26 nfcE, nfdE
    const NFC_E: &str = "\u{00e9}"; // U+00E9
    const NFD_E: &str = "e\u{0301}"; // U+0065 U+0301

    // Go: fsevents_darwin_nfd_test.go:33 TestNormalizeNFC
    // TestNormalizeNFC exercises the CoreFoundation-backed normalizer directly
    // (without going through FSEvents) so a regression in the FFI plumbing is
    // caught even if the end-to-end FSEvents tests are skipped.
    #[test]
    fn test_normalize_nfc() {
        // Latin combining marks (BMP, one combining mark per base).
        let nfc_cafe = format!("caf{NFC_E}");
        let nfd_cafe = format!("caf{NFD_E}");
        // Hangul: composition is algorithmic, not table-driven.
        // "한" (U+D55C) decomposes to ᄒ ᅡ ᆫ (U+1112 U+1161 U+11AB).
        let nfc_han = "\u{D55C}";
        let nfd_han = "\u{1112}\u{1161}\u{11AB}";
        // Multi-codepoint compose: "ệ" (U+1EC7) ⇄ "ệ" (also valid as
        // ệ due to canonical ordering; CFStringNormalize handles both).
        let nfc_e_hook = "\u{1EC7}";
        let nfd_e_hook = "e\u{0323}\u{0302}";

        let tests: Vec<(&str, String, String)> = vec![
            ("empty", String::new(), String::new()),
            (
                "ascii",
                "/var/folders/abc/hello.txt".into(),
                "/var/folders/abc/hello.txt".into(),
            ),
            (
                "ascii-only-high-bit-edge",
                "/\x7f/path".into(),
                "/\x7f/path".into(),
            ),
            ("already-NFC-latin", nfc_cafe.clone(), nfc_cafe.clone()),
            ("NFD-to-NFC-latin", nfd_cafe.clone(), nfc_cafe.clone()),
            ("already-NFC-hangul", nfc_han.into(), nfc_han.into()),
            ("NFD-to-NFC-hangul", nfd_han.into(), nfc_han.into()),
            (
                "already-NFC-multi-mark",
                nfc_e_hook.into(),
                nfc_e_hook.into(),
            ),
            (
                "NFD-to-NFC-multi-mark",
                nfd_e_hook.into(),
                nfc_e_hook.into(),
            ),
            (
                "mixed-ascii-and-NFD",
                format!("/tmp/{nfd_cafe}/file.txt"),
                format!("/tmp/{nfc_cafe}/file.txt"),
            ),
            (
                "non-bmp-passthrough",
                "/tmp/\u{1F600}.txt".into(),
                "/tmp/\u{1F600}.txt".into(),
            ),
        ];

        for (name, input, want) in &tests {
            assert_eq!(&normalize_nfc(input), want, "{name}");
        }
    }

    // Go: fsevents_darwin_nfd_test.go:82 TestNormalizeNFCASCIIFastPath
    // TestNormalizeNFCASCIIFastPath verifies the ASCII fast path returns the
    // input unchanged with no Unicode round-trip.
    #[test]
    fn test_normalize_nfc_ascii_fast_path() {
        let input = "/var/folders/abc/def/hello.txt";
        let out = normalize_nfc(input);
        assert_eq!(out, input, "ascii input mutated");
    }

    // Go: fsevents_darwin_nfd_test.go:92 TestIsASCII
    // PORT: Go's "\x80" is one byte that is not UTF-8; the Rust case is U+0080
    // (bytes C2 80, also not ASCII). Go's last case, the bytes C2 A9, is "a©".
    #[test]
    fn test_is_ascii() {
        let tests: [(&str, bool); 8] = [
            ("", true),
            ("hello", true),
            ("/tmp/file.txt", true),
            ("\x7f", true),          // DEL is the last ASCII byte
            ("\u{80}", false),       // first non-ASCII byte
            ("caf\u{00e9}", false),  // NFC é
            ("cafe\u{0301}", false), // NFD é (combining mark is also non-ASCII)
            ("a\u{00A9}", false),    // © (U+00A9)
        ];
        for (input, want) in tests {
            assert_eq!(is_ascii(input), want, "isASCII({input:?})");
        }
    }
}
