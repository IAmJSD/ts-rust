//! Go string helpers of `scanner_util.rs` (UTF-16 surrogates, the Go
//! string marker, `GoUnit`, Go byte offsets and compares). They live in
//! `goport_util` as the module `scanner_util`, so util files keep their
//! `crate::scanner_util::` paths. `ts_goport`'s `scanner_util.rs`
//! re-exports them.

use std::borrow::Cow;

/// First surrogate code point. It maps to `LONE_SURROGATE_UNIT_BASE`.
const SURROGATE_HIGH_START: u32 = 0xD800;
/// Go `SurrogateLowStart`.
pub const SURROGATE_LOW_START: u32 = 0xDC00;

/// Go `utf16.IsSurrogate`.
fn utf16_is_surrogate(ch: u32) -> bool {
    (0xD800..0xE000).contains(&ch)
}

// Go: stringutil/util.go:280 IsHighSurrogate
pub fn is_high_surrogate(ch: u32) -> bool {
    utf16_is_surrogate(ch) && ch < SURROGATE_LOW_START
}

// Go: stringutil/util.go:284 IsLowSurrogate
pub fn is_low_surrogate(ch: u32) -> bool {
    utf16_is_surrogate(ch) && ch >= SURROGATE_LOW_START
}

// Go: stringutil/util.go:288 IsSurrogate
pub fn is_surrogate(ch: u32) -> bool {
    utf16_is_surrogate(ch)
}

// Go: stringutil/util.go:292 SurrogatePairToCodePoint
pub fn surrogate_pair_to_code_point(high: u32, low: u32) -> u32 {
    // Go utf16.DecodeRune returns U+FFFD for an invalid pair.
    if (0xD800..0xDC00).contains(&high) && (0xDC00..0xE000).contains(&low) {
        (((high - 0xD800) << 10) | (low - 0xDC00)) + 0x10000
    } else {
        0xFFFD
    }
}

// PORT: a Go string is a byte string. Two kinds of Go bytes are not valid
// UTF-8, so a Rust `String` cannot hold them:
// - a lone surrogate (U+D800..U+DFFF), which Go `EncodeJSStringRune` stores
//   as its 3-byte WTF-8 form `ED A0..BF 80..BF`;
// - a source byte that is not valid UTF-8 (always 0x80..=0xFF), which Go
//   keeps unchanged and decodes as `utf8.RuneError` of size 1.
// This port stores a Go string in a "port form" with one escape. The marker
// M = GO_STRING_MARKER (U+FDD0, a Unicode noncharacter) starts a unit of two
// chars:
// - M + char(LONE_SURROGATE_UNIT_BASE + (cp - 0xD800)): the lone surrogate
//   `cp`. The second char is in U+10F800..U+10FFFF.
// - M + char(INVALID_BYTE_UNIT_BASE + b): the invalid byte `b`. The second
//   char is in U+10F780..U+10F7FF.
// - M + M: a real U+FDD0.
// Every other char is itself, including a real U+10F780..U+10FFFF char. So
// each Go string has one port form and each port form one Go string.
// `go_unit_at` reads one unit and `go_string_bytes` gives the Go bytes back.
// Where the port form comes from:
// - `vfs::decode_bytes` writes source text in it (`go_string_from_bytes`),
//   with one invalid byte unit for each byte that Go decodes as RuneError of
//   size 1. That includes each byte of a WTF-8 surrogate, as the Go scanner
//   reads them.
// - The scanner copies source text into token values. A string value fuses
//   the 3 invalid byte units of a WTF-8 surrogate into one lone surrogate
//   unit (`fuse_surrogate_bytes`). This fused form is the "value form": each
//   Go string has exactly one, so equal Go strings have equal value forms.
// - The OS gives file names, the working directory and arguments in the
//   value form (`vfs::go_string_from_os`), and takes the Go bytes back
//   (`vfs::os_path`).
// - A decoded JSON string is valid UTF-8, with each real U+FDD0 written
//   twice (`frontend::json`).
// - `push_js_string_rune` writes one rune.
// Go bytes leave the port only through file writes, OS paths, the process
// output and JSON strings, which all convert the port form
// (`go_string_bytes`, `frontend::json::append_json_quote`).
// A lone M that is not followed by M or a unit char (for example from Go
// `string(rune)`) reads as a real U+FDD0.
// Go works on the bytes of a string in places. The port does the same:
// - A unit is 6 or 7 bytes, not 3 or 1. Offsets in port text after a unit
//   differ from Go byte offsets. Callers use them to slice the same text.
//   `utf16_len` counts Go runes, so lines and columns match Go.
//   `go_byte_offset` and `port_byte_offset` convert an offset where Go
//   writes or reads it (build info diagnostics and declaration signatures).
//   `go_len` is Go `len`, for lengths that Go compares or limits.
// - Go joins strings by bytes, so joined invalid bytes can form a valid char
//   or a WTF-8 surrogate. `go_value` gives the value form of a join.
// - A byte search on the port form can match inside a unit, and a Go byte
//   search can match inside a char. `go_has_prefix`, `go_has_suffix` and
//   `go_slice` work on the Go bytes, as Go does.
pub const GO_STRING_MARKER: char = '\u{FDD0}';
const GO_STRING_MARKER_BYTES: &[u8] = "\u{FDD0}".as_bytes();
const LONE_SURROGATE_UNIT_BASE: u32 = 0x10F800;
const INVALID_BYTE_UNIT_BASE: u32 = 0x10F700;

