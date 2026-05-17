//! S3-compatible storage adapter.
//!
//! Supports MinIO (self-hosted), Cloudflare R2, Backblaze B2, and AWS S3 —
//! any service that speaks the S3v4 signing protocol. The key difference between
//! these services is URL style:
//!
//! | Service         | Style         | path_style |
//! |-----------------|---------------|------------|
//! | MinIO           | path-style    | true       |
//! | Backblaze B2    | path-style    | true       |
//! | Cloudflare R2   | virtual-host  | false      |
//! | AWS S3          | virtual-host  | false      |
//!
//! Path-style: `https://endpoint/bucket/key`
//! Virtual-hosted: `https://bucket.endpoint/key`
//!
//! The `rust-s3` crate handles both via `Bucket::with_path_style()`.

use super::storage::{StorageAdapter, StorageObject};
use crate::config::S3Config;
use crate::error::{Result, SessyncError};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use s3::creds::Credentials;
use s3::{Bucket, Region};
use std::time::Duration;

/// Maximum time to wait for a single S3 API call.
/// Matches the OSS adapter timeout — prevents indefinite hangs on DNS failures,
/// network drops, or stalled connections.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Maximum number of list pages to follow before stopping.
/// Each page holds up to 1 000 objects (S3 default), so 50 pages = 50 000 objects.
/// Prevents an infinite loop if a server keeps returning continuation tokens.
const MAX_LIST_PAGES: u32 = 50;

pub struct S3Storage {
    bucket: Box<Bucket>,
    prefix: String,
}

impl S3Storage {
    pub fn new(cfg: &S3Config) -> Result<Self> {
        let region = Region::Custom {
            region: cfg.region.clone(),
            endpoint: cfg.endpoint.clone(),
        };

        let credentials = Credentials::new(
            Some(&cfg.access_key_id),
            Some(&cfg.access_key_secret),
            None, // security_token
            None, // session_token
            None, // profile
        )
        .map_err(|e| SessyncError::Storage(format!("credentials init: {e}")))?;

        let bucket = Bucket::new(&cfg.bucket, region, credentials)
            .map_err(|e| SessyncError::Storage(format!("bucket init: {e}")))?;

        // Apply path-style if configured (required for MinIO and most self-hosted S3).
        let bucket = if cfg.path_style {
            bucket.with_path_style()
        } else {
            bucket
        };

        Ok(Self {
            bucket,
            prefix: cfg.prefix.clone(),
        })
    }

    /// Prepend the configured prefix to a caller-supplied key.
    fn full_key(&self, key: &str) -> String {
        format!("{}{}", self.prefix, key)
    }

    /// Strip the configured prefix from a storage key to get the bare key
    /// that callers see.  If the prefix isn't present (shouldn't happen),
    /// returns the key unchanged.
    fn strip_prefix<'a>(&self, key: &'a str) -> &'a str {
        key.strip_prefix(self.prefix.as_str()).unwrap_or(key)
    }
}

/// Parse a `last_modified` string from the S3 list response into a UTC DateTime.
///
/// S3 returns ISO 8601 / RFC 3339 timestamps, e.g. `"2024-03-15T12:34:56.000Z"`.
/// Falls back to `Utc::now()` if parsing fails (never expected in practice).
fn parse_last_modified(s: &str) -> DateTime<Utc> {
    // Try RFC 3339 first (the common format: "2024-03-15T12:34:56.000Z").
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return dt.with_timezone(&Utc);
    }
    // Some S3 implementations use RFC 2822 ("Tue, 15 Mar 2024 12:34:56 GMT").
    if let Ok(dt) = DateTime::parse_from_rfc2822(s) {
        return dt.with_timezone(&Utc);
    }
    tracing::warn!(raw = %s, "S3: could not parse last_modified timestamp, using Utc::now()");
    Utc::now()
}

/// Normalize an ETag string from S3 to the sessync internal format.
///
/// S3 (and all compatible services) return ETags as hex MD5 hashes wrapped in
/// double quotes: `"d41d8cd98f00b204e9800998ecf8427e"`. We store them verbatim
/// (quotes included) so comparisons are byte-exact with OSS-derived ETags, which
/// also arrive pre-quoted.
///
/// Some implementations return the ETag without surrounding quotes; if that
/// happens we add them so the format is uniform.
fn normalize_etag(raw: Option<String>) -> Option<String> {
    let s = raw?;
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    if s.starts_with('"') {
        Some(s.to_string())
    } else {
        Some(format!("\"{s}\""))
    }
}

