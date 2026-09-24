//! Firestore (native mode) through its REST API, and Cloud Storage for
//! backup blobs. Each document holds its JSON in one string field, so the
//! service never depends on Firestore's type mapping.
//!
//! On Cloud Run, access tokens come from the metadata server. With
//! `FIRESTORE_EMULATOR_HOST` set, the emulator is used without a token.

use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::Mutex;

use super::{check_id, Blobs, Store, StoreError, StoreResult};

const METADATA_TOKEN: &str =
    "http://metadata.google.internal/computeMetadata/v1/instance/service-accounts/default/token";

/// Access tokens from the metadata server, cached until shortly before expiry.
pub struct Tokens {
    http: reqwest::Client,
    cached: Mutex<Option<(String, Instant)>>,
    disabled: bool,
}

impl Tokens {
    pub fn metadata_server(http: reqwest::Client) -> Self {
        Self {
            http,
            cached: Mutex::new(None),
            disabled: false,
        }
    }

    /// For the Firestore emulator: its `owner` token acts like the service
    /// account, which security rules do not restrict.
    pub fn none(http: reqwest::Client) -> Self {
        Self {
            http,
            cached: Mutex::new(None),
            disabled: true,
        }
    }

    async fn bearer(&self) -> StoreResult<Option<String>> {
        if self.disabled {
            return Ok(Some("owner".into()));
        }
        let mut cached = self.cached.lock().await;
        if let Some((token, until)) = cached.as_ref() {
            if Instant::now() < *until {
                return Ok(Some(token.clone()));
            }
        }
        let v: Value = self
            .http
            .get(METADATA_TOKEN)
            .header("Metadata-Flavor", "Google")
            .send()
            .await
            .map_err(|e| StoreError::Unavailable(format!("metadata server: {e}")))?
            .json()
            .await
            .map_err(|e| StoreError::Unavailable(format!("metadata server: {e}")))?;
        let token = v["access_token"]
            .as_str()
            .ok_or_else(|| StoreError::Unavailable("no access token".into()))?
            .to_string();
        let ttl = v["expires_in"].as_u64().unwrap_or(300).saturating_sub(60);
        *cached = Some((token.clone(), Instant::now() + Duration::from_secs(ttl)));
        Ok(Some(token))
    }

    async fn request(&self, rb: reqwest::RequestBuilder) -> StoreResult<reqwest::Response> {
        let rb = match self.bearer().await? {
            Some(t) => rb.bearer_auth(t),
            None => rb,
        };
        rb.send()
            .await
            .map_err(|e| StoreError::Unavailable(e.to_string()))
    }
}

pub struct Firestore {
    http: reqwest::Client,
    tokens: Tokens,
    /// `.../v1/projects/{p}/databases/(default)/documents`
    base: String,
    /// `projects/{p}/databases/(default)/documents`
    root: String,
}

fn unavailable(what: &str, status: reqwest::StatusCode, body: &str) -> StoreError {
    StoreError::Unavailable(format!(
        "{what}: {status} {}",
        body.chars().take(200).collect::<String>()
    ))
}

impl Firestore {
    pub fn new(http: reqwest::Client, project: &str, tokens: Tokens) -> Self {
        let host = std::env::var("FIRESTORE_EMULATOR_HOST")
            .map(|h| format!("http://{h}"))
            .unwrap_or_else(|_| "https://firestore.googleapis.com".into());
        let root = format!("projects/{project}/databases/(default)/documents");
        Self {
            http,
            tokens,
            base: format!("{host}/v1/{root}"),
            root,
        }
    }

    fn doc(value: &Value) -> Value {
        json!({ "fields": { "json": { "stringValue": value.to_string() } } })
    }

    fn parse(doc: &Value) -> StoreResult<(String, Value)> {
        let name = doc["name"].as_str().unwrap_or_default();
        let id = name.rsplit('/').next().unwrap_or_default().to_string();
        let text = doc["fields"]["json"]["stringValue"]
            .as_str()
            .unwrap_or("null");
        let value = serde_json::from_str(text).map_err(|e| StoreError::Invalid(e.to_string()))?;
        Ok((id, value))
    }

