//! Delta-sync object layout and reconstruction (v0.9.0).
//!
//! Each session is stored on OSS / S3 / MinIO as a small set of objects:
//!
//! ```text
//! {tool}/{project_key}/{session_id}.age                              ← base (full plaintext)
//! {tool}/{project_key}/{session_id}.delta-{seq:04}-{device_id}.age   ← append-only delta
//! {tool}/{project_key}/{session_id}.age.meta.json                    ← decrypted sidecar (unchanged)
//! ```
//!
//! Push decides between writing a new base (first push, file shrank, or
//! compaction triggered) and appending a delta. Pull reassembles by listing all
//! `.age` objects for the session, sorting deltas by `(seq, device_id)`, and
//! concatenating base + every delta after decrypt + maybe_gunzip.
//!
//! The `{device_id}` suffix on deltas avoids cross-machine PUT collisions
//! without requiring backend-specific conditional PUT (`If-None-Match`) — every
//! delta key is naturally unique per pusher. Tradeoff: if two devices are
//! actively writing the same session concurrently (rare for Claude Code, which
//! treats each session as belonging to one device at a time), the reconstructed
//! content interleaves both devices' appends in (seq, device_id) order, which
//! may not match the user's intent. Users who care about that case should use
//! `--fork-on-conflict`.

use crate::adapter::storage::{StorageAdapter, StorageObject};
use crate::compress;
use crate::crypto;
use crate::error::{Result as SessyncResult, SessyncError};

/// Compaction trigger: once this many deltas accumulate, the next push rewrites
/// the base and deletes all deltas. Keeps pull-time list / GET count bounded
/// and amortizes the worst-case reconstruction cost.
pub const COMPACTION_DELTA_COUNT: usize = 10;

/// Short device tag for delta key suffixes. First 8 hex digits of the device
/// UUID — 32 bits of entropy is enough to distinguish a user's handful of
/// machines while keeping object keys readable.
pub fn device_id_short(device_id: &str) -> String {
    device_id
        .chars()
        .filter(|c| c.is_ascii_hexdigit())
        .take(8)
        .collect()
}

/// v0.11.0: OSS layout reorganized so each session lives in its own
/// directory.  Listing a single session is now a prefix list of just one
/// directory (cheaper than scanning the whole tool prefix), and OSS
/// browser UI shows a clean folder-per-session structure instead of
/// hundreds of flat keys.
///
/// Layout:
/// ```text
/// sessync/<tool>/<session_id>/base.age
/// sessync/<tool>/<session_id>/meta.json
/// sessync/<tool>/<session_id>/delta-{seq:04}-{device}.age
/// ```
///
/// Migration from v0.10 flat layout is handled by `sessync migrate-oss-layout`.
///
/// All callers MUST construct OSS keys via these helpers — never with
/// `format!()` inline — to keep layout changes containable to this file.
/// (v0.10 hit 4 hotfixes from inline `format!()` callers being missed
/// during the previous layout change.)
pub fn session_prefix(tool: &str, session_id: &str) -> String {
    format!("{tool}/{session_id}/")
}

/// Base object key: `{tool}/{session_id}/base.age`.
pub fn base_key(tool: &str, session_id: &str) -> String {
    format!("{tool}/{session_id}/base.age")
}

/// Meta sidecar key: `{tool}/{session_id}/meta.json`.
pub fn meta_key(tool: &str, session_id: &str) -> String {
    format!("{tool}/{session_id}/meta.json")
}

/// Delta object key: `{tool}/{session_id}/delta-{seq:04}-{device}.age`.
pub fn delta_key(
    tool: &str,
    session_id: &str,
    seq: u32,
    device_id_short: &str,
) -> String {
    format!(
        "{tool}/{session_id}/delta-{seq:04}-{device_id_short}.age",
    )
}

/// Extract session_id from any v0.11-layout OSS key:
/// `<tool>/<session_id>/<anything>` → Some(session_id).
/// Returns None for keys that don't match (e.g., still-unmigrated v0.10 flat
/// keys, or the salt file).
pub fn session_id_from_key(key: &str) -> Option<String> {
    let mut parts = key.splitn(3, '/');
    let _tool = parts.next()?;
    let sid = parts.next()?;
    parts.next()?; // ensure there IS a filename — rejects "<tool>/<sid>" (v0.10 flat)
    if sid.is_empty() {
        return None;
    }
    Some(sid.to_string())
}

