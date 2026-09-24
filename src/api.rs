//! HTTP routes. Every route except the health check needs a verified
//! Google ID token; the account comes only from that token.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, FromRequestParts, Path, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use cd_protocol::backup::MAX_SNAPSHOT_BYTES;
use cd_protocol::*;

use crate::auth::{AuthError, Verifier};
use crate::catalogue::{Catalogue, Error};

pub struct App {
    pub catalogue: Catalogue,
    pub verifier: Arc<dyn Verifier>,
    pub limiter: RateLimiter,
}

/// Requests per second each account may make, with a burst allowance.
pub struct RateLimiter {
    rate: f64,
    burst: f64,
    buckets: Mutex<HashMap<String, (f64, Instant)>>,
}

impl RateLimiter {
    pub fn new(rate: f64, burst: f64) -> Self {
        Self {
            rate,
            burst,
            buckets: Mutex::default(),
        }
    }

    fn allow(&self, account: &str) -> bool {
        let mut b = self.buckets.lock().unwrap();
        let now = Instant::now();
        let (tokens, at) = b.entry(account.into()).or_insert((self.burst, now));
        *tokens = (*tokens + at.elapsed().as_secs_f64() * self.rate).min(self.burst);
        *at = now;
        if *tokens >= 1.0 {
            *tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

pub struct ApiError(StatusCode, Problem);

impl ApiError {
    fn new(status: StatusCode, code: &str, message: &str) -> Self {
        ApiError(
            status,
            Problem {
                code: code.into(),
                message: message.into(),
            },
        )
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(self.1)).into_response()
    }
}

impl From<Error> for ApiError {
    fn from(e: Error) -> Self {
        match e {
            Error::Problem(status, p) => ApiError(
                StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_REQUEST),
                p,
            ),
            Error::Store(e) => {
                tracing::error!("storage: {e}");
                ApiError::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "unavailable",
                    "storage is unavailable; retry later",
                )
            }
        }
    }
}

/// The signed-in account.
pub struct Caller(pub String);

impl FromRequestParts<Arc<App>> for Caller {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        app: &Arc<App>,
    ) -> Result<Self, Self::Rejection> {
        let token = parts
            .headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .ok_or_else(|| {
                ApiError::new(StatusCode::UNAUTHORIZED, "sign_in", "sign in with Google")
            })?;
        let account = app.verifier.verify(token).await.map_err(|e| match e {
            AuthError::Unavailable(m) => {
                tracing::warn!("key fetch: {m}");
                ApiError::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "unavailable",
                    "cannot check sign-in; retry later",
                )
            }
            AuthError::Missing | AuthError::Invalid(_) => ApiError::new(
                StatusCode::UNAUTHORIZED,
                "sign_in",
                "the sign-in is invalid or expired",
            ),
        })?;
        if !app.limiter.allow(&account.id) {
            return Err(ApiError::new(
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limited",
                "too many requests; slow down",
            ));
        }
        Ok(Caller(account.id))
    }
}

async fn submit(
    State(app): State<Arc<App>>,
    Caller(a): Caller,
    Json(req): Json<SubmitRequest>,
) -> Result<Json<SubmitResponse>, ApiError> {
    Ok(Json(app.catalogue.submit(&a, &req).await?))
}

async fn lookup(
    State(app): State<Arc<App>>,
    _: Caller,
    Json(req): Json<LookupRequest>,
) -> Result<Json<LookupResponse>, ApiError> {
    Ok(Json(app.catalogue.lookup(&req).await?))
}

async fn features(
    State(app): State<Arc<App>>,
    _: Caller,
    Json(req): Json<FeaturesRequest>,
) -> Result<Json<FeaturesResponse>, ApiError> {
    Ok(Json(app.catalogue.features(&req).await?))
}

async fn changes(
    State(app): State<Arc<App>>,
    _: Caller,
    Json(req): Json<ChangesRequest>,
) -> Result<Json<ChangesResponse>, ApiError> {
    Ok(Json(app.catalogue.changes(&req).await?))
}

async fn list_backups(
    State(app): State<Arc<App>>,
    Caller(a): Caller,
) -> Result<Json<Vec<backup::BackupInfo>>, ApiError> {
    Ok(Json(app.catalogue.list_backups(&a).await?))
}

async fn put_backup(
    State(app): State<Arc<App>>,
    Caller(a): Caller,
    Path(id): Path<String>,
    body: Bytes,
) -> Result<Json<backup::BackupInfo>, ApiError> {
    Ok(Json(app.catalogue.put_backup(&a, &id, &body).await?))
}

async fn get_backup(
    State(app): State<Arc<App>>,
    Caller(a): Caller,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let bytes = app.catalogue.get_backup(&a, &id).await?;
    let mut headers = HeaderMap::new();
    headers.insert("content-type", "application/json".parse().expect("static"));
    Ok((headers, bytes).into_response())
}

async fn delete_backup(
    State(app): State<Arc<App>>,
    Caller(a): Caller,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    app.catalogue.delete_backup(&a, &id).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub fn router(app: Arc<App>) -> Router {
    let shared = Router::new()
        .route("/v1/contributions", post(submit))
        .route("/v1/lookup", post(lookup))
        .route("/v1/features", post(features))
        .route("/v1/changes", post(changes))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES));
    let backups = Router::new()
        .route("/v1/backups", get(list_backups))
        .route(
            "/v1/backups/{id}",
            get(get_backup).put(put_backup).delete(delete_backup),
        )
        .layer(DefaultBodyLimit::max(MAX_SNAPSHOT_BYTES));
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .merge(shared)
        .merge(backups)
        .with_state(app)
}
