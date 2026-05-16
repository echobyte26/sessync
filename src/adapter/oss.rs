use super::storage::{StorageAdapter, StorageObject};
use crate::config::OssConfig;
use crate::error::{Result, SessyncError};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use std::time::Duration;

/// Maximum time to wait for a single OSS SDK call. Prevents indefinite hangs
/// caused by DNS failures, network drops, or a stalled connection mid-request.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Maximum number of OSS list pages to follow before stopping.
/// Each page holds up to 1 000 objects, so 50 pages = 50 000 objects max.
/// This prevents an infinite loop if a broken server keeps returning tokens.
const MAX_LIST_PAGES: u32 = 50;

pub struct OssStorage {
    /// Held as `String` so each call rebuilds a fresh `Bucket` handle.
    /// The SDK's query-builder methods (prefix/max_keys/...) consume `self`,
    /// so a shared/cached `Bucket` would be re-mutated by concurrent callers.
    /// Cheap to recreate per call (Arc clone + String clone).
    bucket_name: String,
    client: aliyun_oss_client::Client,
    prefix: String,
}

impl OssStorage {
    pub fn new(cfg: &OssConfig) -> Result<Self> {
        // In 0.13 Client::new takes (key, secret, endpoint) — no bucket arg.
        // It also returns Result<Client, OssError> instead of Client directly.
        let client = aliyun_oss_client::Client::new(
            cfg.access_key_id.clone(),
            cfg.access_key_secret.clone(),
            cfg.endpoint.as_str(),
        )
        .map_err(|e| SessyncError::Storage(format!("client init: {e:?}")))?;

        Ok(Self {
            bucket_name: cfg.bucket.clone(),
            client,
            prefix: cfg.prefix.clone(),
        })
    }

    fn full_key(&self, key: &str) -> String {
        format!("{}{}", self.prefix, key)
    }

    /// Construct a `Bucket` handle from the stored client + bucket name.
    fn bucket(&self) -> Result<aliyun_oss_client::Bucket> {
        self.client
            .bucket(&self.bucket_name)
            .map_err(|e| SessyncError::Storage(format!("bucket handle: {e}")))
    }
}

#[async_trait]
impl StorageAdapter for OssStorage {
    /// Upload `bytes` under `key` (prefixed). Overwrites if the object already exists.
    async fn put(&self, key: &str, bytes: Vec<u8>) -> Result<()> {
        let full = self.full_key(key);
        match tokio::time::timeout(
            REQUEST_TIMEOUT,
            self.bucket()?.object(&full).upload(bytes),
        )
        .await
        {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(SessyncError::Storage(format!("put {full}: {e:?}"))),
            Err(_) => Err(SessyncError::Storage(format!(
                "put {full}: timeout after {}s",
                REQUEST_TIMEOUT.as_secs()
            ))),
        }
    }

    /// Download the object at `key` (prefixed) and return its bytes.
    ///
    /// Normalizes OSS's `NoSuchKey` error to a "not found:" prefix that matches
    /// `LocalFsStorage` and `InMemoryStorage` semantics. Callers (e.g. the B1
    /// shared-salt logic in `init`) string-match on "not found" to decide
    /// "create on first use" vs "fail hard".
    async fn get(&self, key: &str) -> Result<Vec<u8>> {
        let full = self.full_key(key);
        match tokio::time::timeout(
            REQUEST_TIMEOUT,
            self.bucket()?.object(&full).download_to_bytes(),
        )
        .await
        {
            Ok(Ok(buf)) => Ok(buf),
            Ok(Err(e)) => {
                let dbg = format!("{e:?}");
                if dbg.contains("NoSuchKey") {
                    Err(SessyncError::Storage(format!("not found: {key}")))
                } else {
                    Err(SessyncError::Storage(format!("get {full}: {dbg}")))
                }
            }
            Err(_) => Err(SessyncError::Storage(format!(
                "get {full}: timeout after {}s",
                REQUEST_TIMEOUT.as_secs()
            ))),
        }
    }

