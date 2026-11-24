//! The handoff to APNs: HTTP/2, authenticated with a provider token (an
//! ES256 JWT signed with the team's `.p8` key), one connection per
//! environment kept open and made again when it fails.
//!
//! APNs' answer decides what the host does with a notification: sent; try
//! again later (throttled, APNs unavailable, no answer in time); or never
//! (the device token is not valid any more, or the push itself is wrong).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use base64::Engine as _;
use bytes::Bytes;
use http_body_util::{BodyExt as _, Full};
use hyper::client::conn::http2::SendRequest;
use hyper_util::rt::{TokioExecutor, TokioIo};
use p256::ecdsa::signature::Signer as _;
use p256::pkcs8::DecodePrivateKey as _;
use serde_json::{Value, json};

/// Apple's endpoints.
pub const SANDBOX: &str = "https://api.sandbox.push.apple.com";
pub const PRODUCTION: &str = "https://api.push.apple.com";
/// A provider token is made again after this (APNs takes one for an hour,
/// and refuses one refreshed more often than every 20 minutes).
const TOKEN_LIFE_SECS: u64 = 40 * 60;
/// APNs' limit for a notification's JSON.
pub const MAX_PAYLOAD: usize = 4096;

/// Which APNs environment a device token belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Env {
    Sandbox,
    Production,
}

impl Env {
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "sandbox" => Some(Self::Sandbox),
            "production" => Some(Self::Production),
            _ => None,
        }
    }
}

/// One push as a host asks for it (see push-gateway.md), checked.
#[derive(Debug, Clone, PartialEq)]
pub struct PushRequest {
    /// Hex.
    pub device_token: String,
    pub env: Env,
    pub topic: String,
    /// `alert` or `liveactivity`.
    pub push_type: String,
    pub collapse_id: String,
    pub priority: u8,
    pub payload: Value,
}

impl PushRequest {
    /// The request in `v`, or why it is not one.
    ///
    /// # Errors
    /// A field missing or out of bounds, in words for the host's log.
    pub fn from_json(v: &Value) -> Result<Self, String> {
        let s = |k: &str| {
            v.get(k)
                .and_then(Value::as_str)
                .ok_or(format!("missing {k}"))
        };
        let device_token = s("device_token")?.to_ascii_lowercase();
        if device_token.is_empty()
            || device_token.len() > 512
            || !device_token.chars().all(|c| c.is_ascii_hexdigit())
        {
            return Err("bad device_token".into());
        }
        let env = Env::from_name(s("env")?).ok_or("bad env")?;
        let topic = s("topic")?.to_owned();
        if topic.is_empty()
            || topic.len() > 255
            || !topic
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || ".-".contains(c))
        {
            return Err("bad topic".into());
        }
        let push_type = s("push_type")?.to_owned();
        if !matches!(push_type.as_str(), "alert" | "liveactivity") {
            return Err("bad push_type".into());
        }
        let collapse_id = s("collapse_id")?.to_owned();
        if collapse_id.len() > 64 || collapse_id.chars().any(char::is_control) {
            return Err("bad collapse_id".into());
        }
        let priority = match v.get("priority").and_then(Value::as_u64) {
            Some(5) => 5,
            Some(10) | None => 10,
            Some(_) => return Err("bad priority".into()),
        };
        let payload = v.get("payload").cloned().ok_or("missing payload")?;
        if !payload.is_object() || payload.to_string().len() > MAX_PAYLOAD {
            return Err("bad payload".into());
        }
        Ok(Self {
            device_token,
            env,
            topic,
            push_type,
            collapse_id,
            priority,
            payload,
        })
    }
}

/// What APNs made of a push.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Sent,
    /// Try again later, with APNs' reason.
    Temporary(String),
    /// Never to this device token: it is gone or never was this app's. The
    /// host keeps the notification for a new token the phone may tell it.
    BadToken(String),
    /// Never: the push itself is not one APNs takes.
    Permanent(String),
}

