//! The same storage behaviour from the in-memory store and from Firestore.
//! The Firestore half runs when FIRESTORE_EMULATOR_HOST is set (CI starts
//! the emulator); otherwise it is skipped with a message.

use cd_service::store::firestore::{Firestore, Tokens};
use cd_service::store::memory::MemoryStore;
use cd_service::store::Store;
use serde_json::json;

async fn conformance(s: &dyn Store, run: &str) {
    let c = format!("things{run}");
    assert!(s.create(&c, "a~1", &json!({"v": 1})).await.unwrap());
    assert!(
        !s.create(&c, "a~1", &json!({"v": 2})).await.unwrap(),
        "create never overwrites"
    );
    assert_eq!(s.get(&c, "a~1").await.unwrap(), Some(json!({"v": 1})));
    assert_eq!(s.get(&c, "missing").await.unwrap(), None);

    s.put(&c, "a~1", &json!({"v": 3})).await.unwrap();
    s.put(&c, "a~2", &json!({"v": "two"})).await.unwrap();
    s.put(&c, "ab~1", &json!({"v": "other prefix"}))
        .await
        .unwrap();
    s.put(&c, "b~1", &json!({"v": 4})).await.unwrap();
    let a: Vec<String> = s
        .list_prefix(&c, "a~")
        .await
        .unwrap()
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    assert_eq!(a, vec!["a~1", "a~2"]);
    assert_eq!(s.get(&c, "a~1").await.unwrap(), Some(json!({"v": 3})));

    let first: Vec<String> = s
        .list_after(&c, "", 2)
        .await
        .unwrap()
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    // Byte order: "b" sorts before "~".
    assert_eq!(first, vec!["ab~1", "a~1"]);
    let rest: Vec<String> = s
        .list_after(&c, "a~1", 10)
        .await
        .unwrap()
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    assert_eq!(rest, vec!["a~2", "b~1"]);

    assert!(s.delete(&c, "b~1").await.unwrap());
    assert!(!s.delete(&c, "b~1").await.unwrap());
    assert!(s.get(&c, "b~1").await.unwrap().is_none());

    let q = format!("quotas{run}");
    assert_eq!(s.increment(&q, "alice~1").await.unwrap(), 1);
    assert_eq!(s.increment(&q, "alice~1").await.unwrap(), 2);
    assert_eq!(s.increment(&q, "bob~1").await.unwrap(), 1);

    assert!(s.put(&c, "has/slash", &json!({})).await.is_err());
}

#[tokio::test]
async fn memory_store() {
    conformance(&MemoryStore::default(), "").await;
}

#[tokio::test]
async fn firestore_store() {
    if std::env::var("FIRESTORE_EMULATOR_HOST").is_err() {
        eprintln!("skipped: FIRESTORE_EMULATOR_HOST is not set");
        return;
    }
    let http = reqwest::Client::new();
    let fs = Firestore::new(http.clone(), "demo-crate-digger", Tokens::none(http));
    let run = uuid::Uuid::new_v4().simple().to_string();
    conformance(&fs, &run[..8]).await;
}
