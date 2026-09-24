//! The shared catalogue and private backups, on top of the storage
//! primitives.
//!
//! Collections:
//! - `contributions/{account}~{key}`: one per accepted contribution, create-only,
//!   so a retried submission is a duplicate and nothing is overwritten.
//! - `recording_keys/{key}`: fingerprint hash or external id to recording.
//! - `recording_meta/{recording}~{account}`: each account's latest metadata.
//! - `recording_refs/{recording}~{account}`: each account's references.
//! - `features/{recording}~{version}~{account}`: each account's embedding.
//! - `changes/{sequence}`: recordings in the order they changed.
//! - `backups/{account}~{id}`: backup metadata; the snapshot is a blob.
//! - `quotas/{account}~{day}`: daily contribution counts.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use cd_protocol::backup::{BackupInfo, Snapshot, MAX_SNAPSHOT_BYTES};
use cd_protocol::*;
use serde_json::json;

use crate::store::{Blobs, Store, StoreError};

/// Contributions one account may make per day.
pub const DAILY_CONTRIBUTIONS: i64 = 20_000;
/// Embeddings this similar count as agreeing.
const AGREEMENT: f32 = 0.95;

#[derive(Debug)]
pub enum Error {
    Problem(u16, Problem),
    Store(StoreError),
}

impl From<StoreError> for Error {
    fn from(e: StoreError) -> Self {
        Error::Store(e)
    }
}

fn problem(status: u16, code: &str, message: &str) -> Error {
    Error::Problem(
        status,
        Problem {
            code: code.into(),
            message: message.into(),
        },
    )
}

pub type Result<T> = std::result::Result<T, Error>;

pub struct Catalogue {
    pub store: Arc<dyn Store>,
    pub blobs: Arc<dyn Blobs>,
    pub now_ms: Arc<dyn Fn() -> i64 + Send + Sync>,
    sequence: AtomicU64,
}

fn keys_of(r: &RecordingKey) -> Vec<String> {
    let mut keys: Vec<String> = r
        .fingerprint_hash
        .iter()
        .map(|h| format!("fp~{h}"))
        .collect();
    keys.extend(
        r.external_ids
            .iter()
            .map(|e| format!("{}~{}", e.source, e.id)),
    );
    keys.into_iter().map(|k| k.replace('/', "_")).collect()
}

/// A short, id-safe name for a feature version.
fn version_id(v: &FeatureVersion) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in format!(
        "{}|{}|{}",
        v.model_id, v.weights_checksum, v.preprocessing_version
    )
    .bytes()
    {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("v{h:016x}")
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let (mut d, mut na, mut nb) = (0f32, 0f32, 0f32);
    for (x, y) in a.iter().zip(b) {
        d += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        d / (na.sqrt() * nb.sqrt())
    }
}

impl Catalogue {
    pub fn new(
        store: Arc<dyn Store>,
        blobs: Arc<dyn Blobs>,
        now_ms: Arc<dyn Fn() -> i64 + Send + Sync>,
    ) -> Self {
        Self {
            store,
            blobs,
            now_ms,
            sequence: AtomicU64::new(0),
        }
    }

    fn now(&self) -> i64 {
        (self.now_ms)()
    }

    /// Ids that sort in time order across instances.
    fn next_change_id(&self) -> String {
        let n = self.sequence.fetch_add(1, Ordering::Relaxed);
        format!(
            "{:015}-{:08}-{}",
            self.now(),
            n % 100_000_000,
            &uuid::Uuid::new_v4().simple().to_string()[..8]
        )
    }

    async fn find_recording(&self, key: &RecordingKey) -> Result<Option<String>> {
        for k in keys_of(key) {
            if let Some(v) = self.store.get("recording_keys", &k).await? {
                return Ok(v["recording"].as_str().map(str::to_string));
            }
        }
        Ok(None)
    }

    /// The recording for these keys, created if none of them is known.
    /// Keys already pointing elsewhere keep pointing there; nothing is merged.
    async fn resolve_recording(&self, key: &RecordingKey) -> Result<String> {
        let recording = match self.find_recording(key).await? {
            Some(r) => r,
            None => format!("r-{}", uuid::Uuid::new_v4().simple()),
        };
        for k in keys_of(key) {
            self.store
                .create("recording_keys", &k, &json!({ "recording": recording }))
                .await?;
        }
        Ok(recording)
    }