/// The outcome of an APNs answer: its status and its `reason`.
#[must_use]
pub fn outcome(status: u16, reason: &str) -> Outcome {
    match (status, reason) {
        (200, _) => Outcome::Sent,
        // The device token is gone or never was this app's.
        (410, _) | (400, "BadDeviceToken" | "DeviceTokenNotForTopic" | "Unregistered") => {
            Outcome::BadToken(reason_or(reason, "Unregistered"))
        }
        // Our provider token, or APNs itself: fixed on this side or by
        // waiting, never by dropping the notification.
        (403, _) | (429, _) | (500..=599, _) => {
            Outcome::Temporary(reason_or(reason, "Unavailable"))
        }
        // Anything else wrong with the push itself (too large, bad topic or
        // collapse id): sending it again changes nothing.
        (_, _) => Outcome::Permanent(reason_or(reason, "BadRequest")),
    }
}

fn reason_or(reason: &str, default: &str) -> String {
    if reason.is_empty() { default } else { reason }.to_owned()
}

/// Where APNs (or a test's stand-in) answers: `https://host[:port]`, or
/// `http://host:port` for HTTP/2 without TLS (prior knowledge), which only
/// a test's upstream speaks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Upstream {
    /// Only [`Upstream::plain_loopback`] (a test build's) makes one without
    /// TLS: the provider token and the device tokens never go in clear.
    tls: bool,
    pub host: String,
    pub port: u16,
}

impl Upstream {
    /// An APNs endpoint as the config names it: `https://` only.
    ///
    /// # Errors
    /// Not an `https://` URL with a host (an `http://` one included: it
    /// would carry the provider token and the device tokens in clear).
    pub fn parse(url: &str) -> Result<Self, String> {
        let Some(rest) = url.strip_prefix("https://") else {
            return Err(format!("not an https URL: {url}"));
        };
        Self::authority_of(url, rest, true)
    }