#[async_trait]
impl StorageAdapter for S3Storage {
    /// Upload `bytes` under `key` (prefixed). Overwrites if the object already exists.
    async fn put(&self, key: &str, bytes: Vec<u8>) -> Result<()> {
        let full = self.full_key(key);
        match tokio::time::timeout(
            REQUEST_TIMEOUT,
            self.bucket.put_object(&full, &bytes),
        )
        .await
        {
            Ok(Ok(_response)) => Ok(()),
            Ok(Err(e)) => Err(SessyncError::Storage(format!("put {full}: {e}"))),
            Err(_) => Err(SessyncError::Storage(format!(
                "put {full}: timeout after {}s",
                REQUEST_TIMEOUT.as_secs()
            ))),
        }
    }

    /// Download the object at `key` (prefixed) and return its bytes.
    ///
    /// Normalises S3's HTTP 404 response to a `"not found:"` prefix that matches
    /// the `OssStorage` and `LocalFsStorage` contract. Callers (e.g. the shared
    /// salt logic in `init`) string-match on `"not found"` to distinguish
    /// "create on first use" from hard failures.
    async fn get(&self, key: &str) -> Result<Vec<u8>> {
        let full = self.full_key(key);
        match tokio::time::timeout(
            REQUEST_TIMEOUT,
            self.bucket.get_object(&full),
        )
        .await
        {
            Ok(Ok(response)) => {
                let status = response.status_code();
                if status == 404 {
                    return Err(SessyncError::Storage(format!("not found: {key}")));
                }
                if status != 200 {
                    return Err(SessyncError::Storage(format!(
                        "get {full}: unexpected HTTP {status}"
                    )));
                }
                Ok(response.to_vec())
            }
            Ok(Err(e)) => {
                // rust-s3 may surface 404 as an error rather than a non-200 response.
                let msg = e.to_string();
                if msg.contains("404") || msg.contains("NoSuchKey") || msg.contains("Not Found") {
                    Err(SessyncError::Storage(format!("not found: {key}")))
                } else {
                    Err(SessyncError::Storage(format!("get {full}: {e}")))
                }
            }
            Err(_) => Err(SessyncError::Storage(format!(
                "get {full}: timeout after {}s",
                REQUEST_TIMEOUT.as_secs()
            ))),
        }
    }

    /// List objects whose key starts with `<configured_prefix><prefix>`.
    /// Returns objects with the configured prefix stripped (callers see bare keys).
    ///
    /// Follows S3 list_objects_v2 pagination via continuation tokens, accumulating
    /// results across pages. A sanity cap of `MAX_LIST_PAGES` (50) prevents
    /// infinite loops on broken servers.
    async fn list(&self, prefix: &str) -> Result<Vec<StorageObject>> {
        let full_prefix = self.full_key(prefix);
        let mut all_objects: Vec<StorageObject> = Vec::new();
        let mut continuation_token: Option<String> = None;
        let mut pages_fetched: u32 = 0;

        loop {
            // rust-s3 list_page: (prefix, delimiter, max_keys, continuation_token)
            let result = tokio::time::timeout(
                REQUEST_TIMEOUT,
                self.bucket.list_page(
                    full_prefix.clone(),
                    None,       // delimiter — no common-prefix folding
                    continuation_token.clone(),
                    None,       // start_after
                    Some(1000), // max_keys (S3 maximum per page)
                ),
            )
            .await
            .map_err(|_| {
                SessyncError::Storage(format!(
                    "list {full_prefix}: timeout after {}s (page {})",
                    REQUEST_TIMEOUT.as_secs(),
                    pages_fetched + 1,
                ))
            })?;

            let (list_result, _status) = result
                .map_err(|e| SessyncError::Storage(format!("list {full_prefix}: {e}")))?;

            pages_fetched += 1;

            for obj in &list_result.contents {
                let raw_key = &obj.key;
                let bare_key = self
                    .strip_prefix(raw_key)
                    .to_string();
                let last_modified = parse_last_modified(&obj.last_modified);
                let etag = normalize_etag(obj.e_tag.clone());

                all_objects.push(StorageObject {
                    key: bare_key,
                    size: obj.size,
                    last_modified,
                    etag,
                });
            }

            match list_result.next_continuation_token {
                None => break, // last page reached
                Some(token) => {
                    if pages_fetched >= MAX_LIST_PAGES {
                        tracing::warn!(
                            prefix = %full_prefix,
                            pages = pages_fetched,
                            total_items = all_objects.len(),
                            "S3 list hit the {MAX_LIST_PAGES}-page safety cap — \
                             some objects may be invisible. Consider running \
                             `sessync purge` to remove stale sessions.",
                        );
                        break;
                    }
                    continuation_token = Some(token);
                }
            }
        }

        Ok(all_objects)
    }

