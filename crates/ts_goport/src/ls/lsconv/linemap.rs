//! Port of Go `ls/lsconv/linemap.go`.

use crate::ls::lsconv::prelude::*;

use crate::frontend::scanner::scanner_p1::{RUNE_SELF, utf8_decode_rune_in_string};
use crate::gostd;
use crate::scanner_util::contains_go_string_marker;

// Go: ls/lsconv/linemap.go:12 LSPLineStarts
// PORT: Go `[]core.TextPos`; `core.TextPos` is `int32`.
pub type LSPLineStarts = Vec<i32>;

// Go: ls/lsconv/linemap.go:14 LSPLineMap
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LSPLineMap {
    pub line_starts: LSPLineStarts,
    pub ascii_only: bool, // TODO(jakebailey): collect ascii-only info per line
    /// PORT: whether the text holds a marker unit (see
    /// `scanner_util::GO_STRING_MARKER`). Without one, port byte offsets
    /// are Go's, so a UTF-8 column needs no scan of its line (Go
    /// `converters.go` `start+char` and `position - start`).
    pub has_marker: bool,
}

// Go: ls/lsconv/linemap.go:19 ComputeLSPLineStarts
// PORT: Go returns `*LSPLineMap`, which callers share (file line map caches,
// `Converters.getLineMap`); here `Rc<LSPLineMap>`.
#[must_use]
pub fn compute_lsp_line_starts(text: &str) -> Rc<LSPLineMap> {
    // This is like core.ComputeLineStarts, but only considers "\n", "\r", and "\r\n" as line breaks,
    // and reports when the text is ASCII-only.
    let bytes = text.as_bytes();
    let mut line_starts: Vec<i32> =
        Vec::with_capacity(bytes.iter().filter(|&&b| b == b'\n').count() + 1);
    let mut ascii_only = true;

    let text_len = text.len() as i32;
    let mut pos: i32 = 0;
    let mut line_start: i32 = 0;
    while pos < text_len {
        let b = bytes[pos as usize];
        if i32::from(b) < RUNE_SELF {
            pos += 1;
            match b {
                b'\r' => {
                    if pos < text_len && bytes[pos as usize] == b'\n' {
                        pos += 1;
                    }
                    // Go: fallthrough
                    line_starts.push(line_start);
                    line_start = pos;
                }
                b'\n' => {
                    line_starts.push(line_start);
                    line_start = pos;
                }
                _ => {}
            }
        } else {
            let (_, size) = utf8_decode_rune_in_string(text, pos as usize);
            pos += size;
            ascii_only = false;
        }
    }
    line_starts.push(line_start);

    Rc::new(LSPLineMap {
        line_starts,
        ascii_only,
        has_marker: !ascii_only && contains_go_string_marker(text),
    })
}

impl LSPLineMap {
    // Go: ls/lsconv/linemap.go:56 ComputeIndexOfLineStart
    #[must_use]
    pub fn compute_index_of_line_start(&self, target_pos: i32) -> i32 {
        // port of computeLineOfPosition(lineStarts: readonly number[], position: number, lowerBound?: number): number {
        let (line_number, ok) =
            gostd::slices::binary_search_func(&self.line_starts, target_pos, |p: &i32, t: &i32| {
                // Go: cmp.Compare(int(p), int(t))
                p.cmp(t) as i32
            });
        let mut line_number = line_number as i32;
        if !ok && line_number > 0 {
            // If the actual position was not found, the binary search returns where the target line start would be inserted
            // if the target was in the slice.
            // e.g. if the line starts at [5, 10, 23, 80] and the position requested was 20
            // then the search will return (3, false).
            //
            // We want the index of the previous line start, so we subtract 1.
            line_number -= 1;
        }
        line_number
    }
}