    /// A test's stand-in for APNs: `http://` (HTTP/2 without TLS) to this
    /// machine only. Exists only with the `fake-apns` feature, which no
    /// gateway that is run is built with.
    ///
    /// # Errors
    /// Not an `http://` URL to a loopback address.
    #[cfg(feature = "fake-apns")]
    pub fn plain_loopback(url: &str) -> Result<Self, String> {
        let Some(rest) = url.strip_prefix("http://") else {
            return Err(format!("not an http URL: {url}"));
        };
        let up = Self::authority_of(url, rest, false)?;
        let loopback = up.host == "localhost"
            || up
                .host
                .trim_start_matches('[')
                .trim_end_matches(']')
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback());
        if !loopback {
            return Err(format!("a plain upstream must be on this machine: {url}"));
        }
        Ok(up)
    }

    fn authority_of(url: &str, rest: &str, tls: bool) -> Result<Self, String> {
        let authority = rest.trim_end_matches('/');
        let (host, port) = match authority.rsplit_once(':') {
            Some((h, p)) => (h, p.parse().map_err(|_| format!("bad port in {url}"))?),
            None => (authority, if tls { 443 } else { 80 }),
        };
        if host.is_empty() || host.contains('/') {
            return Err(format!("no host in {url}"));
        }
        Ok(Self {
            tls,
            host: host.to_owned(),
            port,
        })
    }

    fn authority(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

/// The team's APNs key and where to reach APNs.
pub struct ApnsConfig {
    pub team_id: String,
    pub key_id: String,
    /// The `.p8` file's text (a PKCS#8 P-256 key).
    pub key_pem: String,
    pub sandbox: Upstream,
    pub production: Upstream,
    /// The longest one push may take, connecting included.
    pub timeout: Duration,
}

/// Sends pushes to APNs.
pub struct Apns {
    team_id: String,
    key_id: String,
    key: p256::ecdsa::SigningKey,
    upstreams: HashMap<Env, Upstream>,
    timeout: Duration,
    tls: tokio_rustls::TlsConnector,
    token: Mutex<Option<(String, u64)>>,
    conns: tokio::sync::Mutex<HashMap<Env, SendRequest<Full<Bytes>>>>,
}

impl Apns {
    /// # Errors
    /// The key is not a P-256 PKCS#8 key.
    pub fn new(config: ApnsConfig) -> Result<Self, String> {
        let key = p256::ecdsa::SigningKey::from_pkcs8_pem(&config.key_pem)
            .map_err(|_| "the APNs key is not a .p8 (PKCS#8 P-256) key".to_owned())?;
        let roots =
            rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let mut tls = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .with_root_certificates(roots)
        .with_no_client_auth();
        tls.alpn_protocols = vec![b"h2".to_vec()];
        Ok(Self {
            team_id: config.team_id,
            key_id: config.key_id,
            key,
            upstreams: HashMap::from([
                (Env::Sandbox, config.sandbox),
                (Env::Production, config.production),
            ]),
            timeout: config.timeout,
            tls: tokio_rustls::TlsConnector::from(Arc::new(tls)),
            token: Mutex::new(None),
            conns: tokio::sync::Mutex::new(HashMap::new()),
        })
    }

    /// The provider token for `now` (seconds), made again when old.
    fn token(&self, now: u64) -> String {
        let mut token = self.token.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some((jwt, at)) = token.as_ref()
            && now.saturating_sub(*at) < TOKEN_LIFE_SECS
        {
            return jwt.clone();
        }
        let b64 = |v: &Value| {
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v.to_string().as_bytes())
        };
        let message = format!(
            "{}.{}",
            b64(&json!({ "alg": "ES256", "kid": self.key_id })),
            b64(&json!({ "iss": self.team_id, "iat": now }))
        );
        let signature: p256::ecdsa::Signature = self.key.sign(message.as_bytes());
        let jwt = format!(
            "{message}.{}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(signature.to_bytes())
        );
        *token = Some((jwt.clone(), now));
        jwt
    }

    fn forget_token(&self) {
        *self.token.lock().unwrap_or_else(PoisonError::into_inner) = None;
    }

    async fn connect(&self, upstream: &Upstream) -> Result<SendRequest<Full<Bytes>>, String> {
        let tcp = tokio::net::TcpStream::connect(upstream.authority())
            .await
            .map_err(|e| format!("connect: {e}"))?;
        let _ = tcp.set_nodelay(true);
        if upstream.tls {
            let name = rustls::pki_types::ServerName::try_from(upstream.host.clone())
                .map_err(|e| e.to_string())?;
            let stream = self
                .tls
                .connect(name, tcp)
                .await
                .map_err(|e| format!("tls: {e}"))?;
            let (send, conn) =
                hyper::client::conn::http2::handshake(TokioExecutor::new(), TokioIo::new(stream))
                    .await
                    .map_err(|e| format!("h2: {e}"))?;
            tokio::spawn(conn);
            Ok(send)
        } else {
            let (send, conn) =
                hyper::client::conn::http2::handshake(TokioExecutor::new(), TokioIo::new(tcp))
                    .await
                    .map_err(|e| format!("h2: {e}"))?;
            tokio::spawn(conn);
            Ok(send)
        }
    }

    /// Hands `push` to APNs at `now` (seconds since 1970).
    pub async fn send(&self, push: &PushRequest, now: u64) -> Outcome {
        match tokio::time::timeout(self.timeout, self.try_send(push, now)).await {
            Ok(outcome) => outcome,
            Err(_) => {
                // The connection may be stuck: a new one next time.
                self.conns.lock().await.remove(&push.env);
                Outcome::Temporary("Timeout".into())
            }
        }
    }

    async fn try_send(&self, push: &PushRequest, now: u64) -> Outcome {
        let Some(upstream) = self.upstreams.get(&push.env) else {
            return Outcome::Permanent("BadEnvironment".into());
        };
        let mut sender = {
            let mut conns = self.conns.lock().await;
            match conns.get(&push.env).filter(|s| !s.is_closed()) {
                Some(s) => s.clone(),
                None => match self.connect(upstream).await {
                    Ok(s) => {
                        conns.insert(push.env, s.clone());
                        s
                    }
                    Err(why) => return Outcome::Temporary(why),
                },
            }
        };
        if sender.ready().await.is_err() {
            self.conns.lock().await.remove(&push.env);
            return Outcome::Temporary("ConnectionClosed".into());
        }
        let mut request = hyper::Request::post(format!(
            "{}://{}/3/device/{}",
            if upstream.tls { "https" } else { "http" },
            upstream.authority(),
            push.device_token
        ))
        .header("authorization", format!("bearer {}", self.token(now)))
        .header("apns-topic", &push.topic)
        .header("apns-push-type", &push.push_type)
        .header("apns-priority", push.priority.to_string())
        .header("content-type", "application/json");
        if !push.collapse_id.is_empty() {
            request = request.header("apns-collapse-id", &push.collapse_id);
        }
        let Ok(request) = request.body(Full::new(Bytes::from(push.payload.to_string()))) else {
            return Outcome::Permanent("BadRequest".into());
        };
        let response = match sender.send_request(request).await {
            Ok(r) => r,
            Err(e) => {
                self.conns.lock().await.remove(&push.env);
                return Outcome::Temporary(format!("send: {e}"));
            }
        };
        let status = response.status().as_u16();
        let body = response
            .into_body()
            .collect()
            .await
            .map(|b| b.to_bytes())
            .unwrap_or_default();
        let reason = serde_json::from_slice::<Value>(&body)
            .ok()
            .and_then(|v| v.get("reason").and_then(Value::as_str).map(str::to_owned))
            .unwrap_or_default();
        if status == 403
            && matches!(
                reason.as_str(),
                "ExpiredProviderToken" | "InvalidProviderToken"
            )
        {
            self.forget_token();
        }
        outcome(status, &reason)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn apns_answers_map_to_what_the_host_does() {
        assert_eq!(outcome(200, ""), Outcome::Sent);
        // The token is what is wrong: the push may still go to a new one.
        assert_eq!(
            outcome(410, "Unregistered"),
            Outcome::BadToken("Unregistered".into())
        );
        assert_eq!(
            outcome(400, "BadDeviceToken"),
            Outcome::BadToken("BadDeviceToken".into())
        );
        assert_eq!(
            outcome(400, "PayloadTooLarge"),
            Outcome::Permanent("PayloadTooLarge".into())
        );
        assert_eq!(
            outcome(429, "TooManyRequests"),
            Outcome::Temporary("TooManyRequests".into())
        );
        assert_eq!(outcome(503, ""), Outcome::Temporary("Unavailable".into()));
        assert_eq!(
            outcome(403, "ExpiredProviderToken"),
            Outcome::Temporary("ExpiredProviderToken".into())
        );
    }

    #[test]
    fn upstreams_parse_with_their_ports() {
        assert_eq!(
            Upstream::parse(PRODUCTION).unwrap(),
            Upstream {
                tls: true,
                host: "api.push.apple.com".into(),
                port: 443
            }
        );
        assert_eq!(Upstream::parse("https://127.0.0.1:9/").unwrap().port, 9);
        assert!(Upstream::parse("ftp://x").is_err());
    }

    /// The provider token and device tokens never go to APNs in clear: a
    /// config naming an `http://` endpoint is refused, whatever the build.
    #[test]
    fn a_plain_http_upstream_is_refused() {
        assert!(Upstream::parse("http://api.push.apple.com").is_err());
        assert!(Upstream::parse("http://127.0.0.1:9").is_err());
    }

    #[cfg(feature = "fake-apns")]
    #[test]
    fn a_tests_plain_upstream_stays_on_this_machine() {
        assert_eq!(
            Upstream::plain_loopback("http://127.0.0.1:9").unwrap().port,
            9
        );
        assert!(Upstream::plain_loopback("http://[::1]:9").is_ok());
        assert!(Upstream::plain_loopback("http://api.push.apple.com:80").is_err());
        assert!(Upstream::plain_loopback("http://10.0.0.5:80").is_err());
    }
}