/// One unit of the port form of a Go string (see `GO_STRING_MARKER`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GoUnit {
    /// A char. A real U+FDD0 is `Char('\u{FDD0}')`.
    Char(char),
    /// A lone surrogate: Go bytes `ED A0..BF 80..BF`.
    Surrogate(u32),
    /// A Go byte that is not valid UTF-8 (0x80..=0xFF).
    InvalidByte(u8),
}

impl GoUnit {
    /// The number of Go bytes in the unit.
    pub fn go_len(self) -> usize {
        match self {
            GoUnit::Char(ch) => ch.len_utf8(),
            GoUnit::Surrogate(_) => 3,
            GoUnit::InvalidByte(_) => 1,
        }
    }

    /// Go `core.UTF16Len` of the unit's Go bytes. Go `range` reads each
    /// byte that is not valid UTF-8 as one RuneError, which is one UTF-16
    /// unit. The 3 bytes of a lone surrogate are 3 such bytes.
    pub fn go_utf16_len(self) -> usize {
        match self {
            GoUnit::Char(ch) => ch.len_utf16(),
            GoUnit::Surrogate(_) => 3,
            GoUnit::InvalidByte(_) => 1,
        }
    }

    /// Appends the unit's Go bytes to `out`.
    pub fn push_go_bytes(self, out: &mut Vec<u8>) {
        match self {
            GoUnit::Char(ch) => out.extend_from_slice(ch.encode_utf8(&mut [0; 4]).as_bytes()),
            GoUnit::Surrogate(cp) => out.extend_from_slice(&[
                0xED,
                0x80 | ((cp >> 6) & 0x3F) as u8,
                0x80 | (cp & 0x3F) as u8,
            ]),
            GoUnit::InvalidByte(b) => out.push(b),
        }
    }
}

/// Reports whether `bytes[i..]` starts with `GO_STRING_MARKER`.
#[inline]
fn go_string_marker_at(bytes: &[u8], i: usize) -> bool {
    bytes.get(i..i + GO_STRING_MARKER_BYTES.len()) == Some(GO_STRING_MARKER_BYTES)
}

/// The byte offset of the first `GO_STRING_MARKER` at or after `from`. The
/// marker starts with a lead byte, so a match starts a char.
///
/// PERF: `memmem::find` builds a searcher on each call, which cost about 15%
/// of Hono's check time (template literal texts are short and many). The
/// lead byte 0xEF is rare in source text, so a check for it (core's inlined
/// `contains`), then `memchr` for it and a compare of the other two bytes,
/// is cheaper.
#[inline]
fn find_go_string_marker(bytes: &[u8], from: usize) -> Option<usize> {
    if !bytes[from..].contains(&GO_STRING_MARKER_BYTES[0]) {
        return None;
    }
    let mut i = from;
    while let Some(j) = memchr::memchr(GO_STRING_MARKER_BYTES[0], &bytes[i..]) {
        let at = i + j;
        if go_string_marker_at(bytes, at) {
            return Some(at);
        }
        i = at + 1;
    }
    None
}

/// Reports whether `text` holds `GO_STRING_MARKER`, which means that its
/// port form differs from plain UTF-8 (see `GO_STRING_MARKER`).
#[inline]
pub fn contains_go_string_marker(text: &str) -> bool {
    find_go_string_marker(text.as_bytes(), 0).is_some()
}

/// The unit that starts at byte `i` of the port form `s`, and its size in
/// `s` (see `GO_STRING_MARKER`). `i` must be a char boundary before the end.
pub fn go_unit_at(s: &str, i: usize) -> (GoUnit, usize) {
    let marker_len = GO_STRING_MARKER_BYTES.len();
    if go_string_marker_at(s.as_bytes(), i) {
        if let Some(next) = s[i + marker_len..].chars().next() {
            let size = marker_len + next.len_utf8();
            let code = next as u32;
            if next == GO_STRING_MARKER {
                return (GoUnit::Char(GO_STRING_MARKER), size);
            }
            if code >= LONE_SURROGATE_UNIT_BASE {
                let cp = code - LONE_SURROGATE_UNIT_BASE + SURROGATE_HIGH_START;
                return (GoUnit::Surrogate(cp), size);
            }
            if code >= INVALID_BYTE_UNIT_BASE + 0x80 {
                return (
                    GoUnit::InvalidByte((code - INVALID_BYTE_UNIT_BASE) as u8),
                    size,
                );
            }
        }
        return (GoUnit::Char(GO_STRING_MARKER), marker_len);
    }
    let ch = s[i..].chars().next().expect("go_unit_at before the end");
    (GoUnit::Char(ch), ch.len_utf8())
}

