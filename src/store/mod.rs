//! Storage primitives. Everything the service keeps goes through these few
//! operations, so the in-memory store used by tests and the Firestore store
//! used in production behave the same, and the catalogue rules live above
//! them in one place.
//!
//! Values are JSON documents. Ids must not contain `/`.

pub mod firestore;
pub mod memory;

use async_trait::async_trait;
use serde_json::Value;

#[derive(Debug)]
pub enum StoreError {
    Unavailable(String),
    Invalid(String),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Unavailable(m) => write!(f, "storage unavailable: {m}"),
            StoreError::Invalid(m) => write!(f, "invalid storage request: {m}"),
        }
    }
}

pub type StoreResult<T> = Result<T, StoreError>;

#[async_trait]
pub trait Store: Send + Sync {
    /// Create only if absent. Returns false, and changes nothing, if it exists.
    async fn create(&self, collection: &str, id: &str, value: &Value) -> StoreResult<bool>;
    async fn get(&self, collection: &str, id: &str) -> StoreResult<Option<Value>>;
    /// Create or replace.
    async fn put(&self, collection: &str, id: &str, value: &Value) -> StoreResult<()>;
    /// Returns whether it existed.
    async fn delete(&self, collection: &str, id: &str) -> StoreResult<bool>;
    /// Every document whose id starts with `prefix`, ordered by id.
    async fn list_prefix(
        &self,
        collection: &str,
        prefix: &str,
    ) -> StoreResult<Vec<(String, Value)>>;
    /// Up to `limit` documents with ids greater than `after`, ordered by id.
    async fn list_after(
        &self,
        collection: &str,
        after: &str,
        limit: usize,
    ) -> StoreResult<Vec<(String, Value)>>;
    /// Atomically add one to a counter and return the new value.
    async fn increment(&self, collection: &str, id: &str) -> StoreResult<i64>;
}

pub fn check_id(id: &str) -> StoreResult<()> {
    if id.is_empty() || id.len() > 700 || id.contains('/') || id == "." || id == ".." {
        return Err(StoreError::Invalid("bad document id".into()));
    }
    Ok(())
}

/// Blob storage for backup snapshots.
#[async_trait]
pub trait Blobs: Send + Sync {
    async fn put(&self, key: &str, bytes: Vec<u8>) -> StoreResult<()>;
    async fn get(&self, key: &str) -> StoreResult<Option<Vec<u8>>>;
    async fn delete(&self, key: &str) -> StoreResult<()>;
}
