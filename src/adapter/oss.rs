use super::storage::{StorageAdapter, StorageObject};
use crate::config::OssConfig;
use crate::error::{Result, SessyncError};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::Deserialize;

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
        self.bucket()?
            .object(&full)
            .upload(bytes)
            .await
            .map_err(|e| SessyncError::Storage(format!("put {full}: {e:?}")))?;
        Ok(())
    }

    /// Download the object at `key` (prefixed) and return its bytes.
    ///
    /// Normalizes OSS's `NoSuchKey` error to a "not found:" prefix that matches
    /// `LocalFsStorage` and `InMemoryStorage` semantics. Callers (e.g. the B1
    /// shared-salt logic in `init`) string-match on "not found" to decide
    /// "create on first use" vs "fail hard".
    async fn get(&self, key: &str) -> Result<Vec<u8>> {
        let full = self.full_key(key);
        match self.bucket()?.object(&full).download_to_bytes().await {
            Ok(buf) => Ok(buf),
            Err(e) => {
                let dbg = format!("{e:?}");
                if dbg.contains("NoSuchKey") {
                    Err(SessyncError::Storage(format!("not found: {key}")))
                } else {
                    Err(SessyncError::Storage(format!("get {full}: {dbg}")))
                }
            }
        }
    }

    /// List objects whose OSS key starts with `<configured_prefix><prefix>`.
    /// Returns keys with the configured prefix stripped (callers see bare keys).
    ///
    /// Note: v1 fetches only the first page (up to 1 000 objects by default).
    /// For the expected session count this is sufficient; pagination can be
    /// added later via the `next_token` returned by `get_objects`.
    async fn list(&self, prefix: &str) -> Result<Vec<StorageObject>> {
        let full_prefix = self.full_key(prefix);

        // Use export_objects with a custom serde type to capture Key, Size,
        // and LastModified from the XML <Contents> elements in a single call.
        #[derive(Debug, Deserialize)]
        struct OssItem {
            #[serde(rename = "Key")]
            key: String,
            #[serde(rename = "Size")]
            size: u64,
            #[serde(rename = "LastModified")]
            last_modified: DateTime<Utc>,
        }

        let result = self
            .bucket()?
            .prefix(&full_prefix)
            .export_objects::<OssItem>()
            .await;

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
                    "OSS list returned no <Contents> — treating as empty",
                );
                (vec![], None)
            }
            Err(e) => {
                return Err(SessyncError::Storage(format!("list {full_prefix}: {e:?}")));
            }
        };

        if next_token.is_some() {
            tracing::warn!(
                prefix = %full_prefix,
                returned = items.len(),
                "OSS list returned a continuation token; v1 only reads the first page — older sessions may be invisible to `resume`",
            );
        }

        let strip = &self.prefix;
        let out = items
            .into_iter()
            .map(|o| StorageObject {
                key: o
                    .key
                    .strip_prefix(strip.as_str())
                    .unwrap_or(&o.key)
                    .to_string(),
                size: o.size,
                last_modified: o.last_modified,
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
        self.bucket()?
            .object(&full)
            .delete()
            .await
            .map_err(|e| SessyncError::Storage(format!("delete {full}: {e:?}")))?;
        Ok(())
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
    use super::is_empty_bucket_xml_error;

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