/// The unit that ends at byte `end` of the port form `s`, and its size in
/// `s`. `end` must be a unit boundary after the start.
pub fn go_unit_before(s: &str, end: usize) -> (GoUnit, usize) {
    let marker_len = GO_STRING_MARKER_BYTES.len();
    let ch = s[..end]
        .chars()
        .next_back()
        .expect("go_unit_before after the start");
    let start = end - ch.len_utf8();
    // A run of M chars starts at a unit start, and its units pair up from
    // there. Count the M chars before `ch`.
    let bytes = s.as_bytes();
    let mut run = 0usize;
    while start >= marker_len * (run + 1)
        && go_string_marker_at(bytes, start - marker_len * (run + 1))
    {
        run += 1;
    }
    if ch == GO_STRING_MARKER {
        // `ch` ends the run. An even run ends with an M + M unit.
        let size = if run % 2 == 1 {
            2 * marker_len
        } else {
            marker_len
        };
        return (GoUnit::Char(GO_STRING_MARKER), size);
    }
    if run % 2 == 1 && ch as u32 >= INVALID_BYTE_UNIT_BASE + 0x80 {
        return go_unit_at(s, start - marker_len);
    }
    (GoUnit::Char(ch), ch.len_utf8())
}

/// The Go bytes of the port form `s` (see `GO_STRING_MARKER`): the bytes
/// that Go writes to a file or the output. Text without a marker is
/// borrowed.
pub fn go_string_bytes(s: &str) -> Cow<'_, [u8]> {
    let bytes = s.as_bytes();
    let Some(first) = find_go_string_marker(bytes, 0) else {
        return Cow::Borrowed(bytes);
    };
    let mut out = Vec::with_capacity(bytes.len());
    let mut start = 0usize;
    let mut next = Some(first);
    while let Some(at) = next {
        out.extend_from_slice(&bytes[start..at]);
        let (unit, size) = go_unit_at(s, at);
        unit.push_go_bytes(&mut out);
        start = at + size;
        next = find_go_string_marker(bytes, start);
    }
    out.extend_from_slice(&bytes[start..]);
    Cow::Owned(out)
}

/// Go `strings.ToValidUTF8(s, "\u{FFFD}")` on the Go bytes of the port form
/// `s` (see `GO_STRING_MARKER`): each run of bytes that are not valid UTF-8
/// becomes one U+FFFD. A lone surrogate unit is 3 such bytes. Text without
/// a marker is borrowed.
pub fn go_to_valid_utf8(s: &str) -> Cow<'_, str> {
    let bytes = s.as_bytes();
    let mut out: Option<String> = None;
    let mut start = 0usize;
    // The end of the last replaced unit. A unit that starts there is in the
    // same run.
    let mut run_end = None;
    let mut next = find_go_string_marker(bytes, 0);
    while let Some(at) = next {
        let (unit, size) = go_unit_at(s, at);
        if matches!(unit, GoUnit::InvalidByte(_) | GoUnit::Surrogate(_)) {
            let out = out.get_or_insert_with(|| String::with_capacity(s.len()));
            out.push_str(&s[start..at]);
            if run_end != Some(at) {
                out.push(char::REPLACEMENT_CHARACTER);
            }
            start = at + size;
            run_end = Some(start);
        }
        next = find_go_string_marker(bytes, at + size);
    }
    match out {
        None => Cow::Borrowed(s),
        Some(mut out) => {
            out.push_str(&s[start..]);
            Cow::Owned(out)
        }
    }
}

/// The port form of the Go bytes `bytes` (see `GO_STRING_MARKER`). Each
/// byte that Go `utf8.DecodeRuneInString` reads as RuneError of size 1
/// becomes one invalid byte unit, and each real U+FDD0 becomes M + M.
pub fn go_string_from_bytes(bytes: Vec<u8>) -> String {
    match String::from_utf8(bytes) {
        Ok(text) => go_string_from_utf8(text),
        Err(err) => {
            let bytes = err.into_bytes();
            let mut out = String::with_capacity(bytes.len() + bytes.len() / 8);
            // Every byte of an invalid chunk is one Go RuneError of size 1:
            // the bytes after its first are continuation bytes, which never
            // start a valid sequence.
            for chunk in bytes.utf8_chunks() {
                push_go_string_from_utf8(&mut out, chunk.valid());
                for &b in chunk.invalid() {
                    out.push(GO_STRING_MARKER);
                    out.push(
                        char::from_u32(INVALID_BYTE_UNIT_BASE + u32::from(b))
                            .expect("U+10F780..U+10F7FF are chars"),
                    );
                }
            }
            out
        }
    }
}

/// The port form of the valid UTF-8 Go string `text`: each real U+FDD0
/// becomes M + M (see `GO_STRING_MARKER`).
pub fn go_string_from_utf8(text: String) -> String {
    if !contains_go_string_marker(&text) {
        return text;
    }
    let mut out = String::with_capacity(text.len() + 8);
    push_go_string_from_utf8(&mut out, &text);
    out
}