/// Extract session_id from a base key, accepting either v0.11 or v0.10 layout.
/// Used by callers that have the base StorageObject in hand and need to compute
/// related keys (meta, deltas) via the standard delta:: helpers.
pub fn session_id_from_base_key(tool: &str, base_key: &str) -> Option<String> {
    let rest = base_key.strip_prefix(&format!("{tool}/"))?;
    // v0.11: "<sid>/base.age"
    if let Some(sid) = rest.strip_suffix("/base.age") {
        return Some(sid.to_string());
    }
    // v0.10: "<sid>.age"
    rest.strip_suffix(".age").map(|s| s.to_string())
}

/// If `key` looks like a delta key, return `(seq, device_id_short)`. Else None.
///
/// v0.11 filename is `delta-{seq:04}-{device}.age` (no embedded sid since the
/// sid is the parent dir).  v0.10 filename was `<sid>.delta-{seq:04}-{device}.age`.
/// Accept both — the migration command produces v0.11 paths but during the
/// transition a few stragglers may remain.
pub fn parse_delta_key(key: &str) -> Option<(u32, String)> {
    let filename = key.rsplit('/').next()?;
    let stripped = filename.strip_suffix(".age")?;
    // Two acceptable forms:
    //   v0.11: stripped == "delta-{seq:04}-{device}"
    //   v0.10: stripped == "<sid>.delta-{seq:04}-{device}"
    let suffix = if let Some(rest) = stripped.strip_prefix("delta-") {
        rest
    } else {
        let pos = stripped.rfind(".delta-")?;
        &stripped[pos + ".delta-".len()..]
    };
    // suffix == "{seq:04}-{device}"
    let mut parts = suffix.splitn(2, '-');
    let seq_str = parts.next()?;
    let device = parts.next()?;
    let seq = seq_str.parse::<u32>().ok()?;
    Some((seq, device.to_string()))
}

/// True iff `key` is a meta sidecar (v0.11 `<sid>/meta.json` or v0.10 `<sid>.meta.json`).
pub fn is_meta_key(key: &str) -> bool {
    let Some(filename) = key.rsplit('/').next() else {
        return false;
    };
    filename == "meta.json" || filename.ends_with(".meta.json")
}

/// True iff `key` is a session base (`.age`) — not a delta, not a meta sidecar.
///
/// v0.11 base filename is exactly "base.age".  v0.10 base filename was
/// "<sid>.age".  Accept both for transitional compatibility.
pub fn is_base_key(key: &str) -> bool {
    let Some(filename) = key.rsplit('/').next() else {
        return false;
    };
    filename == "base.age"
        || (filename.ends_with(".age")
            && !filename.starts_with("delta-")
            && !filename.contains(".delta-")
            && !filename.ends_with(".meta.json"))
}

/// Result of sifting a flat object list down to one session's layout.
pub struct SessionLayout<'a> {
    pub base: Option<&'a StorageObject>,
    /// (seq, device_id_short, object) — sorted ascending by (seq, device_id_short).
    pub deltas: Vec<(u32, String, &'a StorageObject)>,
}

impl<'a> SessionLayout<'a> {
    pub fn delta_count(&self) -> usize {
        self.deltas.len()
    }

    /// Highest delta seq seen, or 0 if no deltas exist.
    pub fn max_delta_seq(&self) -> u32 {
        self.deltas.iter().map(|(s, _, _)| *s).max().unwrap_or(0)
    }

    /// The "latest" remote object for this session — used to derive a single
    /// representative ETag/mtime that pull's skip check can compare against.
    /// Latest delta wins; falls back to base if no deltas.
    pub fn latest_object(&self) -> Option<&'a StorageObject> {
        self.deltas.last().map(|(_, _, o)| *o).or(self.base)
    }
}

