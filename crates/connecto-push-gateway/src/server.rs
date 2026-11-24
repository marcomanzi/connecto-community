//! The gateway's one endpoint, `POST /v1/push` (push-gateway.md): a host
//! enrolled in the config, proven by its bearer token and its device key's
//! signature, hands one push over; the gateway checks it, holds the host to
//! its rate, and hands it to APNs. Nothing of a push's content is logged:
//! only the host, the outcome, and APNs' reason.
//!
//! Answers: `200` sent; `503` (or `429`, over the rate) try again later;
//! `410` never to this device token (APNs refused the token: the host keeps
//! the notification for a new one); `400` never (a request that is not one,
//! or a push APNs refused as such); `401` not a host the gateway knows, or
//! not signed by it.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt as _, Full, Limited};
use hyper::body::Incoming;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::{Value, json};
use sha2::Digest as _;
use ssh_key::PublicKey;

use crate::apns::{Apns, ApnsConfig, Outcome, PushRequest, Upstream};
use crate::auth;

/// The path pushes are posted to.
pub const PUSH_PATH: &str = "/v1/push";
/// The most bytes of a request body: a push is at most 4 KB of payload.
const MAX_BODY: usize = 16 * 1024;

/// A host the gateway serves.
#[derive(Clone)]
pub struct HostEntry {
    pub name: String,
    /// [`auth::token_digest`] of its bearer token.
    pub token_digest: String,
    /// Its device key (Ed25519).
    pub key: PublicKey,
}

/// How many pushes a host may send: `burst` at once, refilled at
/// `per_minute`.
#[derive(Debug, Clone, Copy)]
pub struct Rate {
    pub per_minute: u32,
    pub burst: u32,
}

/// The gateway's settings, from its JSON config (see push-gateway.md).
pub struct Config {
    pub listen: SocketAddr,
    pub hosts: Vec<HostEntry>,
    pub rate: Rate,
    /// The bundle ids pushes may be for; empty: any.
    pub topics: Vec<String>,
    pub apns: ApnsConfig,
}

impl Config {
    /// The config in `v`; the `.p8` key's path is relative to `dir`.
    ///
    /// # Errors
    /// What is missing or wrong, for the person setting it up.
    pub fn from_json(v: &Value, dir: &Path) -> Result<Self, String> {
        let s = |v: &Value, k: &str| {
            v.get(k)
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| format!("config: missing \"{k}\""))
        };
        let listen = s(v, "listen")?
            .parse()
            .map_err(|_| "config: \"listen\" is not an address:port".to_owned())?;
        let mut hosts = Vec::new();
        for h in v
            .get("hosts")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let name = s(h, "name")?;
            let token_digest = s(h, "token_sha256")?.to_ascii_lowercase();
            if token_digest.len() != 64 || !token_digest.chars().all(|c| c.is_ascii_hexdigit()) {
                return Err(format!(
                    "config: host {name}: token_sha256 is not a SHA-256"
                ));
            }
            let key = PublicKey::from_openssh(&s(h, "public_key")?)
                .map_err(|_| format!("config: host {name}: public_key is not an OpenSSH key"))?;
            if key.algorithm() != ssh_key::Algorithm::Ed25519 {
                return Err(format!("config: host {name}: public_key is not Ed25519"));
            }
            hosts.push(HostEntry {
                name,
                token_digest,
                key,
            });
        }
        let rate = v.get("rate");
        let n = |k: &str, default: u32| {
            rate.and_then(|r| r.get(k))
                .and_then(Value::as_u64)
                .and_then(|n| u32::try_from(n).ok())
                .filter(|n| *n > 0)
                .unwrap_or(default)
        };
        let apns = v.get("apns").ok_or("config: missing \"apns\"")?;
        let key_path = dir.join(s(apns, "key_file")?);
        let key_pem = std::fs::read_to_string(&key_path)
            .map_err(|e| format!("config: {}: {e}", key_path.display()))?;
        let upstream = |k: &str, default: &str| {
            Upstream::parse(apns.get(k).and_then(Value::as_str).unwrap_or(default))
        };
        Ok(Self {
            listen,
            hosts,
            rate: Rate {
                per_minute: n("per_minute", 120),
                burst: n("burst", 20),
            },
            topics: v
                .get("topics")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|t| t.as_str().map(str::to_owned))
                .collect(),
            apns: ApnsConfig {
                team_id: s(apns, "team_id")?,
                key_id: s(apns, "key_id")?,
                key_pem,
                sandbox: upstream("sandbox_url", crate::apns::SANDBOX)?,
                production: upstream("production_url", crate::apns::PRODUCTION)?,
                timeout: Duration::from_secs(10),
            },
        })
    }
}

