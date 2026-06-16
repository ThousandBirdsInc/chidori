//! Minimal in-tree WTF-8 support.
//!
//! WTF-8 (Sapin, <https://simonsapin.github.io/wtf-8/>) is a superset of UTF-8
//! that can additionally encode lone surrogate code points. It backs
//! [`crate::value::JsString`] so that JS UTF-16 code-unit semantics — lone
//! surrogates and astral-plane `.length`/indexing — are representable, which
//! the previous Unicode-scalar (`Rc<str>`) model could not express.
//!
//! Valid UTF-8 is a *byte-exact* subset of WTF-8: a code-unit sequence with no
//! unpaired surrogate encodes to exactly the bytes Rust's UTF-8 would produce.
//! Only strings containing an unpaired surrogate use the WTF-8-specific 3-byte
//! surrogate encoding (`ED A0..BF ..`), so the common case pays nothing.
//!
//! The buffers we produce are always *well-formed* WTF-8: an adjacent
//! high+low surrogate pair is combined into a single 4-byte (astral) sequence
//! rather than two 3-byte sequences. We only ever build WTF-8 through
//! [`encode_wtf8`], so the invariant holds by construction; concatenation that
//! could straddle a pair routes back through code units (see `JsString::concat`).

const HIGH: std::ops::RangeInclusive<u16> = 0xD800..=0xDBFF;
const LOW: std::ops::RangeInclusive<u16> = 0xDC00..=0xDFFF;

#[inline]
fn is_high(u: u16) -> bool {
    HIGH.contains(&u)
}
#[inline]
fn is_low(u: u16) -> bool {
    LOW.contains(&u)
}
#[inline]
fn combine(hi: u16, lo: u16) -> u32 {
    0x1_0000 + (((hi as u32) - 0xD800) << 10) + ((lo as u32) - 0xDC00)
}

/// `true` if `units` contains no unpaired surrogate, i.e. it encodes to valid
/// UTF-8 and can take the cheap `JsString::Utf8` arm.
pub fn is_well_formed(units: &[u16]) -> bool {
    let mut i = 0;
    while i < units.len() {
        let u = units[i];
        if is_high(u) {
            if i + 1 < units.len() && is_low(units[i + 1]) {
                i += 2;
                continue;
            }
            return false; // unpaired high
        }
        if is_low(u) {
            return false; // unpaired low
        }
        i += 1;
    }
    true
}

/// Push one code point (0..=0x10FFFF, *including* surrogates) as generalized
/// UTF-8 bytes. Identical to UTF-8 except surrogate code points are permitted
/// (encoded as their 3-byte form).
fn push_code_point(out: &mut Vec<u8>, cp: u32) {
    if cp < 0x80 {
        out.push(cp as u8);
    } else if cp < 0x800 {
        out.push(0xC0 | (cp >> 6) as u8);
        out.push(0x80 | (cp & 0x3F) as u8);
    } else if cp < 0x1_0000 {
        out.push(0xE0 | (cp >> 12) as u8);
        out.push(0x80 | ((cp >> 6) & 0x3F) as u8);
        out.push(0x80 | (cp & 0x3F) as u8);
    } else {
        out.push(0xF0 | (cp >> 18) as u8);
        out.push(0x80 | ((cp >> 12) & 0x3F) as u8);
        out.push(0x80 | ((cp >> 6) & 0x3F) as u8);
        out.push(0x80 | (cp & 0x3F) as u8);
    }
}

/// Encode a UTF-16 code-unit sequence to well-formed WTF-8. Adjacent high+low
/// surrogate pairs combine into one astral code point; unpaired surrogates are
/// preserved as 3-byte sequences.
pub fn encode_wtf8(units: &[u16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(units.len());
    let mut i = 0;
    while i < units.len() {
        let u = units[i];
        if is_high(u) && i + 1 < units.len() && is_low(units[i + 1]) {
            push_code_point(&mut out, combine(u, units[i + 1]));
            i += 2;
        } else {
            push_code_point(&mut out, u as u32);
            i += 1;
        }
    }
    out
}

/// Decode the generalized-UTF-8 sequence starting at `bytes[i]`, returning the
/// code point and the number of bytes consumed. Assumes well-formed WTF-8.
fn decode_code_point(bytes: &[u8], i: usize) -> (u32, usize) {
    let b0 = bytes[i];
    if b0 < 0x80 {
        (b0 as u32, 1)
    } else if b0 < 0xE0 {
        let cp = (((b0 & 0x1F) as u32) << 6) | ((bytes[i + 1] & 0x3F) as u32);
        (cp, 2)
    } else if b0 < 0xF0 {
        let cp = (((b0 & 0x0F) as u32) << 12)
            | (((bytes[i + 1] & 0x3F) as u32) << 6)
            | ((bytes[i + 2] & 0x3F) as u32);
        (cp, 3)
    } else {
        let cp = (((b0 & 0x07) as u32) << 18)
            | (((bytes[i + 1] & 0x3F) as u32) << 12)
            | (((bytes[i + 2] & 0x3F) as u32) << 6)
            | ((bytes[i + 3] & 0x3F) as u32);
        (cp, 4)
    }
}

/// Number of UTF-16 code units encoded by a WTF-8 buffer (astral code points
/// count as 2).
pub fn code_unit_len(bytes: &[u8]) -> usize {
    let mut i = 0;
    let mut n = 0;
    while i < bytes.len() {
        let b0 = bytes[i];
        let (width, units) = if b0 < 0x80 {
            (1, 1)
        } else if b0 < 0xE0 {
            (2, 1)
        } else if b0 < 0xF0 {
            (3, 1)
        } else {
            (4, 2)
        };
        i += width;
        n += units;
    }
    n
}

/// Iterator over the UTF-16 code units of a WTF-8 buffer.
pub struct Wtf8Units<'a> {
    bytes: &'a [u8],
    i: usize,
    pending_low: u16,
}