/// Appends the port form of the valid UTF-8 Go string `text` to `out`.
fn push_go_string_from_utf8(out: &mut String, text: &str) {
    let mut start = 0usize;
    while let Some(at) = find_go_string_marker(text.as_bytes(), start) {
        let end = at + GO_STRING_MARKER_BYTES.len();
        out.push_str(&text[start..end]);
        out.push(GO_STRING_MARKER);
        start = end;
    }
    out.push_str(&text[start..]);
}

/// Fuses the 3 invalid byte units of each WTF-8 surrogate
/// (`ED A0..BF 80..BF`) in the port form `s` into one lone surrogate unit,
/// the port form of the same Go bytes (see `GO_STRING_MARKER`). Source text
/// keeps the 3 units, as the Go scanner reads 3 bytes there. A string value
/// needs the one form. Text without a marker is borrowed.
pub fn fuse_surrogate_bytes(s: &str) -> Cow<'_, str> {
    let bytes = s.as_bytes();
    let mut out: Option<String> = None;
    let mut start = 0usize;
    let mut next = find_go_string_marker(bytes, 0);
    while let Some(at) = next {
        let (unit, size) = go_unit_at(s, at);
        let mut end = at + size;
        if let GoUnit::InvalidByte(0xED) = unit {
            let (code, triple_size, _) = decode_go_js_string_rune(&s[at..]);
            if is_surrogate(code) {
                let out = out.get_or_insert_with(|| String::with_capacity(s.len()));
                out.push_str(&s[start..at]);
                push_js_string_rune(out, code);
                end = at + triple_size;
                start = end;
            }
        }
        next = find_go_string_marker(bytes, end);
    }
    match out {
        None => Cow::Borrowed(s),
        Some(mut out) => {
            out.push_str(&s[start..]);
            Cow::Owned(out)
        }
    }
}

/// The value form (see `GO_STRING_MARKER`) of the Go string with bytes
/// `bytes`: `go_string_from_bytes` with each WTF-8 surrogate fused into one
/// unit (`fuse_surrogate_bytes`). Valid UTF-8 without U+FDD0 is borrowed.
pub fn go_value_from_bytes(bytes: &[u8]) -> Cow<'_, str> {
    match std::str::from_utf8(bytes) {
        Ok(text) if !contains_go_string_marker(text) => Cow::Borrowed(text),
        _ => {
            let text = go_string_from_bytes(bytes.to_vec());
            Cow::Owned(match fuse_surrogate_bytes(&text) {
                Cow::Borrowed(_) => text,
                Cow::Owned(fused) => fused,
            })
        }
    }
}

/// The value form of the port form `s` (see `GO_STRING_MARKER`). Go joins
/// strings by bytes: invalid byte units that a join puts next to each other
/// can hold the bytes of a valid char or a WTF-8 surrogate. Text without a
/// marker is borrowed.
pub fn go_value(s: &str) -> Cow<'_, str> {
    if !contains_go_string_marker(s) {
        return Cow::Borrowed(s);
    }
    let bytes = go_string_bytes(s);
    let value = go_value_from_bytes(&bytes);
    if value == s {
        Cow::Borrowed(s)
    } else {
        Cow::Owned(value.into_owned())
    }
}

/// `go_value` for an owned string.
pub fn go_value_owned(s: String) -> String {
    match go_value(&s) {
        Cow::Borrowed(_) => s,
        Cow::Owned(value) => value,
    }
}

/// Go `len(s)`: the number of Go bytes of the port form `s` (see
/// `GO_STRING_MARKER`).
pub fn go_len(s: &str) -> usize {
    let bytes = s.as_bytes();
    let mut extra = 0usize;
    let mut next = find_go_string_marker(bytes, 0);
    while let Some(at) = next {
        let (unit, size) = go_unit_at(s, at);
        extra += size - unit.go_len();
        next = find_go_string_marker(bytes, at + size);
    }
    bytes.len() - extra
}

/// Go `strings.Compare(a, b)` on the Go bytes of two port forms.
/// `compare_go_strings` gives the same order without the copies.
pub fn compare_go_bytes(a: &str, b: &str) -> std::cmp::Ordering {
    go_string_bytes(a).cmp(&go_string_bytes(b))
}

/// Go `slices.Compare` of two string slices, each compared by Go bytes
/// (`compare_go_bytes`).
pub fn compare_go_bytes_slices(a: &[String], b: &[String]) -> std::cmp::Ordering {
    for (x, y) in a.iter().zip(b) {
        let c = compare_go_bytes(x, y);
        if c.is_ne() {
            return c;
        }
    }
    a.len().cmp(&b.len())
}

/// Go `strings.HasPrefix(s, prefix)` on the Go bytes of two port forms.
pub fn go_has_prefix(s: &str, prefix: &str) -> bool {
    go_string_bytes(s).starts_with(&go_string_bytes(prefix))
}

/// Go `strings.HasSuffix(s, suffix)` on the Go bytes of two port forms.
pub fn go_has_suffix(s: &str, suffix: &str) -> bool {
    go_string_bytes(s).ends_with(&go_string_bytes(suffix))
}