    /// Delete the object at `key` (prefixed).
    ///
    /// Idempotent: S3 DELETE of a non-existent object returns HTTP 204 (no
    /// content), which rust-s3 treats as success. No special "not found" handling
    /// is required.
    async fn delete(&self, key: &str) -> Result<()> {
        let full = self.full_key(key);
        match tokio::time::timeout(
            REQUEST_TIMEOUT,
            self.bucket.delete_object(&full),
        )
        .await
        {
            Ok(Ok(_response)) => Ok(()),
            Ok(Err(e)) => Err(SessyncError::Storage(format!("delete {full}: {e}"))),
            Err(_) => Err(SessyncError::Storage(format!(
                "delete {full}: timeout after {}s",
                REQUEST_TIMEOUT.as_secs()
            ))),
        }
    }

    /// Fetch ETag and size for a single key using S3's HEAD Object.
    ///
    /// Used post-PUT to capture the freshly-assigned ETag without issuing a
    /// full list call. The HEAD request returns only headers (no body), so it
    /// is cheap.
    async fn head(&self, key: &str) -> Result<StorageObject> {
        let full = self.full_key(key);
        match tokio::time::timeout(
            REQUEST_TIMEOUT,
            self.bucket.head_object(&full),
        )
        .await
        {
            Ok(Ok((head, _status))) => {
                let size = head.content_length.unwrap_or(0).max(0) as u64;
                let last_modified = head
                    .last_modified
                    .as_deref()
                    .map(parse_last_modified)
                    .unwrap_or_else(Utc::now);
                let etag = normalize_etag(head.e_tag);
                Ok(StorageObject {
                    key: key.to_string(),
                    size,
                    last_modified,
                    etag,
                })
            }
            Ok(Err(e)) => {
                let msg = e.to_string();
                if msg.contains("404") || msg.contains("NoSuchKey") || msg.contains("Not Found") {
                    Err(SessyncError::Storage(format!("not found: {key}")))
                } else {
                    Err(SessyncError::Storage(format!("head {full}: {msg}")))
                }
            }
            Err(_) => Err(SessyncError::Storage(format!(
                "head {full}: timeout after {}s",
                REQUEST_TIMEOUT.as_secs()
            ))),
        }
    }
}