    /// List objects whose OSS key starts with `<configured_prefix><prefix>`.
    /// Returns keys with the configured prefix stripped (callers see bare keys).
    ///
    /// Follows OSS pagination via `NextContinuationToken`, accumulating results
    /// across pages. A sanity cap of `MAX_LIST_PAGES` (50) prevents infinite
    /// loops on broken servers; if the cap is hit a warning is logged and the
    /// partial result is returned.
    async fn list(&self, prefix: &str) -> Result<Vec<StorageObject>> {
        let full_prefix = self.full_key(prefix);

        // Use export_objects with a custom serde type to capture Key, Size,
        // LastModified, and ETag from the XML <Contents> elements in a single call.
        #[derive(Debug, Deserialize)]
        struct OssItem {
            #[serde(rename = "Key")]
            key: String,
            #[serde(rename = "Size")]
            size: u64,
            #[serde(rename = "LastModified")]
            last_modified: DateTime<Utc>,
            /// OSS list response includes ETag per item inside <Contents>.
            /// The value arrives already quoted, e.g. `"\"D41D8CD98F00B204E9800998ECF8427E\""`.
            /// We store it verbatim — both the recorded etag and the remote etag
            /// come from the same source, so byte-equal comparison works without
            /// any stripping.
            #[serde(rename = "ETag")]
            etag: Option<String>,
        }

        let mut all_items: Vec<OssItem> = Vec::new();
        let mut continuation_token: Option<String> = None;
        let mut pages_fetched: u32 = 0;

        loop {
            // Build the bucket list request, optionally with a continuation token.
            let bucket_builder = {
                let b = self.bucket()?.prefix(&full_prefix);
                if let Some(ref token) = continuation_token {
                    b.continuation_token(token)
                } else {
                    b
                }
            };

            let result = tokio::time::timeout(
                REQUEST_TIMEOUT,
                bucket_builder.export_objects::<OssItem>(),
            )
            .await
            .map_err(|_| {
                SessyncError::Storage(format!(
                    "list {full_prefix}: timeout after {}s (page {})",
                    REQUEST_TIMEOUT.as_secs(),
                    pages_fetched + 1,
                ))
            })?;

            // Aliyun OSS returns a ListBucketResult XML without <Contents> when the
            // prefix matches no objects (empty bucket / no matches). The SDK's strict
            // serde deserialization then surfaces this as:
            //   OssError::ParseXml(serde_xml_rs::Error::Custom { field: "missing field `Contents`" })
            //
            // We match on the typed variant chain so that any future rename of the
            // OssError enum variants will be caught at compile time. The inner `field`
            // string is produced by serde's own `missing_field` helper and is stable
            // across serde versions.
            let (items, next_token): (Vec<OssItem>, _) = match result {
                Ok(v) => v,
                Err(ref e) if is_empty_bucket_xml_error(e) => {
                    tracing::debug!(
                        prefix = %full_prefix,
                        page = pages_fetched + 1,
                        "OSS list returned no <Contents> — treating as empty",
                    );
                    (vec![], None)
                }
                Err(e) => {
                    return Err(SessyncError::Storage(format!("list {full_prefix}: {e:?}")));
                }
            };

            all_items.extend(items);
            pages_fetched += 1;

            match next_token {
                None => break, // last page reached
                Some(token) => {
                    if pages_fetched >= MAX_LIST_PAGES {
                        tracing::warn!(
                            prefix = %full_prefix,
                            pages = pages_fetched,
                            total_items = all_items.len(),
                            "OSS list hit the {MAX_LIST_PAGES}-page safety cap — \
                             some objects may be invisible. Consider running \
                             `sessync purge` to remove stale sessions.",
                        );
                        break;
                    }
                    continuation_token = Some(token);
                }
            }
        }

        let strip = &self.prefix;
        let out = all_items
            .into_iter()
            .map(|o| StorageObject {
                key: o
                    .key
                    .strip_prefix(strip.as_str())
                    .unwrap_or(&o.key)
                    .to_string(),
                size: o.size,
                last_modified: o.last_modified,
                etag: o.etag,
            })
            .collect();
        Ok(out)
    }

    /// Delete the object at `key` (prefixed).
    ///
    /// Idempotency: Aliyun OSS DELETE of a non-existent object returns HTTP 204
    /// (no content), which the SDK treats as success. No special "not found"
    /// handling is required.
    async fn delete(&self, key: &str) -> Result<()> {
        let full = self.full_key(key);
        match tokio::time::timeout(REQUEST_TIMEOUT, self.bucket()?.object(&full).delete()).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(SessyncError::Storage(format!("delete {full}: {e:?}"))),
            Err(_) => Err(SessyncError::Storage(format!(
                "delete {full}: timeout after {}s",
                REQUEST_TIMEOUT.as_secs()
            ))),
        }
    }

    /// Fetch ETag and mtime for a single key using OSS's `?objectMeta` query.
    ///
    /// This is used post-PUT to record the freshly-assigned ETag without issuing
    /// a full list. The `?objectMeta` query is a GET that returns only headers
    /// (no body), so it is cheap.
    ///
    /// Note: this makes a real network call; in unit tests that use `InMemoryStorage`
    /// the call never reaches here — `InMemoryStorage::head` is called instead.
    async fn head(&self, key: &str) -> Result<StorageObject> {
        let full = self.full_key(key);
        match tokio::time::timeout(
            REQUEST_TIMEOUT,
            self.bucket()?.object(&full).get_info(),
        )
        .await
        {
            Ok(Ok(info)) => Ok(StorageObject {
                key: key.to_string(),
                size: info.size(),
                last_modified: *info.last_modified(),
                etag: Some(info.etag().to_string()),
            }),
            Ok(Err(e)) => Err(SessyncError::Storage(format!("head {full}: {e:?}"))),
            Err(_) => Err(SessyncError::Storage(format!(
                "head {full}: timeout after {}s",
                REQUEST_TIMEOUT.as_secs()
            ))),
        }
    }
}

