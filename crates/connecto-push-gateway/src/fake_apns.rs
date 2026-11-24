//! A stand-in for APNs, for tests: HTTP/2 without TLS (prior knowledge) on a
//! local port, checking each provider token's ES256 signature with the test
//! key, recording what it was sent, and answering as scripted (`200` once
//! the script is over).

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, PoisonError};

use base64::Engine as _;
use bytes::Bytes;
use http_body_util::{BodyExt as _, Full};
use hyper::{Request, Response};
use hyper_util::rt::{TokioExecutor, TokioIo};
use p256::ecdsa::signature::Verifier as _;
use p256::pkcs8::EncodePrivateKey as _;
use serde_json::Value;

/// One push as the fake APNs received it.
#[derive(Debug, Clone)]
pub struct Received {
    pub path: String,
    pub headers: HashMap<String, String>,
    pub body: Value,
    /// The provider token was the test key's, for the expected team and key id.
    pub token_ok: bool,
}

/// The test's APNs key: its `.p8` text and the key that checks its tokens.
#[must_use]
pub fn test_key() -> Option<(String, p256::ecdsa::VerifyingKey)> {
    let key = p256::ecdsa::SigningKey::from_bytes(&[7u8; 32].into()).ok()?;
    let pem = key.to_pkcs8_pem(p256::pkcs8::LineEnding::LF).ok()?;
    Some((pem.to_string(), *key.verifying_key()))
}

/// A running fake APNs.
pub struct FakeApns {
    pub addr: SocketAddr,
    pub received: Arc<Mutex<Vec<Received>>>,
    script: Arc<Mutex<VecDeque<(u16, String)>>>,
}

impl FakeApns {
    /// Starts one on the current tokio runtime, checking tokens for
    /// `team_id` and `key_id` with `key`.
    ///
    /// # Errors
    /// No local port.
    pub async fn start(
        key: p256::ecdsa::VerifyingKey,
        team_id: &str,
        key_id: &str,
    ) -> std::io::Result<Self> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let received = Arc::new(Mutex::new(Vec::new()));
        let script = Arc::new(Mutex::new(VecDeque::new()));
        let (rec, scr) = (Arc::clone(&received), Arc::clone(&script));
        let (team, kid) = (team_id.to_owned(), key_id.to_owned());
        tokio::spawn(async move {
            loop {
                let Ok((tcp, _)) = listener.accept().await else {
                    continue;
                };
                let (rec, scr, team, kid) = (
                    Arc::clone(&rec),
                    Arc::clone(&scr),
                    team.clone(),
                    kid.clone(),
                );
                tokio::spawn(async move {
                    let service =
                        hyper::service::service_fn(move |req: Request<hyper::body::Incoming>| {
                            let (rec, scr, team, kid) = (
                                Arc::clone(&rec),
                                Arc::clone(&scr),
                                team.clone(),
                                kid.clone(),
                            );
                            async move {
                                let path = req.uri().path().to_owned();
                                let headers: HashMap<String, String> = req
                                    .headers()
                                    .iter()
                                    .filter_map(|(k, v)| {
                                        Some((k.as_str().to_owned(), v.to_str().ok()?.to_owned()))
                                    })
                                    .collect();
                                let body = req
                                    .into_body()
                                    .collect()
                                    .await
                                    .map(|b| b.to_bytes())
                                    .unwrap_or_default();
                                let token_ok = headers
                                    .get("authorization")
                                    .and_then(|a| a.strip_prefix("bearer "))
                                    .is_some_and(|jwt| token_ok(jwt, &key, &team, &kid));
                                rec.lock()
                                    .unwrap_or_else(PoisonError::into_inner)
                                    .push(Received {
                                        path,
                                        headers,
                                        body: serde_json::from_slice(&body).unwrap_or(Value::Null),
                                        token_ok,
                                    });
                                let (status, reason) = scr
                                    .lock()
                                    .unwrap_or_else(PoisonError::into_inner)
                                    .pop_front()
                                    .unwrap_or((200, String::new()));
                                let body = if reason.is_empty() {
                                    Bytes::new()
                                } else {
                                    Bytes::from(serde_json::json!({ "reason": reason }).to_string())
                                };
                                let mut response = Response::new(Full::new(body));
                                *response.status_mut() = hyper::StatusCode::from_u16(status)
                                    .unwrap_or(hyper::StatusCode::OK);
                                Ok::<_, std::convert::Infallible>(response)
                            }
                        });
                    let _ = hyper::server::conn::http2::Builder::new(TokioExecutor::new())
                        .serve_connection(TokioIo::new(tcp), service)
                        .await;
                });
            }
        });
        Ok(Self {
            addr,
            received,
            script,
        })
    }

    /// The next answers, in order: `(status, reason)`.
    pub fn answer(&self, answers: &[(u16, &str)]) {
        self.script
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend(answers.iter().map(|(s, r)| (*s, (*r).to_owned())));
    }

    #[must_use]
    pub fn received(&self) -> Vec<Received> {
        self.received
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    #[must_use]
    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }
}

fn token_ok(jwt: &str, key: &p256::ecdsa::VerifyingKey, team: &str, kid: &str) -> bool {
    let mut parts = jwt.split('.');
    let (Some(header), Some(claims), Some(sig), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let json = |s: &str| -> Option<Value> { serde_json::from_slice(&b64.decode(s).ok()?).ok() };
    let (Some(h), Some(c)) = (json(header), json(claims)) else {
        return false;
    };
    let Some(signature) = b64
        .decode(sig)
        .ok()
        .and_then(|s| p256::ecdsa::Signature::from_slice(&s).ok())
    else {
        return false;
    };
    h["alg"] == "ES256"
        && h["kid"] == kid
        && c["iss"] == team
        && c["iat"].is_u64()
        && key
            .verify(format!("{header}.{claims}").as_bytes(), &signature)
            .is_ok()
}