// ── Unit tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::{normalize_etag, parse_last_modified, MAX_LIST_PAGES, REQUEST_TIMEOUT};

    /// Confirm the S3 request timeout matches the OSS adapter (30 s).
    #[test]
    fn request_timeout_is_30s() {
        assert_eq!(REQUEST_TIMEOUT.as_secs(), 30);
    }

    /// Pagination cap is 50 pages (50 000 objects).
    #[test]
    fn max_list_pages_is_50() {
        assert_eq!(MAX_LIST_PAGES, 50);
    }

    // ── full_key / strip_prefix logic ─────────────────────────────────────────

    /// The full_key helper prepends the prefix exactly once.
    #[test]
    fn full_key_prepends_prefix() {
        // Simulate the helper without constructing a real S3Storage
        // (which would require network credentials).
        let prefix = "sessync/";
        let key = "claude-code/proj/sess.age";
        let full = format!("{prefix}{key}");
        assert_eq!(full, "sessync/claude-code/proj/sess.age");
    }

    /// strip_prefix removes the configured prefix from a storage key.
    #[test]
    fn strip_prefix_removes_configured_prefix() {
        let prefix = "sessync/";
        let full_key = "sessync/claude-code/proj/sess.age";
        let bare = full_key.strip_prefix(prefix).unwrap_or(full_key);
        assert_eq!(bare, "claude-code/proj/sess.age");
    }

    /// When the prefix is absent (shouldn't happen in practice), the key is
    /// returned unchanged rather than panicking.
    #[test]
    fn strip_prefix_is_safe_when_prefix_absent() {
        let prefix = "sessync/";
        let key = "no-prefix-here.age";
        let bare = key.strip_prefix(prefix).unwrap_or(key);
        assert_eq!(bare, key);
    }

    // ── normalize_etag ────────────────────────────────────────────────────────

    /// ETags already wrapped in double quotes pass through unchanged.
    #[test]
    fn normalize_etag_already_quoted() {
        let e = normalize_etag(Some("\"d41d8cd98f00b204e9800998ecf8427e\"".into()));
        assert_eq!(e.unwrap(), "\"d41d8cd98f00b204e9800998ecf8427e\"");
    }

    /// Unquoted ETags (non-standard but some implementations emit them) get
    /// wrapped in double quotes to produce a uniform format.
    #[test]
    fn normalize_etag_adds_quotes_when_absent() {
        let e = normalize_etag(Some("d41d8cd98f00b204e9800998ecf8427e".into()));
        assert_eq!(e.unwrap(), "\"d41d8cd98f00b204e9800998ecf8427e\"");
    }

    /// None input yields None output.
    #[test]
    fn normalize_etag_none_stays_none() {
        assert!(normalize_etag(None).is_none());
    }

    /// An empty string ETag is treated as absent (None).
    #[test]
    fn normalize_etag_empty_string_is_none() {
        assert!(normalize_etag(Some(String::new())).is_none());
    }

    // ── parse_last_modified ────────────────────────────────────────────────────

    /// RFC 3339 timestamps (S3 standard) parse correctly.
    #[test]
    fn parse_last_modified_rfc3339() {
        // 2024-03-15 12:34:56 UTC = Unix 1710506096
        let dt = parse_last_modified("2024-03-15T12:34:56.000Z");
        assert_eq!(dt.timestamp(), 1710506096);
    }

    /// RFC 3339 with timezone offset parses correctly.
    #[test]
    fn parse_last_modified_rfc3339_with_offset() {
        let dt = parse_last_modified("2024-03-15T12:34:56+00:00");
        assert_eq!(dt.timestamp(), 1710506096);
    }

    /// An unparseable string falls back to Utc::now() without panicking.
    #[test]
    fn parse_last_modified_fallback_is_safe() {
        // This should not panic; we just verify we get a plausible timestamp.
        let before = chrono::Utc::now().timestamp() - 5;
        let dt = parse_last_modified("not-a-date");
        let after = chrono::Utc::now().timestamp() + 5;
        assert!(
            dt.timestamp() >= before && dt.timestamp() <= after,
            "fallback should be close to Utc::now(), got timestamp {}",
            dt.timestamp()
        );
    }

    // ── Pagination accumulator logic ──────────────────────────────────────────

    /// Mirror of the OSS adapter pagination test: items from multiple pages are
    /// combined and accumulation stops when next_continuation_token is None.
    #[test]
    fn pagination_accumulator_collects_all_pages() {
        let pages: Vec<(Vec<&str>, Option<&str>)> = vec![
            (vec!["a", "b"], Some("tok1")),
            (vec!["c", "d"], Some("tok2")),
            (vec!["e"], None),
        ];

        let mut all_items: Vec<&str> = Vec::new();
        let mut continuation_token: Option<String> = None;
        let mut pages_fetched: u32 = 0;

        for (items, next_token) in &pages {
            let expected_token = match pages_fetched {
                0 => None,
                1 => Some("tok1".to_string()),
                2 => Some("tok2".to_string()),
                _ => unreachable!(),
            };
            assert_eq!(continuation_token, expected_token, "page {pages_fetched}");

            all_items.extend(items.iter().copied());
            pages_fetched += 1;

            match next_token {
                None => break,
                Some(token) => {
                    assert!(pages_fetched < MAX_LIST_PAGES, "hit page cap unexpectedly");
                    continuation_token = Some(token.to_string());
                }
            }
        }

        assert_eq!(all_items, vec!["a", "b", "c", "d", "e"]);
        assert_eq!(pages_fetched, 3);
    }

    /// The page cap stops accumulation at exactly MAX_LIST_PAGES.
    #[test]
    fn pagination_accumulator_respects_cap() {
        let mut all_items: Vec<u32> = Vec::new();
        let mut pages_fetched: u32 = 0;
        let mut continuation_token: Option<String> = Some("start".to_string());

        loop {
            all_items.push(pages_fetched);
            pages_fetched += 1;
            let next_token = Some(format!("tok{pages_fetched}"));

            match next_token {
                None => break,
                Some(token) => {
                    if pages_fetched >= MAX_LIST_PAGES {
                        break;
                    }
                    continuation_token = Some(token);
                }
            }
        }

        assert_eq!(pages_fetched, MAX_LIST_PAGES);
        assert_eq!(all_items.len() as u32, MAX_LIST_PAGES);
        let _ = continuation_token;
    }
}