    async fn query(
        &self,
        collection: &str,
        filters: Vec<Value>,
        limit: Option<usize>,
    ) -> StoreResult<Vec<(String, Value)>> {
        let mut q = json!({
            "from": [{ "collectionId": collection }],
            "orderBy": [{ "field": { "fieldPath": "__name__" }, "direction": "ASCENDING" }],
        });
        if !filters.is_empty() {
            q["where"] = json!({ "compositeFilter": { "op": "AND", "filters": filters } });
        }
        if let Some(l) = limit {
            q["limit"] = json!(l);
        }
        let res = self
            .tokens
            .request(
                self.http
                    .post(format!("{}:runQuery", self.base))
                    .json(&json!({ "structuredQuery": q })),
            )
            .await?;
        let status = res.status();
        let body = res.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(unavailable("query", status, &body));
        }
        let rows: Vec<Value> =
            serde_json::from_str(&body).map_err(|e| StoreError::Invalid(e.to_string()))?;
        rows.iter()
            .filter(|r| r.get("document").is_some())
            .map(|r| Self::parse(&r["document"]))
            .collect()
    }

    fn name_filter(&self, collection: &str, op: &str, id: &str) -> Value {
        json!({ "fieldFilter": {
            "field": { "fieldPath": "__name__" },
            "op": op,
            "value": { "referenceValue": format!("{}/{collection}/{id}", self.root) },
        }})
    }
}

#[async_trait]
impl Store for Firestore {
    async fn create(&self, collection: &str, id: &str, value: &Value) -> StoreResult<bool> {
        check_id(id)?;
        let mut url = url::Url::parse(&format!("{}/{collection}", self.base))
            .map_err(|e| StoreError::Invalid(e.to_string()))?;
        url.query_pairs_mut().append_pair("documentId", id);
        let res = self
            .tokens
            .request(self.http.post(url).json(&Self::doc(value)))
            .await?;
        match res.status().as_u16() {
            200 => Ok(true),
            409 => Ok(false),
            _ => {
                let s = res.status();
                Err(unavailable(
                    "create",
                    s,
                    &res.text().await.unwrap_or_default(),
                ))
            }
        }
    }

    async fn get(&self, collection: &str, id: &str) -> StoreResult<Option<Value>> {
        check_id(id)?;
        let res = self
            .tokens
            .request(
                self.http
                    .get(format!("{}/{collection}/{}", self.base, encode(id))),
            )
            .await?;
        match res.status().as_u16() {
            200 => {
                let v: Value = res
                    .json()
                    .await
                    .map_err(|e| StoreError::Invalid(e.to_string()))?;
                Ok(Some(Self::parse(&v)?.1))
            }
            404 => Ok(None),
            _ => {
                let s = res.status();
                Err(unavailable("get", s, &res.text().await.unwrap_or_default()))
            }
        }
    }

    async fn put(&self, collection: &str, id: &str, value: &Value) -> StoreResult<()> {
        check_id(id)?;
        let res = self
            .tokens
            .request(
                self.http
                    .patch(format!("{}/{collection}/{}", self.base, encode(id)))
                    .json(&Self::doc(value)),
            )
            .await?;
        if res.status().is_success() {
            Ok(())
        } else {
            let s = res.status();
            Err(unavailable("put", s, &res.text().await.unwrap_or_default()))
        }
    }

    async fn delete(&self, collection: &str, id: &str) -> StoreResult<bool> {
        let existed = self.get(collection, id).await?.is_some();
        if existed {
            let res = self
                .tokens
                .request(
                    self.http
                        .delete(format!("{}/{collection}/{}", self.base, encode(id))),
                )
                .await?;
            if !res.status().is_success() {
                let s = res.status();
                return Err(unavailable(
                    "delete",
                    s,
                    &res.text().await.unwrap_or_default(),
                ));
            }
        }
        Ok(existed)
    }