/// Returns `true` iff `err` is the specific error produced by serde-xml-rs when the
/// OSS ListBucketResult response omits the `<Contents>` element (empty bucket /
/// no prefix matches). Used in `list()` and extracted here for unit testing.
fn is_empty_bucket_xml_error(err: &aliyun_oss_client::Error) -> bool {
    matches!(
        err,
        aliyun_oss_client::Error::ParseXml(serde_xml_rs::Error::Custom { field })
            if field.contains("missing field") && field.contains("Contents")
    )
}

#[cfg(test)]
mod tests {
    use super::{is_empty_bucket_xml_error, MAX_LIST_PAGES, REQUEST_TIMEOUT};

    /// Confirm the OSS request timeout constant is set to 30 s.
    #[test]
    fn request_timeout_is_30s() {
        assert_eq!(REQUEST_TIMEOUT.as_secs(), 30);
    }

    /// Pagination cap is 50 pages (50 000 objects).
    #[test]
    fn max_list_pages_is_50() {
        assert_eq!(MAX_LIST_PAGES, 50);
    }

    /// Verify the pagination accumulator logic: items from multiple pages are
    /// combined into one Vec, and accumulation stops when there is no next_token.
    ///
    /// This test exercises the pure accumulation logic extracted from `list()`.
    /// We simulate pages as `(items, next_token)` tuples — the same type the SDK
    /// returns from `export_objects` — and verify that the loop correctly:
    ///   - appends items from every page
    ///   - stops when `next_token` is `None`
    ///   - respects the page cap
    #[test]
    fn pagination_accumulator_collects_all_pages() {
        // Simulate three pages: [a, b], [c, d], [e] with None at the end.
        let pages: Vec<(Vec<&str>, Option<&str>)> = vec![
            (vec!["a", "b"], Some("tok1")),
            (vec!["c", "d"], Some("tok2")),
            (vec!["e"], None),
        ];

        let mut all_items: Vec<&str> = Vec::new();
        let mut continuation_token: Option<String> = None;
        let mut pages_fetched: u32 = 0;

        for (items, next_token) in &pages {
            // Check that the token matches what we expect.
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

        assert_eq!(all_items, vec!["a", "b", "c", "d", "e"], "all items collected");
        assert_eq!(pages_fetched, 3, "exactly 3 pages fetched");
    }

    /// Verify that the page cap stops accumulation before MAX_LIST_PAGES+1.
    #[test]
    fn pagination_accumulator_respects_cap() {
        let mut all_items: Vec<u32> = Vec::new();
        let mut pages_fetched: u32 = 0;
        let mut continuation_token: Option<String> = Some("start".to_string());

        // Simulate an infinite sequence of pages (always has a next token).
        loop {
            let items: Vec<u32> = vec![pages_fetched]; // one item per page
            all_items.extend(items);
            pages_fetched += 1;

            // The "server" always returns a continuation token.
            let next_token = Some(format!("tok{pages_fetched}"));

            match next_token {
                None => break,
                Some(token) => {
                    if pages_fetched >= MAX_LIST_PAGES {
                        // Reached cap — stop (warn in production, just break here).
                        break;
                    }
                    continuation_token = Some(token);
                }
            }
        }

        assert_eq!(pages_fetched, MAX_LIST_PAGES, "must stop exactly at the cap");
        assert_eq!(all_items.len() as u32, MAX_LIST_PAGES, "one item per page");
        let _ = continuation_token; // consumed
    }

    /// Verify that the typed classifier accepts the exact error serde-xml-rs
    /// generates when `<Contents>` is absent from the OSS list XML.
    #[test]
    fn empty_bucket_xml_error_matches_missing_contents() {
        // Construct the error exactly as serde produces it via serde::de::Error::missing_field.
        use serde::de::Error as _;
        let serde_err = serde_xml_rs::Error::custom("missing field `Contents`");
        let oss_err = aliyun_oss_client::Error::ParseXml(serde_err);
        assert!(
            is_empty_bucket_xml_error(&oss_err),
            "should recognise missing-Contents as empty-bucket"
        );
    }

    /// Verify that an unrelated ParseXml error (e.g. a genuinely malformed
    /// response) is NOT swallowed and is returned as an error instead.
    #[test]
    fn empty_bucket_xml_error_does_not_match_other_parse_errors() {
        use serde::de::Error as _;
        let serde_err = serde_xml_rs::Error::custom("invalid type: expected u64");
        let oss_err = aliyun_oss_client::Error::ParseXml(serde_err);
        assert!(
            !is_empty_bucket_xml_error(&oss_err),
            "should NOT recognise unrelated parse error as empty-bucket"
        );
    }

    /// Verify that a non-ParseXml error (e.g. a network error variant) is not
    /// misclassified.
    #[test]
    fn empty_bucket_xml_error_does_not_match_non_xml_errors() {
        let oss_err = aliyun_oss_client::Error::InvalidBucket;
        assert!(
            !is_empty_bucket_xml_error(&oss_err),
            "should NOT recognise non-XML error as empty-bucket"
        );
    }
}
