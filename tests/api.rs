//! The service end to end over HTTP, with the in-memory store and real
//! RS256 tokens signed by a test key.

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use base64::Engine;
use cd_protocol::backup::{Playlist, Rating, Seed, Snapshot, Track, SNAPSHOT_VERSION};
use cd_protocol::*;
use cd_service::api::{router, App, RateLimiter};
use cd_service::auth::{AuthError, GoogleVerifier, Jwk, KeySource};
use cd_service::catalogue::Catalogue;
use cd_service::store::memory::{MemoryBlobs, MemoryStore};
use http_body_util::BodyExt;
use rsa::pkcs1::EncodeRsaPrivateKey;
use rsa::traits::PublicKeyParts;
use serde_json::{json, Value};
use tower::ServiceExt;

const CLIENT: &str = "crate-digger-desktop.apps.googleusercontent.com";

fn key() -> &'static rsa::RsaPrivateKey {
    static KEY: OnceLock<rsa::RsaPrivateKey> = OnceLock::new();
    KEY.get_or_init(|| rsa::RsaPrivateKey::new(&mut rand::thread_rng(), 2048).unwrap())
}

struct TestKeys;

#[async_trait]
impl KeySource for TestKeys {
    async fn keys(&self) -> Result<Vec<Jwk>, AuthError> {
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        Ok(vec![Jwk {
            kid: "test".into(),
            n: b64.encode(key().n().to_bytes_be()),
            e: b64.encode(key().e().to_bytes_be()),
        }])
    }
}

fn now_s() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn token_with(sub: &str, aud: &str, iss: &str, exp: i64) -> String {
    let der = key().to_pkcs1_der().unwrap();
    let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
    header.kid = Some("test".into());
    jsonwebtoken::encode(
        &header,
        &json!({ "sub": sub, "aud": aud, "iss": iss, "exp": exp, "iat": now_s() }),
        &jsonwebtoken::EncodingKey::from_rsa_der(der.as_bytes()),
    )
    .unwrap()
}

fn token(sub: &str) -> String {
    token_with(sub, CLIENT, "https://accounts.google.com", now_s() + 3600)
}

fn app_with(rate: f64, burst: f64) -> axum::Router {
    router(Arc::new(App {
        catalogue: Catalogue::new(
            Arc::new(MemoryStore::default()),
            Arc::new(MemoryBlobs::default()),
            Arc::new(|| {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_millis() as i64
            }),
        ),
        verifier: Arc::new(GoogleVerifier::new(Arc::new(TestKeys), vec![CLIENT.into()])),
        limiter: RateLimiter::new(rate, burst),
    }))
}

fn app() -> axum::Router {
    app_with(1000.0, 1000.0)
}