    pub async fn submit(&self, account: &str, req: &SubmitRequest) -> Result<SubmitResponse> {
        req.validate().map_err(|p| Error::Problem(400, p))?;
        let day = self.now() / 86_400_000;
        let mut acks = vec![];
        for c in &req.contributions {
            let key = c.idempotency_key.clone();
            if let Err(p) = c.validate() {
                acks.push(Ack::Rejected {
                    idempotency_key: key,
                    reason: p.code,
                });
                continue;
            }
            let id = format!("{account}~{key}");
            if self.store.get("contributions", &id).await?.is_some() {
                acks.push(Ack::Duplicate {
                    idempotency_key: key,
                });
                continue;
            }
            if self
                .store
                .increment("quotas", &format!("{account}~{day}"))
                .await?
                > DAILY_CONTRIBUTIONS
            {
                acks.push(Ack::Rejected {
                    idempotency_key: key,
                    reason: "daily_limit".into(),
                });
                continue;
            }
            let recording = self.resolve_recording(&c.recording).await?;
            if !self
                .store
                .create(
                    "contributions",
                    &id,
                    &json!({ "recording": recording, "at": self.now() }),
                )
                .await?
            {
                // Another request with the same key won the race.
                acks.push(Ack::Duplicate {
                    idempotency_key: key,
                });
                continue;
            }
            let own = format!("{recording}~{account}");
            if let Some(m) = &c.metadata {
                self.store
                    .put(
                        "recording_meta",
                        &own,
                        &json!({ "metadata": m, "correction": c.correction, "at": self.now() }),
                    )
                    .await?;
            }
            if !c.references.is_empty() {
                self.store
                    .put(
                        "recording_refs",
                        &own,
                        &json!({ "references": c.references }),
                    )
                    .await?;
            }
            if let Some(f) = &c.features {
                self.store
                    .put(
                        "features",
                        &format!("{recording}~{}~{account}", version_id(&f.version)),
                        &serde_json::to_value(f).expect("serialisable"),
                    )
                    .await?;
            }
            self.store
                .put(
                    "changes",
                    &self.next_change_id(),
                    &json!({ "recording": recording }),
                )
                .await?;
            acks.push(Ack::Accepted {
                idempotency_key: key,
            });
        }
        Ok(SubmitResponse { acks })
    }

    /// What contributors say about a recording. The consensus is the
    /// metadata most accounts gave; every other version is kept.
    pub async fn entry(&self, recording: &str) -> Result<Option<CatalogueEntry>> {
        let prefix = format!("{recording}~");
        let metas = self.store.list_prefix("recording_meta", &prefix).await?;
        let features = self.store.list_prefix("features", &prefix).await?;
        let refs = self.store.list_prefix("recording_refs", &prefix).await?;
        if metas.is_empty() && features.is_empty() && refs.is_empty() {
            return Ok(None);
        }
        let mut votes: BTreeMap<String, (usize, Metadata)> = BTreeMap::new();
        for (_, v) in &metas {
            if let Ok(m) = serde_json::from_value::<Metadata>(v["metadata"].clone()) {
                let k = serde_json::to_string(&m).unwrap_or_default();
                votes.entry(k).or_insert((0, m)).0 += 1;
            }
        }
        let mut ranked: Vec<(usize, Metadata)> = votes.into_values().collect();
        ranked.sort_by_key(|r| std::cmp::Reverse(r.0));
        let mut ranked = ranked.into_iter().map(|(_, m)| m);
        let metadata = ranked.next().unwrap_or_default();
        let alternatives = ranked.collect();
        let mut feature_versions: Vec<FeatureVersion> = vec![];
        for (_, v) in &features {
            if let Ok(f) = serde_json::from_value::<Features>(v.clone()) {
                if !feature_versions.contains(&f.version) {
                    feature_versions.push(f.version);
                }
            }
        }
        let mut references: Vec<Reference> = vec![];
        for (_, v) in &refs {
            for r in serde_json::from_value::<Vec<Reference>>(v["references"].clone())
                .unwrap_or_default()
            {
                if !references.contains(&r) {
                    references.push(r);
                }
            }
        }
        Ok(Some(CatalogueEntry {
            recording_id: recording.into(),
            metadata,
            alternatives,
            feature_versions,
            references,
        }))
    }

    pub async fn lookup(&self, req: &LookupRequest) -> Result<LookupResponse> {
        req.validate().map_err(|p| Error::Problem(400, p))?;
        let mut results = vec![];
        for key in &req.recordings {
            results.push(match self.find_recording(key).await? {
                Some(r) => self.entry(&r).await?,
                None => None,
            });
        }
        Ok(LookupResponse { results })
    }

