//! S3-compatible blob backend for the run store.
//!
//! `CHIDORI_RUN_STORE=s3://bucket[/prefix]` mirrors every run's journal and
//! artifacts to any S3-compatible object store — AWS S3, Cloudflare R2, GCS
//! (interop mode), Backblaze B2, MinIO, LocalStack — with no server-side code
//! to deploy (contrast the Durable Object relay, which needs a Worker but
//! gives serialized writers and platform-side leases; see
//! `docs/durable-storage.md` for when to pick which).
//!
//! Configuration:
//!   * `CHIDORI_RUN_STORE_ENDPOINT` — e.g. `https://<acct>.r2.cloudflarestorage.com`
//!     or `http://localhost:9000`; defaults to `https://s3.<region>.amazonaws.com`.
//!     Requests use path-style addressing (`endpoint/bucket/key`), which every
//!     S3-compatible store accepts.
//!   * `CHIDORI_RUN_STORE_REGION` (fallback `AWS_REGION`, default `us-east-1`).
//!   * `CHIDORI_RUN_STORE_ACCESS_KEY_ID` / `CHIDORI_RUN_STORE_SECRET_ACCESS_KEY`
//!     (fallback the standard `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY`;
//!     `AWS_SESSION_TOKEN` is honored when present).
//!
//! Object layout under the configured prefix:
//!   * `runs/<run_id>/records/<seq>.json` — the append-only journal tail: an
//!     append is ONE object PUT (object stores have no append primitive, so
//!     each record is its own object).
//!   * `runs/<run_id>/checkpoint.json` — the full-log artifact written at
//!     safepoints; writing it deletes the superseded tail objects
//!     (compaction, mirroring the filesystem layout's `records.jsonl` rules).
//!   * `runs/<run_id>/blobs/<key>` — auxiliary artifacts by their run-dir
//!     relative key.
//!   * `agents/<name>.json` — the detached-agent registry.
//!
//! Requests are signed with AWS Signature V4 (implemented here over the
//! `hmac`/`sha2` crates already in the tree — no AWS SDK dependency) and run
//! on the same dedicated relay thread as the HTTP backend.
//!
//! Caveat: object stores are last-writer-wins — there is no compare-and-swap
//! here, so run **leases** mirrored through this backend are advisory (the
//! same caveat as the plain filesystem backend). Deployments that need
//! platform-enforced single writers should use the Durable Object relay.

use std::sync::Arc;

use anyhow::{Context, Result};
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

use crate::runtime::call_log::CallRecord;
use crate::runtime::store::{HttpRelay, RunStore, CHECKPOINT_FILE};

type HmacSha256 = Hmac<Sha256>;

const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// Connection + signing configuration for one bucket, shared by every per-run
/// handle (they ride the same relay thread).
pub struct S3BlobStore {
    endpoint: String,
    host: String,
    bucket: String,
    /// Key prefix inside the bucket; empty or `…/`-terminated.
    prefix: String,
    region: String,
    access_key: String,
    secret_key: String,
    session_token: Option<String>,
    relay: Arc<HttpRelay>,
}

impl std::fmt::Debug for S3BlobStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("S3BlobStore")
            .field("endpoint", &self.endpoint)
            .field("bucket", &self.bucket)
            .field("prefix", &self.prefix)
            .field("region", &self.region)
            .finish_non_exhaustive()
    }
}