async fn call(
    app: &axum::Router,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<Vec<u8>>,
) -> (StatusCode, Value) {
    let mut req = Request::builder().method(method).uri(path);
    if let Some(t) = token {
        req = req.header("authorization", format!("Bearer {t}"));
    }
    req = req.header("content-type", "application/json");
    let res = app
        .clone()
        .oneshot(
            req.body(body.map(Body::from).unwrap_or_else(Body::empty))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn post<T: serde::Serialize>(
    app: &axum::Router,
    path: &str,
    who: &str,
    body: &T,
) -> (StatusCode, Value) {
    call(
        app,
        "POST",
        path,
        Some(&token(who)),
        Some(serde_json::to_vec(body).unwrap()),
    )
    .await
}

fn effnet() -> FeatureVersion {
    let (v, _) = SHARED_FEATURE_VERSIONS[0];
    FeatureVersion {
        model_id: v.model_id.into(),
        weights_checksum: v.weights_checksum.into(),
        preprocessing_version: v.preprocessing_version.into(),
    }
}

fn key_fp(h: &str) -> RecordingKey {
    RecordingKey {
        fingerprint_hash: Some(h.into()),
        external_ids: vec![],
    }
}

fn contribution(
    k: &str,
    recording: RecordingKey,
    title: &str,
    embedding: Option<Vec<f32>>,
) -> Contribution {
    Contribution {
        idempotency_key: k.into(),
        recording,
        metadata: Some(Metadata {
            artist: Some("Kerri Chandler".into()),
            title: Some(title.into()),
            ..Default::default()
        }),
        features: embedding.map(|e| Features {
            version: effnet(),
            embedding: e,
            tempo_bpm: None,
            key_camelot: None,
            loudness_lufs: None,
        }),
        references: vec![],
        correction: false,
    }
}

fn submit(cs: Vec<Contribution>) -> SubmitRequest {
    SubmitRequest { contributions: cs }
}

#[tokio::test]
async fn health_is_open_and_everything_else_needs_a_valid_google_token() {
    let app = app();
    assert_eq!(
        call(&app, "GET", "/healthz", None, None).await.0,
        StatusCode::OK
    );
    let body = serde_json::to_vec(&LookupRequest {
        recordings: vec![key_fp("a")],
    })
    .unwrap();
    assert_eq!(
        call(&app, "POST", "/v1/lookup", None, Some(body.clone()))
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    for bad in [
        token_with(
            "u1",
            "someone-elses-app",
            "https://accounts.google.com",
            now_s() + 3600,
        ),
        token_with("u1", CLIENT, "https://evil.example", now_s() + 3600),
        token_with("u1", CLIENT, "accounts.google.com", now_s() - 3600),
        "not.a.token".into(),
    ] {
        let (status, v) = call(&app, "POST", "/v1/lookup", Some(&bad), Some(body.clone())).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{v}");
        assert_eq!(v["code"], "sign_in");
    }
    assert_eq!(
        call(&app, "POST", "/v1/lookup", Some(&token("u1")), Some(body))
            .await
            .0,
        StatusCode::OK
    );
}

#[tokio::test]
async fn retries_are_duplicates_and_conflicts_are_kept() {
    let app = app();
    let a = contribution("k1", key_fp("fp1"), "Rain", None);
    let (s, v) = post(&app, "/v1/contributions", "alice", &submit(vec![a.clone()])).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["acks"][0]["status"], "accepted");
    let (_, v) = post(&app, "/v1/contributions", "alice", &submit(vec![a])).await;
    assert_eq!(v["acks"][0]["status"], "duplicate");

    // Two other accounts agree on another title; Alice's is kept as an alternative.
    for who in ["bob", "carol"] {
        post(
            &app,
            "/v1/contributions",
            who,
            &submit(vec![contribution("k1", key_fp("fp1"), "Rain (Dub)", None)]),
        )
        .await;
    }
    let (_, v) = post(
        &app,
        "/v1/lookup",
        "dave",
        &LookupRequest {
            recordings: vec![key_fp("fp1"), key_fp("unknown")],
        },
    )
    .await;
    let r: LookupResponse = serde_json::from_value(v).unwrap();
    let entry = r.results[0].as_ref().unwrap();
    assert_eq!(entry.metadata.title.as_deref(), Some("Rain (Dub)"));
    assert_eq!(entry.alternatives.len(), 1);
    assert_eq!(entry.alternatives[0].title.as_deref(), Some("Rain"));
    assert!(r.results[1].is_none());
}

#[tokio::test]
async fn keys_resolve_to_one_recording_and_bad_contributions_are_rejected() {
    let app = app();
    let both = RecordingKey {
        fingerprint_hash: Some("fp9".into()),
        external_ids: vec![ExternalId {
            source: "discogs_release".into(),
            id: "123".into(),
        }],
    };
    post(
        &app,
        "/v1/contributions",
        "alice",
        &submit(vec![contribution("k1", both, "Rain", None)]),
    )
    .await;
    let by_id = RecordingKey {
        fingerprint_hash: None,
        external_ids: vec![ExternalId {
            source: "discogs_release".into(),
            id: "123".into(),
        }],
    };
    let (_, v) = post(
        &app,
        "/v1/lookup",
        "bob",
        &LookupRequest {
            recordings: vec![key_fp("fp9"), by_id],
        },
    )
    .await;
    let r: LookupResponse = serde_json::from_value(v).unwrap();
    assert_eq!(
        r.results[0].as_ref().unwrap().recording_id,
        r.results[1].as_ref().unwrap().recording_id
    );

    let wrong = contribution("k2", key_fp("fp9"), "Rain", Some(vec![0.1; 10]));
    let (s, v) = post(&app, "/v1/contributions", "alice", &submit(vec![wrong])).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["acks"][0]["status"], "rejected");
    assert_eq!(v["acks"][0]["reason"], "wrong_dimensions");

    let huge = vec![b' '; MAX_BODY_BYTES + 10];
    let (s, _) = call(
        &app,
        "POST",
        "/v1/contributions",
        Some(&token("alice")),
        Some(huge),
    )
    .await;
    assert_eq!(s, StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn shared_features_are_the_ones_contributors_agree_on() {
    let app = app();
    let mut near = vec![0.5f32; 1280];
    let mut other = vec![0.5f32; 1280];
    other[..640].fill(-0.5);
    near[0] = 0.51;
    for (who, e) in [
        ("alice", vec![0.5f32; 1280]),
        ("bob", near),
        ("carol", other),
    ] {
        post(
            &app,
            "/v1/contributions",
            who,
            &submit(vec![contribution("f1", key_fp("fpx"), "Rain", Some(e))]),
        )
        .await;
    }
    let (_, v) = post(
        &app,
        "/v1/lookup",
        "dave",
        &LookupRequest {
            recordings: vec![key_fp("fpx")],
        },
    )
    .await;
    let recording = v["results"][0]["recording_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        v["results"][0]["feature_versions"][0]["model_id"],
        effnet().model_id
    );
    let (_, v) = post(
        &app,
        "/v1/features",
        "dave",
        &FeaturesRequest {
            recording_id: recording,
            version: effnet(),
        },
    )
    .await;
    let f: FeaturesResponse = serde_json::from_value(v).unwrap();
    assert_eq!(f.agreeing_contributors, 2);
    assert!(f.features.unwrap().embedding[640] > 0.0);
}

#[tokio::test]
async fn changes_page_through_with_a_cursor() {
    let app = app();
    for i in 0..5 {
        post(
            &app,
            "/v1/contributions",
            "alice",
            &submit(vec![contribution(
                &format!("c{i}"),
                key_fp(&format!("fp{i}")),
                "T",
                None,
            )]),
        )
        .await;
    }
    let (_, v) = post(
        &app,
        "/v1/changes",
        "bob",
        &ChangesRequest {
            cursor: None,
            limit: 3,
        },
    )
    .await;
    let first: ChangesResponse = serde_json::from_value(v).unwrap();
    assert_eq!(first.changes.len(), 3);
    let (_, v) = post(
        &app,
        "/v1/changes",
        "bob",
        &ChangesRequest {
            cursor: first.next_cursor.clone(),
            limit: 3,
        },
    )
    .await;
    let second: ChangesResponse = serde_json::from_value(v).unwrap();
    assert_eq!(second.changes.len(), 2);
    let (_, v) = post(
        &app,
        "/v1/changes",
        "bob",
        &ChangesRequest {
            cursor: second.next_cursor.clone(),
            limit: 3,
        },
    )
    .await;
    let done: ChangesResponse = serde_json::from_value(v).unwrap();
    assert!(done.changes.is_empty());
    assert_eq!(done.next_cursor, second.next_cursor);
}

fn snapshot() -> Snapshot {
    Snapshot {
        version: SNAPSHOT_VERSION,
        created_at_ms: 1,
        tracks: vec![Track {
            id: "t1".into(),
            metadata: Metadata::default(),
            fingerprint_hash: None,
            kept: true,
            file: None,
        }],
        ratings: vec![Rating {
            track: "t1".into(),
            kind: "star3".into(),
            at_ms: 1,
        }],
        seeds: vec![Seed {
            kind: "artist".into(),
            value: "Kerri Chandler".into(),
        }],
        playlists: vec![Playlist {
            name: "Friday".into(),
            tracks: vec!["t1".into()],
        }],
    }
}

#[tokio::test]
async fn backups_belong_only_to_their_account() {
    let app = app();
    let body = serde_json::to_vec(&snapshot()).unwrap();
    let (s, _) = call(
        &app,
        "PUT",
        "/v1/backups/b1",
        Some(&token("alice")),
        Some(body.clone()),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    // A retried upload replaces the same backup.
    call(
        &app,
        "PUT",
        "/v1/backups/b1",
        Some(&token("alice")),
        Some(body.clone()),
    )
    .await;
    let (_, v) = call(&app, "GET", "/v1/backups", Some(&token("alice")), None).await;
    assert_eq!(v.as_array().unwrap().len(), 1);

    let (s, v) = call(&app, "GET", "/v1/backups/b1", Some(&token("alice")), None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(serde_json::from_value::<Snapshot>(v).unwrap(), snapshot());

    // Another account cannot list, read, restore or delete it.
    let (_, v) = call(&app, "GET", "/v1/backups", Some(&token("mallory")), None).await;
    assert!(v.as_array().unwrap().is_empty());
    assert_eq!(
        call(&app, "GET", "/v1/backups/b1", Some(&token("mallory")), None)
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        call(
            &app,
            "DELETE",
            "/v1/backups/b1",
            Some(&token("mallory")),
            None
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        call(&app, "GET", "/v1/backups/b1", Some(&token("alice")), None)
            .await
            .0,
        StatusCode::OK
    );

    assert_eq!(
        call(
            &app,
            "DELETE",
            "/v1/backups/b1",
            Some(&token("alice")),
            None
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        call(&app, "GET", "/v1/backups/b1", Some(&token("alice")), None)
            .await
            .0,
        StatusCode::NOT_FOUND
    );

    let mut bad = snapshot();
    bad.ratings[0].track = "missing".into();
    let (s, v) = call(
        &app,
        "PUT",
        "/v1/backups/b2",
        Some(&token("alice")),
        Some(serde_json::to_vec(&bad).unwrap()),
    )
    .await;
    assert_eq!(
        (s, v["code"].as_str()),
        (StatusCode::BAD_REQUEST, Some("unknown_track"))
    );
    assert_eq!(
        call(
            &app,
            "PUT",
            "/v1/backups/..%2Fx",
            Some(&token("alice")),
            Some(body)
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn accounts_are_rate_limited() {
    let app = app_with(0.001, 3.0);
    let body = serde_json::to_vec(&LookupRequest {
        recordings: vec![key_fp("a")],
    })
    .unwrap();
    let mut statuses = vec![];
    for _ in 0..5 {
        statuses.push(
            call(
                &app,
                "POST",
                "/v1/lookup",
                Some(&token("alice")),
                Some(body.clone()),
            )
            .await
            .0,
        );
    }
    assert_eq!(
        statuses
            .iter()
            .filter(|s| **s == StatusCode::TOO_MANY_REQUESTS)
            .count(),
        2
    );
    // Another account has its own allowance.
    assert_eq!(
        call(&app, "POST", "/v1/lookup", Some(&token("bob")), Some(body))
            .await
            .0,
        StatusCode::OK
    );
}
