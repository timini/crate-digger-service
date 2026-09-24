//! In-memory store and blobs for tests and local runs.

use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;

use async_trait::async_trait;
use serde_json::Value;

use super::{check_id, Blobs, Store, StoreResult};

#[derive(Default)]
pub struct MemoryStore {
    docs: Mutex<HashMap<String, BTreeMap<String, Value>>>,
}

#[async_trait]
impl Store for MemoryStore {
    async fn create(&self, collection: &str, id: &str, value: &Value) -> StoreResult<bool> {
        check_id(id)?;
        let mut docs = self.docs.lock().unwrap();
        let c = docs.entry(collection.into()).or_default();
        if c.contains_key(id) {
            return Ok(false);
        }
        c.insert(id.into(), value.clone());
        Ok(true)
    }

    async fn get(&self, collection: &str, id: &str) -> StoreResult<Option<Value>> {
        check_id(id)?;
        Ok(self
            .docs
            .lock()
            .unwrap()
            .get(collection)
            .and_then(|c| c.get(id))
            .cloned())
    }

    async fn put(&self, collection: &str, id: &str, value: &Value) -> StoreResult<()> {
        check_id(id)?;
        self.docs
            .lock()
            .unwrap()
            .entry(collection.into())
            .or_default()
            .insert(id.into(), value.clone());
        Ok(())
    }

    async fn delete(&self, collection: &str, id: &str) -> StoreResult<bool> {
        check_id(id)?;
        Ok(self
            .docs
            .lock()
            .unwrap()
            .get_mut(collection)
            .and_then(|c| c.remove(id))
            .is_some())
    }

    async fn list_prefix(
        &self,
        collection: &str,
        prefix: &str,
    ) -> StoreResult<Vec<(String, Value)>> {
        Ok(self
            .docs
            .lock()
            .unwrap()
            .get(collection)
            .map(|c| {
                c.range(prefix.to_string()..)
                    .take_while(|(k, _)| k.starts_with(prefix))
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect()
            })
            .unwrap_or_default())
    }

    async fn list_after(
        &self,
        collection: &str,
        after: &str,
        limit: usize,
    ) -> StoreResult<Vec<(String, Value)>> {
        Ok(self
            .docs
            .lock()
            .unwrap()
            .get(collection)
            .map(|c| {
                c.range::<str, _>((std::ops::Bound::Excluded(after), std::ops::Bound::Unbounded))
                    .take(limit)
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect()
            })
            .unwrap_or_default())
    }

    async fn increment(&self, collection: &str, id: &str) -> StoreResult<i64> {
        check_id(id)?;
        let mut docs = self.docs.lock().unwrap();
        let c = docs.entry(collection.into()).or_default();
        let n = c.get(id).and_then(|v| v["count"].as_i64()).unwrap_or(0) + 1;
        c.insert(id.into(), serde_json::json!({ "count": n }));
        Ok(n)
    }
}

#[derive(Default)]
pub struct MemoryBlobs {
    blobs: Mutex<HashMap<String, Vec<u8>>>,
}

#[async_trait]
impl Blobs for MemoryBlobs {
    async fn put(&self, key: &str, bytes: Vec<u8>) -> StoreResult<()> {
        self.blobs.lock().unwrap().insert(key.into(), bytes);
        Ok(())
    }
    async fn get(&self, key: &str) -> StoreResult<Option<Vec<u8>>> {
        Ok(self.blobs.lock().unwrap().get(key).cloned())
    }
    async fn delete(&self, key: &str) -> StoreResult<()> {
        self.blobs.lock().unwrap().remove(key);
        Ok(())
    }
}