impl S3BlobStore {
    /// Build from a `s3://bucket[/prefix]` value plus the environment
    /// (endpoint, region, credentials — see the module docs).
    pub fn from_env(value: &str) -> Result<Arc<Self>> {
        let rest = value
            .strip_prefix("s3://")
            .ok_or_else(|| anyhow::anyhow!("expected s3://bucket[/prefix], got `{value}`"))?;
        let (bucket, prefix) = match rest.split_once('/') {
            Some((bucket, prefix)) => (bucket.to_string(), prefix.trim_matches('/').to_string()),
            None => (rest.to_string(), String::new()),
        };
        if bucket.is_empty() {
            anyhow::bail!("CHIDORI_RUN_STORE=s3://… is missing a bucket name");
        }
        let prefix = if prefix.is_empty() {
            String::new()
        } else {
            format!("{prefix}/")
        };
        let region = std::env::var("CHIDORI_RUN_STORE_REGION")
            .or_else(|_| std::env::var("AWS_REGION"))
            .unwrap_or_else(|_| "us-east-1".to_string());
        let endpoint = std::env::var("CHIDORI_RUN_STORE_ENDPOINT")
            .unwrap_or_else(|_| format!("https://s3.{region}.amazonaws.com"));
        let endpoint = endpoint.trim_end_matches('/').to_string();
        let host = url::Url::parse(&endpoint)
            .ok()
            .and_then(|url| {
                url.host_str().map(|h| match url.port() {
                    Some(port) => format!("{h}:{port}"),
                    None => h.to_string(),
                })
            })
            .ok_or_else(|| anyhow::anyhow!("invalid CHIDORI_RUN_STORE_ENDPOINT `{endpoint}`"))?;
        let access_key = std::env::var("CHIDORI_RUN_STORE_ACCESS_KEY_ID")
            .or_else(|_| std::env::var("AWS_ACCESS_KEY_ID"))
            .context(
                "s3 run store requires AWS_ACCESS_KEY_ID (or CHIDORI_RUN_STORE_ACCESS_KEY_ID)",
            )?;
        let secret_key = std::env::var("CHIDORI_RUN_STORE_SECRET_ACCESS_KEY")
            .or_else(|_| std::env::var("AWS_SECRET_ACCESS_KEY"))
            .context(
                "s3 run store requires AWS_SECRET_ACCESS_KEY (or CHIDORI_RUN_STORE_SECRET_ACCESS_KEY)",
            )?;
        Ok(Arc::new(Self {
            endpoint,
            host,
            bucket,
            prefix,
            region,
            access_key,
            secret_key,
            session_token: std::env::var("AWS_SESSION_TOKEN").ok(),
            relay: HttpRelay::new_headless(),
        }))
    }

    /// Test constructor with explicit connection parameters.
    #[cfg(test)]
    pub fn for_tests(endpoint: &str, bucket: &str, prefix: &str) -> Arc<Self> {
        let host = url::Url::parse(endpoint)
            .ok()
            .and_then(|url| {
                url.host_str().map(|h| match url.port() {
                    Some(port) => format!("{h}:{port}"),
                    None => h.to_string(),
                })
            })
            .unwrap();
        Arc::new(Self {
            endpoint: endpoint.trim_end_matches('/').to_string(),
            host,
            bucket: bucket.to_string(),
            prefix: if prefix.is_empty() {
                String::new()
            } else {
                format!("{}/", prefix.trim_matches('/'))
            },
            region: "us-east-1".to_string(),
            access_key: "test-access-key".to_string(),
            secret_key: "test-secret-key".to_string(),
            session_token: None,
            relay: HttpRelay::new_headless(),
        })
    }

    // --- Object operations --------------------------------------------------

    pub fn put_object(&self, key: &str, bytes: &[u8]) -> Result<()> {
        let (status, body) = self.signed("PUT", key, &[], Some(bytes))?;
        anyhow::ensure!(
            (200..300).contains(&status),
            "s3 PUT {key} failed: HTTP {status} {}",
            String::from_utf8_lossy(&body)
        );
        Ok(())
    }

    pub fn get_object(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let (status, body) = self.signed("GET", key, &[], None)?;
        match status {
            404 => Ok(None),
            s if (200..300).contains(&s) => Ok(Some(body)),
            s => anyhow::bail!(
                "s3 GET {key} failed: HTTP {s} {}",
                String::from_utf8_lossy(&body)
            ),
        }
    }

    pub fn delete_object(&self, key: &str) -> Result<()> {
        let (status, body) = self.signed("DELETE", key, &[], None)?;
        // 404 tolerated: deleting an absent object is Ok, matching RunStore.
        anyhow::ensure!(
            status == 404 || (200..300).contains(&status),
            "s3 DELETE {key} failed: HTTP {status} {}",
            String::from_utf8_lossy(&body)
        );
        Ok(())
    }

