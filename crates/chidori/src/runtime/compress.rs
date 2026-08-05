//! Deterministic compression backing the `node:zlib` shim.
//!
//! Compression is a pure function of `(input, level)` for a fixed miniz
//! version, so — exactly like `node:crypto` hashing — the codecs run inline
//! with no capture: two runs (and record/replay) produce identical bytes.
//! Decompressed output is bounded so hostile input (a zip bomb inside
//! fetched data) fails with a clear error instead of exhausting the host.
//!
//! Brotli is NOT provided here: there is no Brotli codec in the dependency
//! tree, and the shim keeps those entry points fail-loud.

use std::io::{Read, Write};

use flate2::read::{DeflateDecoder, GzDecoder, ZlibDecoder};
use flate2::write::{DeflateEncoder, GzEncoder, ZlibEncoder};
use flate2::Compression;

/// Cap on a single codec call's output. Large enough for real payloads,
/// small enough that a decompression bomb dies loudly before the memory
/// watchdog has to intervene.
const MAX_OUTPUT_BYTES: u64 = 64 * 1024 * 1024;

/// Map a Node zlib `level` (-1 = Z_DEFAULT_COMPRESSION, 0-9) to flate2's.
fn compression(level: Option<i64>) -> Result<Compression, String> {
    match level {
        None | Some(-1) => Ok(Compression::default()),
        Some(l @ 0..=9) => Ok(Compression::new(l as u32)),
        Some(other) => Err(format!(
            "zlib: invalid compression level {other} (expected -1 through 9)"
        )),
    }
}

fn encode<W: Write>(
    mut encoder: W,
    data: &[u8],
    finish: impl FnOnce(W) -> std::io::Result<Vec<u8>>,
    what: &str,
) -> Result<Vec<u8>, String> {
    encoder
        .write_all(data)
        .map_err(|e| format!("zlib: {what} failed: {e}"))?;
    finish(encoder).map_err(|e| format!("zlib: {what} failed: {e}"))
}

fn bounded_decode(reader: impl Read, what: &str) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let mut limited = reader.take(MAX_OUTPUT_BYTES + 1);
    limited
        .read_to_end(&mut out)
        .map_err(|e| format!("zlib: {what} failed: {e}"))?;
    if out.len() as u64 > MAX_OUTPUT_BYTES {
        return Err(format!(
            "zlib: {what} output exceeds the {} MiB cap",
            MAX_OUTPUT_BYTES / (1024 * 1024)
        ));
    }
    Ok(out)
}

/// Run one zlib codec op. `op` matches the `node:zlib` function family:
/// `deflate`/`inflate` (zlib wrapper), `deflateRaw`/`inflateRaw` (bare
/// DEFLATE), `gzip`/`gunzip`, and `unzip` (auto-detects gzip vs zlib by the
/// gzip magic bytes, like Node).
pub fn zlib_op(op: &str, data: &[u8], level: Option<i64>) -> Result<Vec<u8>, String> {
    match op {
        "deflate" => encode(
            ZlibEncoder::new(Vec::new(), compression(level)?),
            data,
            ZlibEncoder::finish,
            "deflate",
        ),
        "deflateRaw" => encode(
            DeflateEncoder::new(Vec::new(), compression(level)?),
            data,
            DeflateEncoder::finish,
            "deflateRaw",
        ),
        "gzip" => encode(
            GzEncoder::new(Vec::new(), compression(level)?),
            data,
            GzEncoder::finish,
            "gzip",
        ),
        "inflate" => bounded_decode(ZlibDecoder::new(data), "inflate"),
        "inflateRaw" => bounded_decode(DeflateDecoder::new(data), "inflateRaw"),
        "gunzip" => bounded_decode(GzDecoder::new(data), "gunzip"),
        "unzip" => {
            if data.starts_with(&[0x1f, 0x8b]) {
                bounded_decode(GzDecoder::new(data), "unzip")
            } else {
                bounded_decode(ZlibDecoder::new(data), "unzip")
            }
        }
        other => Err(format!("zlib: unknown codec op `{other}`")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &[u8] = b"the quick brown fox jumps over the lazy dog, twice: \
                            the quick brown fox jumps over the lazy dog";

    #[test]
    fn round_trips_every_format() {
        for (enc, dec) in [
            ("deflate", "inflate"),
            ("deflateRaw", "inflateRaw"),
            ("gzip", "gunzip"),
        ] {
            let compressed = zlib_op(enc, SAMPLE, None).unwrap();
            assert_ne!(compressed, SAMPLE);
            let restored = zlib_op(dec, &compressed, None).unwrap();
            assert_eq!(restored, SAMPLE, "{enc}/{dec} round trip");
        }
    }

    #[test]
    fn unzip_autodetects_gzip_and_zlib() {
        let gz = zlib_op("gzip", SAMPLE, None).unwrap();
        assert_eq!(zlib_op("unzip", &gz, None).unwrap(), SAMPLE);
        let zl = zlib_op("deflate", SAMPLE, None).unwrap();
        assert_eq!(zlib_op("unzip", &zl, None).unwrap(), SAMPLE);
    }

    #[test]
    fn compression_is_deterministic_and_level_aware() {
        let a = zlib_op("gzip", SAMPLE, Some(9)).unwrap();
        let b = zlib_op("gzip", SAMPLE, Some(9)).unwrap();
        assert_eq!(a, b);
        // Level 0 stores; level 9 compresses this redundant sample smaller.
        let stored = zlib_op("deflate", SAMPLE, Some(0)).unwrap();
        let best = zlib_op("deflate", SAMPLE, Some(9)).unwrap();
        assert!(best.len() < stored.len());
    }

    #[test]
    fn invalid_level_and_op_error() {
        assert!(zlib_op("deflate", SAMPLE, Some(10))
            .unwrap_err()
            .contains("invalid compression level"));
        assert!(zlib_op("shrink", SAMPLE, None)
            .unwrap_err()
            .contains("unknown codec op"));
    }

    #[test]
    fn corrupt_input_fails_cleanly() {
        let err = zlib_op("gunzip", b"not gzip at all", None).unwrap_err();
        assert!(err.contains("gunzip"), "{err}");
    }
}