/// Go `s[from:to]` with Go byte offsets, in the value form (see
/// `go_value_from_bytes`). A cut inside a char leaves invalid bytes, as in
/// Go. Text without a marker, cut at char boundaries, is borrowed.
pub fn go_slice(s: &str, from: usize, to: usize) -> Cow<'_, str> {
    match go_string_bytes(s) {
        Cow::Borrowed(bytes) => go_value_from_bytes(&bytes[from..to]),
        Cow::Owned(bytes) => Cow::Owned(go_value_from_bytes(&bytes[from..to]).into_owned()),
    }
}

/// Go `[]rune(s)` on the Go bytes of the port form `s`: each byte that is
/// not valid UTF-8 is one U+FFFD. A lone surrogate unit is 3 such bytes.
pub fn go_runes(s: &str) -> Vec<char> {
    if !contains_go_string_marker(s) {
        return s.chars().collect();
    }
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0usize;
    while i < s.len() {
        let (unit, size) = go_unit_at(s, i);
        match unit {
            GoUnit::Char(ch) => out.push(ch),
            GoUnit::InvalidByte(_) => out.push(char::REPLACEMENT_CHARACTER),
            GoUnit::Surrogate(_) => out.extend([char::REPLACEMENT_CHARACTER; 3]),
        }
        i += size;
    }
    out
}

/// Go `strings.Map(mapping, s)` on the Go bytes of the port form `s` (see
/// `GO_STRING_MARKER`). Go passes each byte that is not valid UTF-8 to
/// `mapping` as U+FFFD and writes the result, so the result has no invalid
/// byte or lone surrogate unit. A lone surrogate unit is 3 such bytes.
pub fn go_map_runes(s: &str, mapping: impl Fn(char) -> char) -> String {
    let mut out = String::with_capacity(s.len());
    let mut i = 0usize;
    while i < s.len() {
        let (unit, size) = go_unit_at(s, i);
        let (ch, count) = match unit {
            GoUnit::Char(ch) => (ch, 1),
            GoUnit::InvalidByte(_) => (char::REPLACEMENT_CHARACTER, 1),
            GoUnit::Surrogate(_) => (char::REPLACEMENT_CHARACTER, 3),
        };
        for _ in 0..count {
            let mapped = mapping(ch);
            out.push(mapped);
            if mapped == GO_STRING_MARKER {
                out.push(GO_STRING_MARKER);
            }
        }
        i += size;
    }
    out
}

/// Go `utf8.DecodeRune(bytes)`: the rune and its size. An invalid byte is
/// `(RuneError, 1)`, and an empty slice `(RuneError, 0)`.
pub fn go_decode_rune_bytes(bytes: &[u8]) -> (u32, usize) {
    // A valid first char fits in 4 bytes.
    let head = &bytes[..bytes.len().min(4)];
    match head.utf8_chunks().next() {
        None => (char::REPLACEMENT_CHARACTER as u32, 0),
        Some(chunk) => match chunk.valid().chars().next() {
            Some(ch) => (ch as u32, ch.len_utf8()),
            None => (char::REPLACEMENT_CHARACTER as u32, 1),
        },
    }
}

// Go: stringutil/util.go:334 DecodeJSStringRune
/// Go `DecodeJSStringRune` on Go bytes: the WTF-8 bytes of a lone
/// surrogate are one rune of size 3.
pub fn go_decode_js_string_rune_bytes(bytes: &[u8]) -> (u32, usize) {
    if let [0xED, b1 @ 0xA0..=0xBF, b2 @ 0x80..=0xBF, ..] = *bytes {
        return (
            0xD000 | (u32::from(b1) & 0x3F) << 6 | (u32::from(b2) & 0x3F),
            3,
        );
    }
    go_decode_rune_bytes(bytes)
}

/// Appends the port form of the Go rune `ch` (see `GO_STRING_MARKER`) to
/// `out`.
pub fn push_js_string_rune(out: &mut String, ch: u32) {
    if is_surrogate(ch) {
        out.push(GO_STRING_MARKER);
        // The unit base maps the surrogate range onto valid chars.
        out.push(
            char::from_u32(LONE_SURROGATE_UNIT_BASE + (ch - SURROGATE_HIGH_START))
                .unwrap_or(char::REPLACEMENT_CHARACTER),
        );
        return;
    }
    let c = char::from_u32(ch).unwrap_or(char::REPLACEMENT_CHARACTER);
    if c == GO_STRING_MARKER {
        out.push(GO_STRING_MARKER);
    }
    out.push(c);
}

// Go: stringutil/util.go:323 EncodeJSStringRune
// PORT: writes the port form (see GO_STRING_MARKER), not WTF-8.
pub fn encode_js_string_rune(ch: u32) -> String {
    let mut out = String::with_capacity(8);
    push_js_string_rune(&mut out, ch);
    out
}