    /// ListObjectsV2 under `prefix`. With a delimiter, returns
    /// `(keys, common_prefixes)`; follows continuation tokens.
    pub fn list(
        &self,
        prefix: &str,
        delimiter: Option<&str>,
    ) -> Result<(Vec<String>, Vec<String>)> {
        let mut keys = Vec::new();
        let mut common = Vec::new();
        let mut continuation: Option<String> = None;
        loop {
            let mut query: Vec<(String, String)> = vec![
                ("list-type".to_string(), "2".to_string()),
                ("prefix".to_string(), prefix.to_string()),
            ];
            if let Some(delimiter) = delimiter {
                query.push(("delimiter".to_string(), delimiter.to_string()));
            }
            if let Some(ref token) = continuation {
                query.push(("continuation-token".to_string(), token.clone()));
            }
            let (status, body) = self.signed("GET", "", &query, None)?;
            anyhow::ensure!(
                (200..300).contains(&status),
                "s3 LIST {prefix} failed: HTTP {status} {}",
                String::from_utf8_lossy(&body)
            );
            let xml = String::from_utf8_lossy(&body);
            keys.extend(extract_xml_values(&xml, "Key"));
            for block in extract_xml_values(&xml, "CommonPrefixes") {
                common.extend(extract_xml_values(&block, "Prefix"));
            }
            let truncated = extract_xml_values(&xml, "IsTruncated")
                .first()
                .is_some_and(|v| v == "true");
            if !truncated {
                break;
            }
            continuation = extract_xml_values(&xml, "NextContinuationToken")
                .into_iter()
                .next();
            if continuation.is_none() {
                break;
            }
        }
        Ok((keys, common))
    }

    /// The configured key prefix (empty or `…/`-terminated), so the factory
    /// can address `runs/…` / `agents/…` keyspaces.
    #[allow(dead_code)] // Not yet wired into a call path; staged API.
    pub fn key_prefix(&self) -> &str {
        &self.prefix
    }

    // --- SigV4 --------------------------------------------------------------

    /// Send one signed request. `key` is bucket-relative ("" for a bucket-level
    /// operation like LIST); `query` must be the exact query the request uses.
    /// Build the SigV4-signed request parts (url, headers, content type)
    /// without sending — shared by the sync and pipelined paths.
    fn build_signed(
        &self,
        method: &'static str,
        key: &str,
        query: &[(String, String)],
        body: Option<&[u8]>,
    ) -> Result<(String, Vec<(String, String)>, &'static str)> {
        let now = chrono::Utc::now();
        let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();
        let date = now.format("%Y%m%d").to_string();
        let payload_hash = match body {
            Some(bytes) => hex_sha256(bytes),
            None => EMPTY_SHA256.to_string(),
        };

        // Canonical URI: /bucket/key with each segment AWS-uri-encoded.
        let mut canonical_uri = format!("/{}", uri_encode(&self.bucket, false));
        if !key.is_empty() {
            canonical_uri.push('/');
            canonical_uri.push_str(&uri_encode(key, false));
        }

        let mut sorted_query: Vec<(String, String)> = query.to_vec();
        sorted_query.sort();
        let canonical_query = sorted_query
            .iter()
            .map(|(k, v)| format!("{}={}", uri_encode(k, true), uri_encode(v, true)))
            .collect::<Vec<_>>()
            .join("&");

        // Signed headers, sorted: content-type only rides with a body (the
        // relay attaches it then), and the session token when present.
        let content_type = "application/octet-stream";
        let mut header_pairs: Vec<(String, String)> = vec![
            ("host".to_string(), self.host.clone()),
            ("x-amz-content-sha256".to_string(), payload_hash.clone()),
            ("x-amz-date".to_string(), amz_date.clone()),
        ];
        if body.is_some() {
            header_pairs.push(("content-type".to_string(), content_type.to_string()));
        }
        if let Some(ref token) = self.session_token {
            header_pairs.push(("x-amz-security-token".to_string(), token.clone()));
        }
        header_pairs.sort();
        let canonical_headers = header_pairs
            .iter()
            .map(|(k, v)| format!("{k}:{}\n", v.trim()))
            .collect::<String>();
        let signed_headers = header_pairs
            .iter()
            .map(|(k, _)| k.as_str())
            .collect::<Vec<_>>()
            .join(";");

        let canonical_request = format!(
            "{method}\n{canonical_uri}\n{canonical_query}\n{canonical_headers}\n{signed_headers}\n{payload_hash}"
        );
        let scope = format!("{date}/{}/s3/aws4_request", self.region);
        let string_to_sign = format!(
            "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
            hex_sha256(canonical_request.as_bytes())
        );
        let mut signing_key = hmac_sha256(
            format!("AWS4{}", self.secret_key).as_bytes(),
            date.as_bytes(),
        );
        for part in [self.region.as_bytes(), b"s3", b"aws4_request"] {
            signing_key = hmac_sha256(&signing_key, part);
        }
        let signature = hex::encode(hmac_sha256(&signing_key, string_to_sign.as_bytes()));
        let authorization = format!(
            "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
            self.access_key
        );

        // Request headers: everything signed except host/content-type (the
        // HTTP client and the relay's body path supply those), plus the
        // Authorization line.
        let mut headers: Vec<(String, String)> = header_pairs
            .into_iter()
            .filter(|(k, _)| k != "host" && k != "content-type")
            .collect();
        headers.push(("authorization".to_string(), authorization));

        let url = if canonical_query.is_empty() {
            format!("{}{canonical_uri}", self.endpoint)
        } else {
            format!("{}{canonical_uri}?{canonical_query}", self.endpoint)
        };
        Ok((url, headers, content_type))
    }