    async fn list_prefix(
        &self,
        collection: &str,
        prefix: &str,
    ) -> StoreResult<Vec<(String, Value)>> {
        // Ids from `prefix` up to `prefix` followed by the highest code point.
        let end = format!("{prefix}\u{10FFFF}");
        self.query(
            collection,
            vec![
                self.name_filter(collection, "GREATER_THAN_OR_EQUAL", prefix),
                self.name_filter(collection, "LESS_THAN", &end),
            ],
            None,
        )
        .await
    }

    async fn list_after(
        &self,
        collection: &str,
        after: &str,
        limit: usize,
    ) -> StoreResult<Vec<(String, Value)>> {
        let filters = if after.is_empty() {
            vec![]
        } else {
            vec![self.name_filter(collection, "GREATER_THAN", after)]
        };
        self.query(collection, filters, Some(limit)).await
    }

    async fn increment(&self, collection: &str, id: &str) -> StoreResult<i64> {
        check_id(id)?;
        let body = json!({ "writes": [{ "transform": {
            "document": format!("{}/{collection}/{id}", self.root),
            "fieldTransforms": [{ "fieldPath": "count", "increment": { "integerValue": "1" } }],
        }}]});
        let res = self
            .tokens
            .request(self.http.post(format!("{}:commit", self.base)).json(&body))
            .await?;
        let status = res.status();
        let text = res.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(unavailable("increment", status, &text));
        }
        let v: Value =
            serde_json::from_str(&text).map_err(|e| StoreError::Invalid(e.to_string()))?;
        v["writeResults"][0]["transformResults"][0]["integerValue"]
            .as_str()
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| StoreError::Invalid("no counter value".into()))
    }
}

fn encode(id: &str) -> String {
    url::form_urlencoded::byte_serialize(id.as_bytes())
        .collect::<String>()
        .replace('+', "%20")
}

/// Cloud Storage objects, one per backup snapshot.
pub struct Gcs {
    http: reqwest::Client,
    tokens: Tokens,
    bucket: String,
}

impl Gcs {
    pub fn new(http: reqwest::Client, bucket: &str, tokens: Tokens) -> Self {
        Self {
            http,
            tokens,
            bucket: bucket.into(),
        }
    }
}

#[async_trait]
impl Blobs for Gcs {
    async fn put(&self, key: &str, bytes: Vec<u8>) -> StoreResult<()> {
        let mut url = url::Url::parse(&format!(
            "https://storage.googleapis.com/upload/storage/v1/b/{}/o",
            self.bucket
        ))
        .map_err(|e| StoreError::Invalid(e.to_string()))?;
        url.query_pairs_mut()
            .append_pair("uploadType", "media")
            .append_pair("name", key);
        let res = self
            .tokens
            .request(
                self.http
                    .post(url)
                    .header("Content-Type", "application/json")
                    .body(bytes),
            )
            .await?;
        if res.status().is_success() {
            Ok(())
        } else {
            let s = res.status();
            Err(unavailable(
                "upload",
                s,
                &res.text().await.unwrap_or_default(),
            ))
        }
    }

    async fn get(&self, key: &str) -> StoreResult<Option<Vec<u8>>> {
        let url = format!(
            "https://storage.googleapis.com/storage/v1/b/{}/o/{}?alt=media",
            self.bucket,
            encode(key)
        );
        let res = self.tokens.request(self.http.get(url)).await?;
        match res.status().as_u16() {
            200 => Ok(Some(
                res.bytes()
                    .await
                    .map_err(|e| StoreError::Unavailable(e.to_string()))?
                    .to_vec(),
            )),
            404 => Ok(None),
            _ => {
                let s = res.status();
                Err(unavailable(
                    "download",
                    s,
                    &res.text().await.unwrap_or_default(),
                ))
            }
        }
    }

    async fn delete(&self, key: &str) -> StoreResult<()> {
        let url = format!(
            "https://storage.googleapis.com/storage/v1/b/{}/o/{}",
            self.bucket,
            encode(key)
        );
        let res = self.tokens.request(self.http.delete(url)).await?;
        match res.status().as_u16() {
            200 | 204 | 404 => Ok(()),
            _ => {
                let s = res.status();
                Err(unavailable(
                    "delete object",
                    s,
                    &res.text().await.unwrap_or_default(),
                ))
            }
        }
    }
}