impl Iterator for Wtf8Units<'_> {
    type Item = u16;
    fn next(&mut self) -> Option<u16> {
        if self.pending_low != 0 {
            let lo = self.pending_low;
            self.pending_low = 0;
            return Some(lo);
        }
        if self.i >= self.bytes.len() {
            return None;
        }
        let (cp, w) = decode_code_point(self.bytes, self.i);
        self.i += w;
        if cp < 0x1_0000 {
            Some(cp as u16)
        } else {
            // Astral: emit a surrogate pair, low unit on the next call.
            let v = cp - 0x1_0000;
            self.pending_low = 0xDC00 + (v & 0x3FF) as u16;
            Some(0xD800 + (v >> 10) as u16)
        }
    }
}

/// Decode WTF-8 bytes to their UTF-16 code units.
pub fn decode_units(bytes: &[u8]) -> Wtf8Units<'_> {
    Wtf8Units {
        bytes,
        i: 0,
        pending_low: 0,
    }
}

/// Lossy decode to a Rust `String`: every unpaired surrogate becomes U+FFFD.
/// This is the conversion used at the host boundary (JSON, tool/prompt args),
/// where lone surrogates cannot cross. For well-formed buffers it is an exact
/// round-trip.
pub fn to_string_lossy(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let (cp, w) = decode_code_point(bytes, i);
        i += w;
        match char::from_u32(cp) {
            Some(c) => s.push(c),
            None => s.push('\u{FFFD}'), // surrogate code point
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    // Round-trip code units -> WTF-8 -> code units.
    fn round_trip(units: &[u16]) -> Vec<u16> {
        decode_units(&encode_wtf8(units)).collect()
    }

    #[test]
    fn ascii_and_bmp_round_trip() {
        let u: Vec<u16> = "héllo wörld".encode_utf16().collect();
        assert_eq!(round_trip(&u), u);
        // Valid UTF-8 byte-for-byte: WTF-8 of clean text == its UTF-8.
        assert_eq!(encode_wtf8(&u), "héllo wörld".as_bytes());
    }

    #[test]
    fn surrogate_pair_combines_to_four_bytes() {
        // U+1F600 GRINNING FACE = D83D DE00.
        let units = [0xD83Du16, 0xDE00];
        let bytes = encode_wtf8(&units);
        assert_eq!(bytes, "😀".as_bytes()); // single 4-byte sequence
        assert_eq!(code_unit_len(&bytes), 2);
        assert_eq!(round_trip(&units), units);
        assert!(is_well_formed(&units));
    }

    #[test]
    fn lone_high_surrogate_preserved() {
        let units = [0x0041u16, 0xD800, 0x0042]; // "A<lone hi>B"
        let bytes = encode_wtf8(&units);
        assert_eq!(code_unit_len(&bytes), 3);
        assert_eq!(round_trip(&units), units);
        assert!(!is_well_formed(&units));
        assert_eq!(to_string_lossy(&bytes), "A\u{FFFD}B");
    }

    #[test]
    fn lone_low_surrogate_preserved() {
        let units = [0xDC00u16];
        assert!(!is_well_formed(&units));
        assert_eq!(round_trip(&units), units);
        assert_eq!(to_string_lossy(&encode_wtf8(&units)), "\u{FFFD}");
    }

    #[test]
    fn split_pair_across_concat_recombines() {
        // A high surrogate then a separate low surrogate, encoded together,
        // must combine into the single astral code point (well-formed WTF-8).
        let joined = [0xD83Du16, 0xDE00];
        assert!(is_well_formed(&joined));
        assert_eq!(encode_wtf8(&joined), "😀".as_bytes());
        // But two lone highs stay two 3-byte sequences.
        let two_high = [0xD800u16, 0xD800];
        assert_eq!(code_unit_len(&encode_wtf8(&two_high)), 2);
        assert!(!is_well_formed(&two_high));
    }
}