/// What a request's headers say of who sent it.
struct Sent {
    bearer: Option<String>,
    host: Option<String>,
    time: Option<String>,
    nonce: Option<String>,
    signature: Option<String>,
}

struct Bucket {
    tokens: f64,
    at_ms: u64,
}

type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

/// The gateway: its hosts, APNs, and what it keeps between requests (each
/// host's rate, the signatures it has seen).
pub struct Gateway {
    hosts: Vec<HostEntry>,
    rate: Rate,
    topics: Vec<String>,
    apns: Apns,
    /// Milliseconds since 1970; tests set their own.
    clock: Clock,
    buckets: Mutex<HashMap<String, Bucket>>,
    /// The nonces of the requests each host had taken (by host, then by the
    /// nonce's hash, with its time): each is taken once.
    seen: Mutex<HashMap<String, HashMap<[u8; 32], u64>>>,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

fn answer(status: StatusCode, body: &Value) -> Response<Full<Bytes>> {
    let mut response = Response::new(Full::new(Bytes::from(body.to_string())));
    *response.status_mut() = status;
    response.headers_mut().insert(
        "content-type",
        hyper::header::HeaderValue::from_static("application/json"),
    );
    response
}

fn refuse(status: StatusCode, reason: &str) -> Response<Full<Bytes>> {
    answer(status, &json!({ "reason": reason }))
}

impl Gateway {
    /// # Errors
    /// The APNs key cannot be read.
    pub fn new(config: Config) -> Result<Self, String> {
        Self::with_clock(config, Arc::new(now_ms))
    }

    /// As [`Self::new`], its time (ms since 1970) from `clock`.
    ///
    /// # Errors
    /// The APNs key cannot be read.
    pub fn with_clock(config: Config, clock: Clock) -> Result<Self, String> {
        Ok(Self {
            hosts: config.hosts,
            rate: config.rate,
            topics: config.topics,
            apns: Apns::new(config.apns)?,
            clock,
            buckets: Mutex::new(HashMap::new()),
            seen: Mutex::new(HashMap::new()),
        })
    }

    /// Serves `listener` until the task is dropped.
    pub async fn serve(self: Arc<Self>, listener: tokio::net::TcpListener) {
        loop {
            let Ok((tcp, _)) = listener.accept().await else {
                continue;
            };
            let gateway = Arc::clone(&self);
            tokio::spawn(async move {
                let service = hyper::service::service_fn(move |req| {
                    let gateway = Arc::clone(&gateway);
                    async move { Ok::<_, std::convert::Infallible>(gateway.handle(req).await) }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(tcp), service)
                    .await;
            });
        }
    }

    /// One request.
    pub async fn handle(&self, req: Request<Incoming>) -> Response<Full<Bytes>> {
        if req.uri().path() != PUSH_PATH {
            return refuse(StatusCode::NOT_FOUND, "NotFound");
        }
        if req.method() != hyper::Method::POST {
            return refuse(StatusCode::METHOD_NOT_ALLOWED, "MethodNotAllowed");
        }
        let header = |k: &str| {
            req.headers()
                .get(k)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned)
        };
        let sent = Sent {
            bearer: header("authorization"),
            host: header(auth::HOST_HEADER),
            time: header(auth::TIME_HEADER),
            nonce: header(auth::NONCE_HEADER),
            signature: header(auth::SIGNATURE_HEADER),
        };
        let host = sent.host.clone();
        let Ok(body) = Limited::new(req.into_body(), MAX_BODY).collect().await else {
            return refuse(StatusCode::PAYLOAD_TOO_LARGE, "TooLarge");
        };
        let body = body.to_bytes();
        let (status, reason) = match self.push(sent, &body).await {
            Ok(()) => (StatusCode::OK, String::new()),
            Err((status, reason)) => (status, reason),
        };
        // The host and the outcome only: never a token or a payload.
        eprintln!(
            "push: {} {}{}",
            host.as_deref().unwrap_or("-"),
            status.as_u16(),
            if reason.is_empty() {
                String::new()
            } else {
                format!(" {reason}")
            }
        );
        if status == StatusCode::OK {
            answer(status, &json!({ "sent": true }))
        } else {
            refuse(status, &reason)
        }
    }