/// Extract a single session's base + deltas from a flat list response.
///
/// v0.10.0: signature no longer takes `project_key` — same session_id from
/// any cwd context shares the same OSS path, and all devices' deltas are
/// collected together into one layout for reconstruction.
pub fn find_session_layout<'a>(
    all_objects: &'a [StorageObject],
    tool: &str,
    session_id: &str,
) -> SessionLayout<'a> {
    let session_dir = session_prefix(tool, session_id); // "<tool>/<sid>/"

    let mut base: Option<&StorageObject> = None;
    let mut deltas: Vec<(u32, String, &StorageObject)> = Vec::new();

    for obj in all_objects {
        // v0.11: every key for this session starts with "<tool>/<sid>/".
        // For transitional compatibility with v0.10 flat layout, also accept
        // keys that match the legacy "<tool>/<sid>.{age,delta-...}" shape.
        let belongs = obj.key.starts_with(&session_dir)
            || obj.key == format!("{tool}/{session_id}.age")
            || obj.key.starts_with(&format!("{tool}/{session_id}.delta-"));
        if !belongs {
            continue;
        }
        if is_base_key(&obj.key) {
            base = Some(obj);
        } else if let Some((seq, dev)) = parse_delta_key(&obj.key) {
            deltas.push((seq, dev, obj));
        }
    }
    deltas.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    SessionLayout { base, deltas }
}

