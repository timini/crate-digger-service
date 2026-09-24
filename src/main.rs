//! Configuration comes from the environment:
//! - `PORT` (Cloud Run sets it; default 8080)
//! - `GOOGLE_CLIENT_IDS`: comma-separated OAuth client ids whose ID tokens are accepted
//! - `STORE`: `firestore` (default) or `memory` for local runs
//! - `GCP_PROJECT`, `BACKUP_BUCKET`: for the Firestore store and backups
//! - `FIRESTORE_EMULATOR_HOST`: use the emulator instead of Firestore

use std::sync::Arc;

use cd_service::api::{router, App, RateLimiter};
use cd_service::auth::{GoogleKeys, GoogleVerifier};
use cd_service::catalogue::Catalogue;
use cd_service::store::firestore::{Firestore, Gcs, Tokens};
use cd_service::store::memory::{MemoryBlobs, MemoryStore};
use cd_service::store::{Blobs, Store};

fn env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .expect("http client");
    let audience: Vec<String> = env("GOOGLE_CLIENT_IDS")
        .expect("GOOGLE_CLIENT_IDS must be set")
        .split(',')
        .map(|s| s.trim().to_string())
        .collect();
    let (store, blobs): (Arc<dyn Store>, Arc<dyn Blobs>) = match env("STORE").as_deref() {
        Some("memory") => (
            Arc::new(MemoryStore::default()),
            Arc::new(MemoryBlobs::default()),
        ),
        _ => {
            let project = env("GCP_PROJECT").expect("GCP_PROJECT must be set");
            let bucket = env("BACKUP_BUCKET").expect("BACKUP_BUCKET must be set");
            let tokens = || {
                if env("FIRESTORE_EMULATOR_HOST").is_some() {
                    Tokens::none(http.clone())
                } else {
                    Tokens::metadata_server(http.clone())
                }
            };
            (
                Arc::new(Firestore::new(http.clone(), &project, tokens())),
                Arc::new(Gcs::new(
                    http.clone(),
                    &bucket,
                    Tokens::metadata_server(http.clone()),
                )),
            )
        }
    };
    let app = Arc::new(App {
        catalogue: Catalogue::new(
            store,
            blobs,
            Arc::new(|| {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0)
            }),
        ),
        verifier: Arc::new(GoogleVerifier::new(Arc::new(GoogleKeys { http }), audience)),
        limiter: RateLimiter::new(10.0, 40.0),
    });
    let port = env("PORT").unwrap_or_else(|| "8080".into());
    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{port}"))
        .await
        .expect("bind");
    tracing::info!(port, "listening");
    axum::serve(listener, router(app))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .expect("server");
}