    /// The embedding most contributors agree on, with how many agree.
    pub async fn features(&self, req: &FeaturesRequest) -> Result<FeaturesResponse> {
        if req.version.shared_dims().is_none() {
            return Err(problem(
                400,
                "unsupported_model",
                "that analysis version is not shared",
            ));
        }
        crate::store::check_id(&req.recording_id)
            .map_err(|_| problem(400, "bad_recording", "invalid recording id"))?;
        let rows = self
            .store
            .list_prefix(
                "features",
                &format!("{}~{}~", req.recording_id, version_id(&req.version)),
            )
            .await?;
        let all: Vec<Features> = rows
            .into_iter()
            .filter_map(|(_, v)| serde_json::from_value(v).ok())
            .collect();
        let best = all
            .iter()
            .map(|f| {
                let agree = all
                    .iter()
                    .filter(|g| cosine(&f.embedding, &g.embedding) >= AGREEMENT)
                    .count();
                (agree, f)
            })
            .max_by_key(|(n, _)| *n);
        Ok(match best {
            Some((n, f)) => FeaturesResponse {
                features: Some(f.clone()),
                agreeing_contributors: n as u32,
            },
            None => FeaturesResponse {
                features: None,
                agreeing_contributors: 0,
            },
        })
    }

    pub async fn changes(&self, req: &ChangesRequest) -> Result<ChangesResponse> {
        req.validate().map_err(|p| Error::Problem(400, p))?;
        let after = req.cursor.clone().unwrap_or_default();
        let rows = self
            .store
            .list_after("changes", &after, req.limit as usize)
            .await?;
        let next_cursor = rows.last().map(|(id, _)| id.clone()).or(req.cursor.clone());
        let mut seen = vec![];
        let mut changes = vec![];
        for (_, v) in rows {
            let Some(r) = v["recording"].as_str() else {
                continue;
            };
            if seen.iter().any(|s| s == r) {
                continue;
            }
            seen.push(r.to_string());
            if let Some(e) = self.entry(r).await? {
                changes.push(e);
            }
        }
        Ok(ChangesResponse {
            changes,
            next_cursor,
        })
    }

    // Private backups. The account always comes from the verified token.

    fn backup_key(account: &str, id: &str) -> String {
        format!("accounts/{account}/{id}.json")
    }

    fn check_backup_id(id: &str) -> Result<()> {
        if id.is_empty()
            || id.len() > 64
            || !id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err(problem(
                400,
                "bad_backup_id",
                "backup ids are 1 to 64 letters, digits, - or _",
            ));
        }
        Ok(())
    }

    /// Store a snapshot under the caller's account. Uploading again with
    /// the same id replaces that backup, so a retried upload is harmless.
    pub async fn put_backup(&self, account: &str, id: &str, body: &[u8]) -> Result<BackupInfo> {
        Self::check_backup_id(id)?;
        if body.len() > MAX_SNAPSHOT_BYTES {
            return Err(problem(
                413,
                "too_large",
                "the snapshot is larger than 20 MiB",
            ));
        }
        let snapshot: Snapshot = serde_json::from_slice(body)
            .map_err(|e| problem(400, "bad_snapshot", &e.to_string()))?;
        snapshot.validate().map_err(|p| Error::Problem(400, p))?;
        self.blobs
            .put(&Self::backup_key(account, id), body.to_vec())
            .await?;
        let info = BackupInfo {
            id: id.into(),
            created_at_ms: self.now(),
            size_bytes: body.len() as u64,
            version: snapshot.version,
        };
        self.store
            .put(
                "backups",
                &format!("{account}~{id}"),
                &serde_json::to_value(&info).expect("serialisable"),
            )
            .await?;
        Ok(info)
    }

    pub async fn list_backups(&self, account: &str) -> Result<Vec<BackupInfo>> {
        let mut out: Vec<BackupInfo> = self
            .store
            .list_prefix("backups", &format!("{account}~"))
            .await?
            .into_iter()
            .filter_map(|(_, v)| serde_json::from_value(v).ok())
            .collect();
        out.sort_by_key(|b| std::cmp::Reverse(b.created_at_ms));
        Ok(out)
    }

    pub async fn get_backup(&self, account: &str, id: &str) -> Result<Vec<u8>> {
        Self::check_backup_id(id)?;
        if self
            .store
            .get("backups", &format!("{account}~{id}"))
            .await?
            .is_none()
        {
            return Err(problem(404, "not_found", "no such backup"));
        }
        self.blobs
            .get(&Self::backup_key(account, id))
            .await?
            .ok_or_else(|| problem(404, "not_found", "no such backup"))
    }

    pub async fn delete_backup(&self, account: &str, id: &str) -> Result<()> {
        Self::check_backup_id(id)?;
        if !self
            .store
            .delete("backups", &format!("{account}~{id}"))
            .await?
        {
            return Err(problem(404, "not_found", "no such backup"));
        }
        self.blobs.delete(&Self::backup_key(account, id)).await?;
        Ok(())
    }
}
