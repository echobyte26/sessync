//! gzip compression with backward-compatible magic-byte detection.
//!
//! Push pipeline (v0.9.0+):  jsonl → gzip → encrypt → upload
//! Pull pipeline:            download → decrypt → maybe_gunzip → write jsonl
//!
//! `maybe_gunzip` checks the first two bytes for the gzip magic number
//! (0x1f 0x8b). Valid JSON / JSONL never starts with these bytes (0x1f is a
//! control char and 0x8b is an invalid UTF-8 start byte), so detection has
//! zero false positives — pre-v0.9.0 raw plaintext passes through unchanged.

use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use std::io::{Read, Write};

/// gzip the input in memory. Cannot fail for `Vec<u8>` writes.
pub fn gzip(plaintext: &[u8]) -> Vec<u8> {
    let mut enc =
        GzEncoder::new(Vec::with_capacity(plaintext.len() / 4), Compression::default());
    enc.write_all(plaintext)
        .expect("gzip write to in-memory Vec cannot fail");
    enc.finish()
        .expect("gzip finish on in-memory Vec cannot fail")
}

/// If `data` starts with the gzip magic bytes, gunzip it. Otherwise return as-is.
///
/// Used on the pull side to seamlessly handle both pre-v0.9.0 (raw jsonl) and
/// v0.9.0+ (gzipped) payloads after decryption.
pub fn maybe_gunzip(data: &[u8]) -> std::io::Result<Vec<u8>> {
    if data.len() < 2 || data[0] != 0x1f || data[1] != 0x8b {
        return Ok(data.to_vec());
    }
    let mut dec = GzDecoder::new(data);
    let mut out = Vec::with_capacity(data.len() * 4);
    dec.read_to_end(&mut out)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gzip_roundtrip() {
        let original = b"hello world\n".repeat(100);
        let compressed = gzip(&original);
        assert!(compressed.len() < original.len(), "compressed must be smaller");
        assert_eq!(compressed[0], 0x1f, "first byte must be gzip magic");
        assert_eq!(compressed[1], 0x8b, "second byte must be gzip magic");
        let decompressed = maybe_gunzip(&compressed).unwrap();
        assert_eq!(decompressed, original);
    }

    #[test]
    fn maybe_gunzip_passthrough_for_raw_jsonl() {
        let jsonl = b"{\"type\":\"user\",\"content\":\"hello\"}\n";
        let result = maybe_gunzip(jsonl).unwrap();
        assert_eq!(result, jsonl);
    }

    #[test]
    fn maybe_gunzip_passthrough_for_short_input() {
        assert_eq!(maybe_gunzip(&[]).unwrap(), Vec::<u8>::new());
        assert_eq!(maybe_gunzip(b"x").unwrap(), b"x");
        assert_eq!(maybe_gunzip(&[0x1f]).unwrap(), vec![0x1f]);
    }

    #[test]
    fn maybe_gunzip_handles_only_first_byte_match() {
        // 0x1f alone (without 0x8b second byte) must not be treated as gzip.
        let data = &[0x1f, 0x42, 0x00];
        assert_eq!(maybe_gunzip(data).unwrap(), data.to_vec());
    }

    #[test]
    fn gzip_compresses_jsonl_well() {
        // Realistic Claude Code jsonl has heavy event-shape repetition.
        let event = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Hello, this is a response from the assistant agent."}]},"timestamp":"2026-05-18T19:00:00Z"}"#;
        let jsonl = format!("{event}\n").repeat(1000);
        let compressed = gzip(jsonl.as_bytes());
        let ratio = jsonl.len() as f64 / compressed.len() as f64;
        assert!(
            ratio > 5.0,
            "realistic jsonl should compress >5x, got {ratio:.1}x"
        );
    }

    #[test]
    fn jsonl_first_bytes_never_collide_with_gzip_magic() {
        // Sanity: assert that common jsonl first-line prefixes are not 0x1f 0x8b.
        for sample in [
            b"{\"type\":\"user\"}\n".as_ref(),
            b"[\n".as_ref(),
            b"null\n".as_ref(),
            b"{}".as_ref(),
        ] {
            assert!(
                sample.len() < 2 || sample[0] != 0x1f || sample[1] != 0x8b,
                "jsonl sample collides with gzip magic: {sample:?}"
            );
        }
    }
}