// Go: stringutil/util.go:334 DecodeJSStringRune
// PORT: returns the Go `rune` as `u32`, because a lone surrogate is not a
// valid Rust `char`, and the size in `s`. It reads one unit of the port form
// (see GO_STRING_MARKER). An empty string gives U+FFFD with size 0, as
// `utf8.DecodeRuneInString`. `decode_go_js_string_rune` also gives the Go
// size.
pub fn decode_js_string_rune(s: &str) -> (u32, i32) {
    let (code, size, _) = decode_go_js_string_rune(s);
    (code, size as i32)
}

/// Go `DecodeJSStringRune(s)` on the Go bytes of the port form `s`: the rune,
/// its size in `s` and its size in Go bytes. An invalid byte is
/// `(RuneError, size, 1)`, and a real U+FFFD is `(RuneError, 3, 3)`.
pub fn decode_go_js_string_rune(s: &str) -> (u32, usize, usize) {
    if s.is_empty() {
        return (char::REPLACEMENT_CHARACTER as u32, 0, 0);
    }
    let (unit, size) = go_unit_at(s, 0);
    match unit {
        GoUnit::Char(ch) => (ch as u32, size, ch.len_utf8()),
        GoUnit::Surrogate(cp) => (cp, size, 3),
        GoUnit::InvalidByte(b) => {
            // Go checks for the WTF-8 bytes of a surrogate first. They are 3
            // invalid byte units here when they came from source text.
            if b == 0xED && size < s.len() {
                if let (GoUnit::InvalidByte(b1), size1) = go_unit_at(s, size) {
                    if (0xA0..=0xBF).contains(&b1) && size + size1 < s.len() {
                        if let (GoUnit::InvalidByte(b2), size2) = go_unit_at(s, size + size1) {
                            if (0x80..=0xBF).contains(&b2) {
                                let cp =
                                    0xD000 | (u32::from(b1) & 0x3F) << 6 | (u32::from(b2) & 0x3F);
                                return (cp, size + size1 + size2, 3);
                            }
                        }
                    }
                }
            }
            (char::REPLACEMENT_CHARACTER as u32, size, 1)
        }
    }
}

// PORT: Go `strings.Compare` on the Go bytes of a string. Plain `str` order
// differs from Go in two places, because this port stores two Go byte forms
// in other ways (see GO_STRING_MARKER):
// - A lone surrogate unit is the 3-byte WTF-8 form `ED A0..BF 80..BF` in Go.
//   A real U+FDD0 unit (M + M) is `EF B7 90`.
// - An invalid byte unit is one byte 0x80..=0xFF in Go. This includes the
//   internal symbol name prefix `\xFE` (`ast::INTERNAL_SYMBOL_NAME_PREFIX`).
// The Go bytes of chars and lone surrogates are prefix free, so comparing the
// first different unit by its Go bytes gives the Go result. An invalid byte
// can be a prefix of another unit's bytes, so then the Go bytes of the rest
// of both strings are compared.
pub fn compare_go_strings(a: &str, b: &str) -> std::cmp::Ordering {
    /// Length of the common byte prefix. It compares 8 bytes at a time: in
    /// a little-endian load, the lowest set bit of the XOR is in the first
    /// different byte.
    fn common_prefix_len(a: &[u8], b: &[u8]) -> usize {
        let mut p = 0;
        for (x, y) in a.chunks_exact(8).zip(b.chunks_exact(8)) {
            let diff = u64::from_le_bytes(x.try_into().unwrap())
                ^ u64::from_le_bytes(y.try_into().unwrap());
            if diff != 0 {
                return p + (diff.trailing_zeros() / 8) as usize;
            }
            p += 8;
        }
        p + a[p..]
            .iter()
            .zip(&b[p..])
            .take_while(|(x, y)| x == y)
            .count()
    }
    // Equal bytes are equal characters, so skip the common byte prefix and
    // step back to the start of the first different character.
    let mut p = common_prefix_len(a.as_bytes(), b.as_bytes());
    // PERF: an ASCII byte is always a whole unit whose Go byte is itself.
    // When the prefix is empty or ends with an ASCII byte, `p` starts a unit
    // in both strings. When the next byte of each string is ASCII or absent,
    // those units are single bytes (or the end), so byte order is the Go
    // order. This holds for any encoding in which non-ASCII units have Go
    // bytes >= 0x80, and skips the unit decoding below.
    if p == 0 || a.as_bytes()[p - 1].is_ascii() {
        let (x, y) = (a.as_bytes().get(p), b.as_bytes().get(p));
        if x.is_none_or(u8::is_ascii) && y.is_none_or(u8::is_ascii) {
            return x.cmp(&y);
        }
    }
    while !a.is_char_boundary(p) {
        p -= 1;
    }
    // Step back to the start of the unit. Every M starts a unit or is the
    // second M of an M + M unit, so a run of M chars before `p` starts at a
    // unit start. An odd run means `p` is the second char of a unit.
    let marker_len = GO_STRING_MARKER_BYTES.len();
    let mut run = 0usize;
    while p >= marker_len * (run + 1)
        && go_string_marker_at(a.as_bytes(), p - marker_len * (run + 1))
    {
        run += 1;
    }
    if run % 2 == 1 {
        p -= marker_len;
    }
    let (ra, rb) = (&a[p..], &b[p..]);
    match (ra.is_empty(), rb.is_empty()) {
        (false, false) => {
            let (ua, _) = go_unit_at(ra, 0);
            let (ub, _) = go_unit_at(rb, 0);
            if matches!(ua, GoUnit::InvalidByte(_)) || matches!(ub, GoUnit::InvalidByte(_)) {
                return go_string_bytes(ra).cmp(&go_string_bytes(rb));
            }
            let (mut bx, mut by) = ([0u8; 4], [0u8; 4]);
            go_unit_bytes(ua, &mut bx).cmp(go_unit_bytes(ub, &mut by))
        }
        (x, y) => y.cmp(&x),
    }
}