    async fn push(&self, sent: Sent, body: &[u8]) -> Result<(), (StatusCode, String)> {
        let unauthorized = |why: &str| (StatusCode::UNAUTHORIZED, why.to_owned());
        let Sent {
            bearer,
            host,
            time,
            nonce,
            signature,
        } = sent;
        let token = bearer
            .as_deref()
            .and_then(|b| b.strip_prefix("Bearer "))
            .ok_or_else(|| unauthorized("NoToken"))?;
        let digest = auth::token_digest(token);
        let entry = self
            .hosts
            .iter()
            .find(|h| h.token_digest == digest && host.as_deref() == Some(h.name.as_str()))
            .ok_or_else(|| unauthorized("UnknownHost"))?;
        let now_ms = (self.clock)();
        let now = now_ms / 1000;
        let time: u64 = time
            .and_then(|t| t.parse().ok())
            .ok_or_else(|| unauthorized("NoTime"))?;
        if now.abs_diff(time) > auth::WINDOW_SECS {
            return Err(unauthorized("StaleTime"));
        }
        let signature = signature.ok_or_else(|| unauthorized("NoSignature"))?;
        let nonce = nonce.ok_or_else(|| unauthorized("NoNonce"))?;
        let signed = auth::Signed {
            method: "POST",
            path: PUSH_PATH,
            time,
            nonce: &nonce,
            body,
        };
        if !auth::verify(&entry.key, &signed, &signature) {
            return Err(unauthorized("BadSignature"));
        }
        // Each signed request is taken once: one seen on the way and sent
        // again is refused. Only a request that passed the signature, is no
        // replay and is within the host's rate is remembered, so what is
        // kept is bounded by the rate (see `replay_cap`), per host, and a
        // flood over it costs no memory.
        let seen: [u8; 32] = sha2::Sha256::digest(nonce.as_bytes()).into();
        {
            let mut all = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
            let mine = all.entry(entry.name.clone()).or_default();
            mine.retain(|_, at| now.abs_diff(*at) <= 2 * auth::WINDOW_SECS);
            if mine.contains_key(&seen) {
                return Err(unauthorized("Replayed"));
            }
            if !self.take_token(&entry.name, now_ms) {
                return Err((StatusCode::TOO_MANY_REQUESTS, "TooManyRequests".into()));
            }
            // The rate bounds it already; the cap holds whatever the clock does.
            let cap = self.replay_cap();
            while mine.len() >= cap {
                let Some(oldest) = mine.iter().min_by_key(|(_, at)| **at).map(|(k, _)| *k) else {
                    break;
                };
                mine.remove(&oldest);
            }
            mine.insert(seen, time);
        }
        let v: Value = serde_json::from_slice(body)
            .map_err(|_| (StatusCode::BAD_REQUEST, "NotJson".to_owned()))?;
        let push = PushRequest::from_json(&v).map_err(|why| (StatusCode::BAD_REQUEST, why))?;
        let base = push
            .topic
            .strip_suffix(".push-type.liveactivity")
            .unwrap_or(&push.topic);
        if !self.topics.is_empty() && !self.topics.iter().any(|t| t == base) {
            return Err((StatusCode::BAD_REQUEST, "TopicNotAllowed".into()));
        }
        match self.apns.send(&push, now).await {
            Outcome::Sent => Ok(()),
            Outcome::Temporary(why) => Err((StatusCode::SERVICE_UNAVAILABLE, why)),
            // The device token is gone: the host keeps the notification for
            // a token the phone tells it later.
            Outcome::BadToken(why) => Err((StatusCode::GONE, why)),
            // The push itself: the host drops it.
            Outcome::Permanent(why) => Err((StatusCode::BAD_REQUEST, why)),
        }
    }

    /// The most nonces kept for one host: as many requests as its rate lets
    /// through in the time a nonce is kept (twice the signature window),
    /// plus its burst.
    fn replay_cap(&self) -> usize {
        let window_minutes = (2 * auth::WINDOW_SECS).div_ceil(60);
        let per_window = u64::from(self.rate.per_minute).saturating_mul(window_minutes);
        usize::try_from(per_window.saturating_add(u64::from(self.rate.burst))).unwrap_or(usize::MAX)
    }

    /// How many nonces are kept now, over every host: for tests that the
    /// replay guard's memory stays bounded.
    #[doc(hidden)]
    #[must_use]
    pub fn replay_entries(&self) -> usize {
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .map(HashMap::len)
            .sum()
    }

    /// One push from `host`'s bucket; false when it is empty.
    fn take_token(&self, host: &str, now_ms: u64) -> bool {
        let mut buckets = self.buckets.lock().unwrap_or_else(PoisonError::into_inner);
        let burst = f64::from(self.rate.burst);
        let bucket = buckets.entry(host.to_owned()).or_insert(Bucket {
            tokens: burst,
            at_ms: now_ms,
        });
        #[allow(clippy::cast_precision_loss)]
        let elapsed = now_ms.saturating_sub(bucket.at_ms) as f64;
        bucket.tokens =
            (bucket.tokens + elapsed * f64::from(self.rate.per_minute) / 60_000.0).min(burst);
        bucket.at_ms = now_ms;
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}
