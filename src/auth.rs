//! Google sign-in. Every request carries a Google ID token issued to the
//! Crate Digger app. The token's signature is checked against Google's
//! published keys, along with its issuer, audience (the app's OAuth client
//! id) and expiry. The account is the token's subject, never anything the
//! request says about itself.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
use serde::Deserialize;
use tokio::sync::RwLock;

pub const GOOGLE_CERTS: &str = "https://www.googleapis.com/oauth2/v3/certs";
const ISSUERS: [&str; 2] = ["accounts.google.com", "https://accounts.google.com"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    /// Google's stable subject id.
    pub id: String,
}

#[derive(Debug)]
pub enum AuthError {
    Missing,
    Invalid(String),
    Unavailable(String),
}

#[async_trait]
pub trait Verifier: Send + Sync {
    async fn verify(&self, token: &str) -> Result<Account, AuthError>;
}

#[derive(Debug, Clone, Deserialize)]
pub struct Jwk {
    pub kid: String,
    pub n: String,
    pub e: String,
}

/// Where the signing keys come from: Google, or a test's own keys.
#[async_trait]
pub trait KeySource: Send + Sync {
    async fn keys(&self) -> Result<Vec<Jwk>, AuthError>;
}

pub struct GoogleKeys {
    pub http: reqwest::Client,
}

#[async_trait]
impl KeySource for GoogleKeys {
    async fn keys(&self) -> Result<Vec<Jwk>, AuthError> {
        #[derive(Deserialize)]
        struct Set {
            keys: Vec<Jwk>,
        }
        let set: Set = self
            .http
            .get(GOOGLE_CERTS)
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .map_err(|e| AuthError::Unavailable(e.to_string()))?
            .json()
            .await
            .map_err(|e| AuthError::Unavailable(e.to_string()))?;
        Ok(set.keys)
    }
}

#[derive(Deserialize)]
struct Claims {
    sub: String,
}

pub struct GoogleVerifier {
    source: Arc<dyn KeySource>,
    audience: Vec<String>,
    /// Keys by id, refreshed at most once a minute and at least hourly.
    cache: RwLock<(HashMap<String, Jwk>, Option<Instant>)>,
}

impl GoogleVerifier {
    pub fn new(source: Arc<dyn KeySource>, audience: Vec<String>) -> Self {
        Self {
            source,
            audience,
            cache: RwLock::new((HashMap::new(), None)),
        }
    }

    async fn key(&self, kid: &str) -> Result<Jwk, AuthError> {
        {
            let (keys, fetched) = &*self.cache.read().await;
            let fresh = fetched.is_some_and(|t| t.elapsed() < Duration::from_secs(3600));
            if let (Some(k), true) = (keys.get(kid), fresh) {
                return Ok(k.clone());
            }
        }
        let mut cache = self.cache.write().await;
        // An unknown kid may mean Google rotated keys; refetch, but not in a loop.
        if cache
            .1
            .is_none_or(|t| t.elapsed() > Duration::from_secs(60))
            || !cache.0.contains_key(kid)
        {
            let keys = self.source.keys().await?;
            cache.0 = keys.into_iter().map(|k| (k.kid.clone(), k)).collect();
            cache.1 = Some(Instant::now());
        }
        cache
            .0
            .get(kid)
            .cloned()
            .ok_or_else(|| AuthError::Invalid("unknown signing key".into()))
    }
}

#[async_trait]
impl Verifier for GoogleVerifier {
    async fn verify(&self, token: &str) -> Result<Account, AuthError> {
        let header = decode_header(token).map_err(|e| AuthError::Invalid(e.to_string()))?;
        if header.alg != Algorithm::RS256 {
            return Err(AuthError::Invalid("unexpected algorithm".into()));
        }
        let kid = header
            .kid
            .ok_or_else(|| AuthError::Invalid("no key id".into()))?;
        let jwk = self.key(&kid).await?;
        let key = DecodingKey::from_rsa_components(&jwk.n, &jwk.e)
            .map_err(|e| AuthError::Invalid(e.to_string()))?;
        let mut v = Validation::new(Algorithm::RS256);
        v.set_audience(&self.audience);
        v.set_issuer(&ISSUERS);
        v.leeway = 30;
        let data =
            decode::<Claims>(token, &key, &v).map_err(|e| AuthError::Invalid(e.to_string()))?;
        if data.claims.sub.is_empty()
            || data.claims.sub.len() > 255
            || data.claims.sub.contains(['/', '~'])
        {
            return Err(AuthError::Invalid("unusable subject".into()));
        }
        Ok(Account {
            id: data.claims.sub,
        })
    }
}