/// The Go bytes of a unit, written to `buf` (see `compare_go_strings` and
/// ls `get_possible_symbol_reference_positions`).
pub fn go_unit_bytes(unit: GoUnit, buf: &mut [u8; 4]) -> &[u8] {
    match unit {
        GoUnit::Surrogate(cp) => {
            *buf = [
                0xED,
                0x80 | ((cp >> 6) & 0x3F) as u8,
                0x80 | (cp & 0x3F) as u8,
                0,
            ];
            &buf[..3]
        }
        GoUnit::Char(ch) => {
            let len = ch.encode_utf8(&mut buf[..]).len();
            &buf[..len]
        }
        GoUnit::InvalidByte(b) => {
            buf[0] = b;
            &buf[..1]
        }
    }
}

/// The Go byte offset of the port offset `pos` in the port form `text` (see
/// `GO_STRING_MARKER`). Go writes byte offsets into build info, declaration
/// signatures, LSP UTF-8 columns and completion data. A `pos` that is `k`
/// bytes into a unit of `g` Go bytes gives the unit's Go offset plus
/// `min(k, g)`; `port_byte_offset` gives back `pos` for `k < g`. A negative
/// offset is kept, and an offset past the end counts the bytes past it.
pub fn go_byte_offset(text: &str, pos: i32) -> i32 {
    if pos <= 0 {
        return pos;
    }
    let bytes = text.as_bytes();
    let end = (pos as usize).min(bytes.len());
    // A unit that `end` cuts can start inside its marker, so the search
    // reads up to 2 bytes past `end`.
    let window = &bytes[..(end + GO_STRING_MARKER_BYTES.len() - 1).min(bytes.len())];
    let find = |from: usize| find_go_string_marker(window, from).filter(|&at| at < end);
    // The port bytes of the units before `end` that Go does not have.
    let mut extra = 0usize;
    let mut next = find(0);
    while let Some(at) = next {
        let (unit, size) = go_unit_at(text, at);
        if at + size > end {
            // `pos` is inside the unit: count its first `min(k, g)` Go
            // bytes.
            extra += (end - at).saturating_sub(unit.go_len());
            break;
        }
        extra += size - unit.go_len();
        next = find(at + size);
    }
    pos - extra as i32
}

/// The port offset of the Go byte offset `go_pos` in the port form `text`,
/// the inverse of `go_byte_offset`. An offset `k` bytes into the Go bytes of
/// a unit gives the unit's port offset plus `k`, so that `go_byte_offset`
/// gives `go_pos` back. A negative offset is kept, and an offset past the
/// end counts the bytes past it.
// PORT: such an offset is inside the unit's first char. For a real U+FDD0
// (M + M) the first M holds Go's 3 bytes, so a byte slice there cuts the
// same bytes as Go.
pub fn port_byte_offset(text: &str, go_pos: i32) -> i32 {
    if go_pos <= 0 {
        return go_pos;
    }
    let go_pos = go_pos as usize;
    let bytes = text.as_bytes();
    // `port` and `go` are the same point in both texts.
    let (mut port, mut go) = (0usize, 0usize);
    while let Some(at) = find_go_string_marker(bytes, port) {
        if go + (at - port) >= go_pos {
            break;
        }
        go += at - port;
        let (unit, size) = go_unit_at(text, at);
        if go + unit.go_len() > go_pos {
            return (at + (go_pos - go)) as i32;
        }
        go += unit.go_len();
        port = at + size;
    }
    (port + (go_pos - go)) as i32
}

/// `go_byte_offset` and `port_byte_offset` for many offsets of one text,
/// with one scan for markers. LSP code lenses and references convert one
/// offset for each item; each call of the plain functions scans from the
/// text start.
pub struct GoOffsets {
    len: usize,
    /// The marker units in text order.
    units: Vec<GoOffsetUnit>,
}

struct GoOffsetUnit {
    /// The unit's port offset and size.
    at: usize,
    size: usize,
    /// The unit's Go offset and Go length.
    go_at: usize,
    go_len: usize,
}