    fn signed(
        &self,
        method: &'static str,
        key: &str,
        query: &[(String, String)],
        body: Option<&[u8]>,
    ) -> Result<(u16, Vec<u8>)> {
        let (url, headers, content_type) = self.build_signed(method, key, query, body)?;
        self.relay
            .request_full(method, url, body.map(<[u8]>::to_vec), content_type, headers)
    }

    /// Pipelined PUT: signed like [`Self::signed`] but enqueued without
    /// waiting for the round-trip; outcomes surface at the relay's barrier
    /// (the `RunStore::flush` durability gate).
    fn put_object_async(&self, key: &str, bytes: &[u8]) -> Result<()> {
        let (url, headers, content_type) = self.build_signed("PUT", key, &[], Some(bytes))?;
        self.relay.request_async(
            "PUT",
            url,
            Some(bytes.to_vec()),
            content_type,
            headers,
            false,
        )
    }

    /// Pipelined DELETE; a 404 counts as success, matching
    /// [`Self::delete_object`].
    fn delete_object_async(&self, key: &str) -> Result<()> {
        let (url, headers, content_type) = self.build_signed("DELETE", key, &[], None)?;
        self.relay
            .request_async("DELETE", url, None, content_type, headers, true)
    }
}

/// AWS SigV4 URI encoding: unreserved characters pass through; `/` passes
/// through in paths but is encoded in query values.
fn uri_encode(value: &str, encode_slash: bool) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            b'/' if !encode_slash => out.push('/'),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn hex_sha256(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("hmac accepts any key length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

/// Extract the inner text of every `<tag>…</tag>` occurrence (non-nested,
/// which is all ListObjectsV2 responses use), XML-unescaping the basics.
/// For `CommonPrefixes` the "inner text" is itself XML and is re-parsed for
/// `Prefix` by the caller.
fn extract_xml_values(xml: &str, tag: &str) -> Vec<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find(&open) {
        let after = &rest[start + open.len()..];
        let Some(end) = after.find(&close) else { break };
        out.push(xml_unescape(&after[..end]));
        rest = &after[end + close.len()..];
    }
    out
}

fn xml_unescape(value: &str) -> String {
    value
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

// ---------------------------------------------------------------------------
// RunStore over the blob layout
// ---------------------------------------------------------------------------

/// One run's view of the bucket (`runs/<run_id>/…` under the configured
/// prefix).
#[derive(Debug)]
pub struct BlobRunStore {
    store: Arc<S3BlobStore>,
    run_id: String,
    /// Pipeline record appends through the relay instead of blocking one
    /// network round-trip per record. Off under `CHIDORI_DURABILITY=strict`;
    /// in besteffort mode the `flush()` barrier at pause/settle is the
    /// durability gate (`RunStore::flush` contract).
    pipelined: bool,
}

impl BlobRunStore {
    pub fn new(store: Arc<S3BlobStore>, run_id: impl Into<String>) -> Self {
        Self {
            store,
            run_id: run_id.into(),
            pipelined: !crate::runtime::store::strict_durability(),
        }
    }

    fn run_key(&self, rest: &str) -> String {
        format!("{}runs/{}/{rest}", self.store.prefix, self.run_id)
    }

    fn record_key(&self, seq: u64) -> String {
        self.run_key(&format!("records/{seq:020}.json"))
    }
}

impl RunStore for BlobRunStore {
    fn append_record(&self, record: &CallRecord) -> Result<()> {
        // One object per record: object stores have no append primitive, so
        // the journal tail is a keyspace and an append is a single PUT.
        // Re-appending a seq overwrites its object, matching the contract.
        let key = self.record_key(record.seq);
        let bytes = serde_json::to_vec(record)?;
        if self.pipelined {
            return self.store.put_object_async(&key, &bytes);
        }
        self.store.put_object(&key, &bytes)
    }

    fn write_call_log(&self, records: &[CallRecord]) -> Result<()> {
        self.store.put_object(
            &self.run_key(CHECKPOINT_FILE),
            &serde_json::to_vec_pretty(records)?,
        )?;
        // Compaction: the checkpoint supersedes the tail objects.
        let (tail, _) = self.store.list(&self.run_key("records/"), None)?;
        for key in tail {
            self.store.delete_object(&key)?;
        }
        Ok(())
    }

    fn load_call_log(&self) -> Result<Option<Vec<CallRecord>>> {
        let checkpoint: Option<Vec<CallRecord>> =
            match self.store.get_object(&self.run_key(CHECKPOINT_FILE))? {
                Some(bytes) => Some(serde_json::from_slice(&bytes)?),
                None => None,
            };
        let (tail_keys, _) = self.store.list(&self.run_key("records/"), None)?;
        // Keys are zero-padded seqs, so lexicographic order is seq order.
        let mut tail_keys = tail_keys;
        tail_keys.sort();
        let mut tail = Vec::with_capacity(tail_keys.len());
        for key in tail_keys {
            if let Some(bytes) = self.store.get_object(&key)? {
                if let Ok(record) = serde_json::from_slice::<CallRecord>(&bytes) {
                    tail.push(record);
                }
            }
        }
        Ok(crate::runtime::store::union_checkpoint_and_tail(
            checkpoint, tail,
        ))
    }

    fn put_blob(&self, key: &str, bytes: &[u8]) -> Result<()> {
        let object_key = self.run_key(&format!("blobs/{key}"));
        if self.pipelined {
            return self.store.put_object_async(&object_key, bytes);
        }
        self.store.put_object(&object_key, bytes)
    }

    fn get_blob(&self, key: &str) -> Result<Option<Vec<u8>>> {
        self.store
            .get_object(&self.run_key(&format!("blobs/{key}")))
    }

    fn delete_blob(&self, key: &str) -> Result<()> {
        let object_key = self.run_key(&format!("blobs/{key}"));
        if self.pipelined {
            return self.store.delete_object_async(&object_key);
        }
        self.store.delete_object(&object_key)
    }

    fn list_blobs(&self) -> Result<Vec<String>> {
        let prefix = self.run_key("blobs/");
        let (keys, _) = self.store.list(&prefix, None)?;
        Ok(keys
            .into_iter()
            .filter_map(|key| key.strip_prefix(&prefix).map(str::to_string))
            .collect())
    }

    fn flush(&self) -> Result<()> {
        // Surface pipelined-append failures at the durability gate; only
        // besteffort mode pipelines, and its contract is log-and-continue —
        // matching the pre-pipelining per-append error handling.
        if let Err(err) = self.store.relay.barrier() {
            tracing::warn!(run_id = %self.run_id, error = %err, "s3 mirror pipelined writes failed");
        }
        Ok(())
    }
}

/// Run ids known to the bucket (`runs/<id>/…` common prefixes).
#[allow(dead_code)] // Not yet wired into a call path; staged API.
pub fn list_runs(store: &S3BlobStore) -> Result<Vec<String>> {
    let prefix = format!("{}runs/", store.prefix);
    let (_, common) = store.list(&prefix, Some("/"))?;
    Ok(common
        .into_iter()
        .filter_map(|p| {
            p.strip_prefix(&prefix)
                .map(|rest| rest.trim_end_matches('/').to_string())
        })
        .filter(|id| !id.is_empty())
        .collect())
}

pub fn registry_put(store: &S3BlobStore, name: &str, entry: &serde_json::Value) -> Result<()> {
    store.put_object(
        &format!("{}agents/{name}.json", store.prefix),
        &serde_json::to_vec_pretty(entry)?,
    )
}

pub fn registry_get(store: &S3BlobStore, name: &str) -> Result<Option<serde_json::Value>> {
    match store.get_object(&format!("{}agents/{name}.json", store.prefix))? {
        Some(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        None => Ok(None),
    }
}

pub fn registry_list(store: &S3BlobStore) -> Result<Vec<serde_json::Value>> {
    let prefix = format!("{}agents/", store.prefix);
    let (keys, _) = store.list(&prefix, None)?;
    let mut out = Vec::new();
    for key in keys {
        if let Some(bytes) = store.get_object(&key)? {
            if let Ok(value) = serde_json::from_slice(&bytes) {
                out.push(value);
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    use axum::response::IntoResponse as _;

    use super::*;

    /// A minimal in-process S3-compatible endpoint: path-style object
    /// PUT/GET/DELETE plus ListObjectsV2 (prefix, delimiter, XML response).
    /// Signature headers are accepted but not validated — the test asserts
    /// protocol compatibility, not IAM.
    fn spawn_mock_s3() -> String {
        use axum::extract::{Path as AxPath, Query, State};
        use axum::http::StatusCode;

        type Objects = std::sync::Arc<Mutex<BTreeMap<String, Vec<u8>>>>;
        let objects: Objects = Default::default();

        fn xml_escape(value: &str) -> String {
            value
                .replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;")
        }

        async fn list_bucket(
            State(objects): State<Objects>,
            AxPath(bucket): AxPath<String>,
            Query(params): Query<std::collections::HashMap<String, String>>,
        ) -> axum::response::Response {
            let prefix = params.get("prefix").cloned().unwrap_or_default();
            let delimiter = params.get("delimiter").cloned();
            let full_prefix = format!("{bucket}/{prefix}");
            let mut keys = Vec::new();
            let mut common = std::collections::BTreeSet::new();
            for key in objects.lock().unwrap().keys() {
                let Some(rest) = key.strip_prefix(&full_prefix) else {
                    continue;
                };
                match delimiter.as_deref() {
                    Some(d) if rest.contains(d) => {
                        let head = &rest[..rest.find(d).unwrap() + d.len()];
                        common.insert(format!("{prefix}{head}"));
                    }
                    _ => keys.push(format!("{prefix}{rest}")),
                }
            }
            let mut xml = String::from(
                "<?xml version=\"1.0\"?><ListBucketResult><IsTruncated>false</IsTruncated>",
            );
            for key in keys {
                xml.push_str(&format!(
                    "<Contents><Key>{}</Key></Contents>",
                    xml_escape(&key)
                ));
            }
            for p in common {
                xml.push_str(&format!(
                    "<CommonPrefixes><Prefix>{}</Prefix></CommonPrefixes>",
                    xml_escape(&p)
                ));
            }
            xml.push_str("</ListBucketResult>");
            (StatusCode::OK, xml).into_response()
        }

        async fn object(
            State(objects): State<Objects>,
            AxPath((bucket, key)): AxPath<(String, String)>,
            method: axum::http::Method,
            body: axum::body::Bytes,
        ) -> axum::response::Response {
            let full = format!("{bucket}/{key}");
            match method.as_str() {
                "PUT" => {
                    objects.lock().unwrap().insert(full, body.to_vec());
                    StatusCode::OK.into_response()
                }
                "GET" => match objects.lock().unwrap().get(&full) {
                    Some(bytes) => (StatusCode::OK, bytes.clone()).into_response(),
                    None => StatusCode::NOT_FOUND.into_response(),
                },
                "DELETE" => {
                    objects.lock().unwrap().remove(&full);
                    StatusCode::NO_CONTENT.into_response()
                }
                _ => StatusCode::METHOD_NOT_ALLOWED.into_response(),
            }
        }

        let app = axum::Router::new()
            .route("/{bucket}", axum::routing::get(list_bucket))
            .route("/{bucket}/{*key}", axum::routing::any(object))
            .with_state(objects);

        let (addr_tx, addr_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                addr_tx.send(listener.local_addr().unwrap()).unwrap();
                axum::serve(listener, app).await.unwrap();
            });
        });
        format!("http://{}", addr_rx.recv().unwrap())
    }

    fn record(seq: u64, function: &str) -> CallRecord {
        CallRecord {
            seq,
            parent_seq: None,
            function: function.to_string(),
            args: serde_json::json!({}),
            result: serde_json::json!({"ok": true}),
            duration_ms: 1,
            token_usage: None,
            timestamp: chrono::Utc::now(),
            error: None,
        }
    }

    #[test]
    fn blob_run_store_conformance_and_layout() {
        let endpoint = spawn_mock_s3();
        let s3 = S3BlobStore::for_tests(&endpoint, "chidori-runs", "team-a");
        let store = BlobRunStore::new(s3.clone(), "run-blob");

        // Journal: appends are per-record objects…
        assert!(store.load_call_log().unwrap().is_none());
        store.append_record(&record(1, "prompt")).unwrap();
        store.append_record(&record(2, "tool")).unwrap();
        let loaded = store.load_call_log().unwrap().unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[1].function, "tool");

        // …a checkpoint compacts them away…
        store
            .write_call_log(&[record(1, "prompt"), record(2, "tool"), record(3, "signal")])
            .unwrap();
        let (tail, _) = s3.list("team-a/runs/run-blob/records/", None).unwrap();
        assert!(tail.is_empty(), "checkpoint should compact tail objects");

        // …and a stranded post-checkpoint append is recovered on load.
        store.append_record(&record(4, "http")).unwrap();
        let loaded = store.load_call_log().unwrap().unwrap();
        assert_eq!(
            loaded.iter().map(|r| r.seq).collect::<Vec<_>>(),
            vec![1, 2, 3, 4]
        );

        // Re-appending a seq replaces its object.
        store.append_record(&record(4, "http_retry")).unwrap();
        let loaded = store.load_call_log().unwrap().unwrap();
        assert_eq!(loaded.len(), 4);
        assert_eq!(loaded[3].function, "http_retry");

        // Blobs round-trip under blobs/, including nested keys.
        store.put_blob("manifest.json", b"{}").unwrap();
        store.put_blob("signals/inbox.json", b"[]").unwrap();
        assert_eq!(store.get_blob("manifest.json").unwrap().unwrap(), b"{}");
        let keys = store.list_blobs().unwrap();
        assert!(keys.contains(&"manifest.json".to_string()));
        assert!(keys.contains(&"signals/inbox.json".to_string()));
        store.delete_blob("manifest.json").unwrap();
        assert!(store.get_blob("manifest.json").unwrap().is_none());
        store.delete_blob("manifest.json").unwrap(); // absent delete is Ok

        // Run listing via common prefixes; registry round-trips.
        let other = BlobRunStore::new(s3.clone(), "run-other");
        other.append_record(&record(1, "log")).unwrap();
        let mut runs = list_runs(&s3).unwrap();
        runs.sort();
        assert_eq!(runs, vec!["run-blob".to_string(), "run-other".to_string()]);
        registry_put(&s3, "triager", &serde_json::json!({"run_id": "run-blob"})).unwrap();
        assert_eq!(
            registry_get(&s3, "triager").unwrap().unwrap()["run_id"],
            "run-blob"
        );
        assert_eq!(registry_list(&s3).unwrap().len(), 1);
    }

    #[test]
    fn sigv4_matches_known_vector() {
        // AWS's documented GET example ("Example: GET Object" from the SigV4
        // test suite): verify the derived signing key path produces the
        // published signature for the canonical string-to-sign inputs.
        let secret = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
        let date = "20130524";
        let region = "us-east-1";
        let string_to_sign = "AWS4-HMAC-SHA256\n20130524T000000Z\n20130524/us-east-1/s3/aws4_request\n7344ae5b7ee6c3e7e6b0fe0640412a37625d1fbfff95c48bbb2dc43964946972";
        let mut key = hmac_sha256(format!("AWS4{secret}").as_bytes(), date.as_bytes());
        for part in [
            region.as_bytes(),
            b"s3".as_slice(),
            b"aws4_request".as_slice(),
        ] {
            key = hmac_sha256(&key, part);
        }
        let signature = hex::encode(hmac_sha256(&key, string_to_sign.as_bytes()));
        assert_eq!(
            signature,
            "f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
        );
    }
}