/// Download, decrypt, decompress, and concatenate base + all deltas in order.
pub async fn reconstruct<S: StorageAdapter>(
    storage: &S,
    key: &[u8; 32],
    layout: &SessionLayout<'_>,
) -> SessyncResult<Vec<u8>> {
    let Some(base) = layout.base else {
        return Err(SessyncError::Storage(
            "session has no base object — cannot reconstruct".to_string(),
        ));
    };

    let mut out: Vec<u8> = Vec::new();

    let base_ct = storage.get(&base.key).await?;
    let base_pt = crypto::decrypt(&base_ct, key)?;
    let base_raw = compress::maybe_gunzip(&base_pt)
        .map_err(|e| SessyncError::Storage(format!("gunzip base {}: {e}", base.key)))?;
    out.extend_from_slice(&base_raw);

    for (_seq, _dev, obj) in &layout.deltas {
        let ct = storage.get(&obj.key).await?;
        let pt = crypto::decrypt(&ct, key)?;
        let raw = compress::maybe_gunzip(&pt)
            .map_err(|e| SessyncError::Storage(format!("gunzip delta {}: {e}", obj.key)))?;
        out.extend_from_slice(&raw);
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn obj(key: &str) -> StorageObject {
        StorageObject {
            key: key.to_string(),
            last_modified: Utc::now(),
            size: 0,
            etag: Some("etag".into()),
        }
    }

    #[test]
    fn device_id_short_strips_hyphens_and_truncates() {
        let id = "1f36aa73-46a5-45c6-85e2-01486168fa4c";
        assert_eq!(device_id_short(id), "1f36aa73");
    }

    // v0.10.0: tests use the new project_key-free OSS path layout
    //   `<tool>/<session_id>.{age, delta-N-DEV.age, meta.json}`

    // v0.11.0 tests use the new per-session subdirectory layout:
    //   <tool>/<session_id>/{base.age, meta.json, delta-N-DEV.age}

    #[test]
    fn key_builders_round_trip_with_parser() {
        let k = delta_key("claude-code", "sess-001", 5, "abc12345");
        assert_eq!(k, "claude-code/sess-001/delta-0005-abc12345.age");
        let (seq, dev) = parse_delta_key(&k).unwrap();
        assert_eq!(seq, 5);
        assert_eq!(dev, "abc12345");
    }

    #[test]
    fn key_helpers_use_v011_layout() {
        assert_eq!(base_key("claude-code", "X"), "claude-code/X/base.age");
        assert_eq!(meta_key("claude-code", "X"), "claude-code/X/meta.json");
        assert_eq!(session_prefix("claude-code", "X"), "claude-code/X/");
    }

    #[test]
    fn parse_delta_key_accepts_v010_legacy() {
        // Old layout (v0.10) still parses for transitional compatibility.
        let (seq, dev) = parse_delta_key("claude-code/sess-001.delta-0005-abc12345.age").unwrap();
        assert_eq!(seq, 5);
        assert_eq!(dev, "abc12345");
    }

    #[test]
    fn parse_delta_key_rejects_non_delta_keys() {
        assert_eq!(parse_delta_key("foo/sess/base.age"), None);
        assert_eq!(parse_delta_key("foo/sess/meta.json"), None);
        assert_eq!(parse_delta_key("foo/sess/delta-abc.age"), None); // seq not numeric
    }

    #[test]
    fn is_base_key_matches_v011_layout() {
        assert!(is_base_key("claude-code/sess/base.age"));
        assert!(!is_base_key("claude-code/sess/delta-0001-abc.age"));
        assert!(!is_base_key("claude-code/sess/meta.json"));
        assert!(!is_base_key("claude-code/random.json"));
        // Legacy v0.10 base still recognized
        assert!(is_base_key("claude-code/sess.age"));
    }

    #[test]
    fn session_id_from_key_extracts_sid() {
        assert_eq!(
            session_id_from_key("claude-code/abc-123/base.age").as_deref(),
            Some("abc-123")
        );
        assert_eq!(
            session_id_from_key("claude-code/abc-123/delta-0001-dev.age").as_deref(),
            Some("abc-123")
        );
        // Pre-v0.11 flat key — no sub-directory, return None to indicate
        // unmigrated leftover (caller can either skip or trigger migration).
        assert_eq!(session_id_from_key("claude-code/sess.age"), None);
        assert_eq!(session_id_from_key("not-a-key"), None);
    }

    #[test]
    fn find_session_layout_groups_base_and_deltas() {
        let all = vec![
            obj("claude-code/sess-001/base.age"),
            obj("claude-code/sess-001/delta-0002-aaa.age"),
            obj("claude-code/sess-001/delta-0001-bbb.age"),
            obj("claude-code/sess-001/delta-0003-aaa.age"),
            obj("claude-code/sess-001/meta.json"),
            obj("claude-code/other-session/base.age"),
        ];
        let layout = find_session_layout(&all, "claude-code", "sess-001");
        assert!(layout.base.is_some());
        assert_eq!(layout.delta_count(), 3);
        let seqs: Vec<u32> = layout.deltas.iter().map(|(s, _, _)| *s).collect();
        assert_eq!(seqs, vec![1, 2, 3]);
        assert_eq!(layout.max_delta_seq(), 3);
    }

    #[test]
    fn find_session_layout_returns_empty_for_unknown_session() {
        let all = vec![obj("claude-code/sess-001/base.age")];
        let layout = find_session_layout(&all, "claude-code", "nope");
        assert!(layout.base.is_none());
        assert_eq!(layout.delta_count(), 0);
        assert_eq!(layout.max_delta_seq(), 0);
    }

    #[test]
    fn latest_object_prefers_delta_over_base() {
        let all = vec![
            obj("claude-code/sess-001/base.age"),
            obj("claude-code/sess-001/delta-0001-aaa.age"),
        ];
        let layout = find_session_layout(&all, "claude-code", "sess-001");
        let latest = layout.latest_object().unwrap();
        assert!(latest.key.contains("delta-"));
    }

    #[test]
    fn latest_object_falls_back_to_base_when_no_deltas() {
        let all = vec![obj("claude-code/sess-001/base.age")];
        let layout = find_session_layout(&all, "claude-code", "sess-001");
        let latest = layout.latest_object().unwrap();
        assert_eq!(latest.key, "claude-code/sess-001/base.age");
    }

    // v0.10.0: critical new property — find_session_layout merges deltas from
    // multiple devices (different DEV-id suffix) under the same session_id.
    // This is what enables cross-device session continuation: each device
    // appends its own delta sequence, reconstruction concatenates all.
    #[test]
    fn find_session_layout_merges_deltas_from_multiple_devices() {
        let all = vec![
            obj("claude-code/sess-cross.age"),
            obj("claude-code/sess-cross.delta-0001-mini1234.age"),
            obj("claude-code/sess-cross.delta-0002-mini1234.age"),
            obj("claude-code/sess-cross.delta-0001-pro56789.age"), // pro's device, same seq is OK
            obj("claude-code/sess-cross.delta-0002-pro56789.age"),
        ];
        let layout = find_session_layout(&all, "claude-code", "sess-cross");
        assert_eq!(
            layout.delta_count(),
            4,
            "all deltas from both devices must be in layout for reconstruction"
        );
        // Verify both device IDs appear among the deltas
        let devs: std::collections::HashSet<String> =
            layout.deltas.iter().map(|(_, d, _)| d.clone()).collect();
        assert!(devs.contains("mini1234"));
        assert!(devs.contains("pro56789"));
    }
}