impl GoOffsets {
    pub fn new(text: &str) -> Self {
        let bytes = text.as_bytes();
        let mut units = Vec::new();
        // `port` and `go` are the same point in both texts.
        let (mut port, mut go) = (0usize, 0usize);
        while let Some(at) = find_go_string_marker(bytes, port) {
            let (unit, size) = go_unit_at(text, at);
            let go_at = go + (at - port);
            units.push(GoOffsetUnit {
                at,
                size,
                go_at,
                go_len: unit.go_len(),
            });
            go = go_at + unit.go_len();
            port = at + size;
        }
        Self {
            len: text.len(),
            units,
        }
    }

    /// Reports whether the text has a marker unit. Without one, port and Go
    /// offsets are the same.
    pub fn has_units(&self) -> bool {
        !self.units.is_empty()
    }

    /// `go_byte_offset(text, pos)`.
    pub fn go_offset(&self, pos: i32) -> i32 {
        if pos <= 0 {
            return pos;
        }
        let end = (pos as usize).min(self.len);
        let i = self.units.partition_point(|u| u.at < end);
        let Some(u) = i.checked_sub(1).map(|i| &self.units[i]) else {
            return pos;
        };
        // The port bytes before `u` that Go does not have, then `u`'s.
        let mut extra = u.at - u.go_at;
        if u.at + u.size > end {
            extra += (end - u.at).saturating_sub(u.go_len);
        } else {
            extra += u.size - u.go_len;
        }
        pos - extra as i32
    }

    /// `port_byte_offset(text, go_pos)`.
    pub fn port_offset(&self, go_pos: i32) -> i32 {
        if go_pos <= 0 {
            return go_pos;
        }
        let go_pos = go_pos as usize;
        let i = self.units.partition_point(|u| u.go_at < go_pos);
        let Some(u) = i.checked_sub(1).map(|i| &self.units[i]) else {
            return go_pos as i32;
        };
        if u.go_at + u.go_len > go_pos {
            return (u.at + (go_pos - u.go_at)) as i32;
        }
        (u.at + u.size + (go_pos - u.go_at - u.go_len)) as i32
    }
}

/// The unit of the port form `s` that holds byte `pos` after its start
/// (see `GO_STRING_MARKER`): the unit's start, the unit and its size in `s`.
/// `None` when `pos` is a unit boundary or at or past the end. A char that
/// is not in a marker unit is a unit, so a `pos` inside it gives it.
// PERF: only a continuation byte, the marker's lead byte or a unit char's
// lead byte (0xF4) can be inside a unit, so most calls return after one
// compare.
pub fn go_unit_cut_at(s: &str, pos: usize) -> Option<(usize, GoUnit, usize)> {
    let marker_len = GO_STRING_MARKER_BYTES.len();
    let bytes = s.as_bytes();
    let &b = bytes.get(pos)?;
    if pos == 0 || b < 0x80 || (b >= 0xC0 && b != GO_STRING_MARKER_BYTES[0] && b != 0xF4) {
        return None;
    }
    let mut c = pos;
    while !s.is_char_boundary(c) {
        c -= 1;
    }
    // `c` starts a unit unless it is the second char of a marker unit: M or
    // a unit char after a run of M chars of odd length (see
    // `go_unit_before`).
    let ch = s[c..].chars().next()?;
    let mut at = c;
    if ch == GO_STRING_MARKER || ch as u32 >= INVALID_BYTE_UNIT_BASE + 0x80 {
        let mut run = 0usize;
        while c >= marker_len * (run + 1) && go_string_marker_at(bytes, c - marker_len * (run + 1))
        {
            run += 1;
        }
        if run % 2 == 1 {
            at = c - marker_len;
        }
    }
    if at == pos {
        return None;
    }
    let (unit, size) = go_unit_at(s, at);
    Some((at, unit, size))
}

// Go: stringutil/util.go:352 CombineSurrogatePairs
// CombineSurrogatePairs canonicalizes a JS-string value produced by
// concatenation, merging any adjacent high+low surrogate sentinel pair (as
// written by EncodeJSStringRune) into the single supplementary code point they
// represent. Strings without a lone-surrogate sentinel (the common case) are
// returned unchanged.
// PORT: the sentinel check looks for GO_STRING_MARKER.
pub fn combine_surrogate_pairs(s: &str) -> String {
    combine_surrogate_pairs_cow(s).into_owned()
}

/// `combine_surrogate_pairs` that borrows `s` when it is unchanged, so only
/// a string with a sentinel is copied.
pub fn combine_surrogate_pairs_cow(s: &str) -> Cow<'_, str> {
    if !contains_go_string_marker(s) {
        return Cow::Borrowed(s);
    }
    let mut b = String::with_capacity(s.len());
    let mut i = 0usize;
    while i < s.len() {
        let (r, size) = decode_js_string_rune(&s[i..]);
        let size = size as usize;
        if is_high_surrogate(r) {
            let (low, low_size) = decode_js_string_rune(&s[i + size..]);
            if is_low_surrogate(low) {
                let combined = surrogate_pair_to_code_point(r, low);
                b.push(char::from_u32(combined).unwrap_or(char::REPLACEMENT_CHARACTER));
                i += size + low_size as usize;
                continue;
            }
        }
        b.push_str(&s[i..i + size]);
        i += size;
    }
    Cow::Owned(b)
}
