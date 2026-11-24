//! The gateway against a fake APNs (HTTP/2): what it hands over, how APNs'
//! answers come back to the host, and the requests it refuses.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use bytes::Bytes;
use connecto_push_gateway::apns::{ApnsConfig, Upstream};
use connecto_push_gateway::auth;
use connecto_push_gateway::fake_apns::{FakeApns, test_key};
use connecto_push_gateway::server::{Config, Gateway, HostEntry, PUSH_PATH, Rate};
use http_body_util::{BodyExt as _, Full};
use hyper_util::rt::TokioIo;
use serde_json::{Value, json};
use ssh_key::rand_core::OsRng;
use ssh_key::{Algorithm, HashAlg, PrivateKey};

const TOKEN: &str = "host-token-1";
const TEAM: &str = "TEAM123456";
const KID: &str = "KEY1234567";

struct Rig {
    apns: FakeApns,
    gateway: std::net::SocketAddr,
    key: PrivateKey,
    clock: Arc<AtomicU64>,
    /// The gateway itself, for what it keeps.
    served: Arc<Gateway>,
}

async fn rig(rate: Rate) -> Rig {
    let (pem, verifying) = test_key().unwrap();
    let apns = FakeApns::start(verifying, TEAM, KID).await.unwrap();
    let key = PrivateKey::random(&mut OsRng, Algorithm::Ed25519).unwrap();
    let upstream = Upstream::plain_loopback(&apns.url()).unwrap();
    let config = Config {
        listen: "127.0.0.1:0".parse().unwrap(),
        hosts: vec![HostEntry {
            name: "studio-mac".into(),
            token_digest: auth::token_digest(TOKEN),
            key: key.public_key().clone(),
        }],
        rate,
        topics: vec!["studio.connecto.connectoFlutterApp".into()],
        apns: ApnsConfig {
            team_id: TEAM.into(),
            key_id: KID.into(),
            key_pem: pem,
            sandbox: upstream.clone(),
            production: upstream,
            timeout: Duration::from_secs(2),
        },
    };
    let clock = Arc::new(AtomicU64::new(1_800_000_000_000));
    let c = Arc::clone(&clock);
    let gateway =
        Arc::new(Gateway::with_clock(config, Arc::new(move || c.load(Ordering::SeqCst))).unwrap());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(Arc::clone(&gateway).serve(listener));
    Rig {
        apns,
        gateway: addr,
        key,
        clock,
        served: gateway,
    }
}

fn push_body(token: &str) -> Vec<u8> {
    json!({
        "device_token": token,
        "env": "sandbox",
        "topic": "studio.connecto.connectoFlutterApp",
        "push_type": "alert",
        "collapse_id": "c0ffee",
        "priority": 10,
        "payload": { "aps": { "alert": { "title": "Connecto · p", "body": "La risposta è pronta" } }, "r": "req-1" },
    })
    .to_string()
    .into_bytes()
}

impl Rig {
    fn now(&self) -> u64 {
        self.clock.load(Ordering::SeqCst) / 1000
    }

    /// The headers a host sends for `body` at `time`, with a new nonce.
    fn signed(&self, body: &[u8], time: u64) -> Vec<(&'static str, String)> {
        static NONCES: AtomicU64 = AtomicU64::new(0);
        let nonce = format!("nonce-{:016}", NONCES.fetch_add(1, Ordering::SeqCst));
        let sig = self
            .key
            .sign(
                auth::NAMESPACE,
                HashAlg::Sha512,
                &auth::signed_message("POST", PUSH_PATH, time, &nonce, body),
            )
            .unwrap();
        vec![
            ("authorization", format!("Bearer {TOKEN}")),
            (auth::HOST_HEADER, "studio-mac".into()),
            (auth::TIME_HEADER, time.to_string()),
            (
                auth::SIGNATURE_HEADER,
                auth::encode_signature(&sig).unwrap(),
            ),
            (auth::NONCE_HEADER, nonce),
        ]
    }

    async fn post(&self, headers: &[(&'static str, String)], body: &[u8]) -> (u16, Value) {
        let tcp = tokio::net::TcpStream::connect(self.gateway).await.unwrap();
        let (mut send, conn) = hyper::client::conn::http1::handshake(TokioIo::new(tcp))
            .await
            .unwrap();
        tokio::spawn(conn);
        let mut request = hyper::Request::post(format!("http://{}{PUSH_PATH}", self.gateway))
            .header("host", self.gateway.to_string())
            .header("content-type", "application/json");
        for (k, v) in headers {
            request = request.header(*k, v);
        }
        let response = send
            .send_request(request.body(Full::new(Bytes::from(body.to_vec()))).unwrap())
            .await
            .unwrap();
        let status = response.status().as_u16();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    async fn push(&self, token: &str) -> (u16, Value) {
        let body = push_body(token);
        let headers = self.signed(&body, self.now());
        self.post(&headers, &body).await
    }
}

const RATE: Rate = Rate {
    per_minute: 600,
    burst: 50,
};

#[tokio::test]
async fn a_signed_push_reaches_apns_as_apple_expects_it() {
    let rig = rig(RATE).await;
    let (status, body) = rig.push("ab12cd").await;
    assert_eq!(status, 200, "{body}");
    let got = rig.apns.received();
    assert_eq!(got.len(), 1);
    let r = &got[0];
    assert_eq!(r.path, "/3/device/ab12cd");
    assert!(
        r.token_ok,
        "the provider token is not a valid ES256 JWT of the team's key"
    );
    assert_eq!(
        r.headers["apns-topic"],
        "studio.connecto.connectoFlutterApp"
    );
    assert_eq!(r.headers["apns-push-type"], "alert");
    assert_eq!(r.headers["apns-priority"], "10");
    assert_eq!(r.headers["apns-collapse-id"], "c0ffee");
    assert_eq!(r.body["r"], "req-1");
}

#[tokio::test]
async fn apns_unavailable_is_a_retry_and_then_it_goes() {
    let rig = rig(RATE).await;
    rig.apns
        .answer(&[(503, "ServiceUnavailable"), (429, "TooManyRequests")]);
    assert_eq!(rig.push("ab12").await.0, 503);
    assert_eq!(
        rig.push("ab12").await.0,
        503,
        "APNs throttling is a retry too"
    );
    assert_eq!(rig.push("ab12").await.0, 200);
    assert_eq!(rig.apns.received().len(), 3);
}

#[tokio::test]
async fn a_token_apns_rejects_is_gone_for_good() {
    let rig = rig(RATE).await;
    rig.apns.answer(&[
        (410, "Unregistered"),
        (400, "BadDeviceToken"),
        (400, "PayloadTooLarge"),
    ]);
    let (status, body) = rig.push("ab12").await;
    assert_eq!(
        (status, body["reason"].as_str()),
        (410, Some("Unregistered"))
    );
    let (status, body) = rig.push("ab12").await;
    assert_eq!(
        (status, body["reason"].as_str()),
        (410, Some("BadDeviceToken"))
    );
    // The push itself is wrong, not the token: told apart, as the host
    // keeps a notification for a new token but drops a bad push.
    let (status, body) = rig.push("ab12").await;
    assert_eq!(
        (status, body["reason"].as_str()),
        (400, Some("PayloadTooLarge"))
    );
}

#[tokio::test]
async fn an_unreachable_apns_is_a_retry() {
    // A port nothing answers on.
    let rig = rig(RATE).await;
    let dead = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dead_addr = dead.local_addr().unwrap();
    drop(dead);
    let (pem, _) = test_key().unwrap();
    let upstream = Upstream::plain_loopback(&format!("http://{dead_addr}")).unwrap();
    let gateway = Arc::new(
        Gateway::new(Config {
            listen: "127.0.0.1:0".parse().unwrap(),
            hosts: vec![HostEntry {
                name: "studio-mac".into(),
                token_digest: auth::token_digest(TOKEN),
                key: rig.key.public_key().clone(),
            }],
            rate: RATE,
            topics: Vec::new(),
            apns: ApnsConfig {
                team_id: TEAM.into(),
                key_id: KID.into(),
                key_pem: pem,
                sandbox: upstream.clone(),
                production: upstream,
                timeout: Duration::from_secs(2),
            },
        })
        .unwrap(),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(Arc::clone(&gateway).serve(listener));
    let body = push_body("ab12");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let dead_rig = Rig {
        apns: rig.apns,
        gateway: addr,
        key: rig.key,
        clock: rig.clock,
        served: gateway,
    };
    let (status, _) = dead_rig.post(&dead_rig.signed(&body, now), &body).await;
    assert_eq!(status, 503);
}

#[tokio::test]
async fn unsigned_forged_stale_or_replayed_requests_never_reach_apns() {
    let rig = rig(RATE).await;
    let body = push_body("ab12");
    let now = rig.now();
    let good = rig.signed(&body, now);
    // No signature; a wrong token; another key's signature; the body changed.
    let mut unsigned = good.clone();
    unsigned.retain(|(k, _)| *k != auth::SIGNATURE_HEADER);
    assert_eq!(rig.post(&unsigned, &body).await.0, 401);
    let mut wrong_token = good.clone();
    wrong_token[0].1 = "Bearer nope".into();
    assert_eq!(rig.post(&wrong_token, &body).await.0, 401);
    let forger = PrivateKey::random(&mut OsRng, Algorithm::Ed25519).unwrap();
    let forged_sig = forger
        .sign(
            auth::NAMESPACE,
            HashAlg::Sha512,
            &auth::signed_message("POST", PUSH_PATH, now, &good[4].1, &body),
        )
        .unwrap();
    let mut forged = good.clone();
    forged[3].1 = auth::encode_signature(&forged_sig).unwrap();
    assert_eq!(rig.post(&forged, &body).await.0, 401);
    assert_eq!(
        rig.post(&good, &push_body("ffff")).await.0,
        401,
        "a changed body passed"
    );
    // Out of the window.
    let stale = rig.signed(&body, now - auth::WINDOW_SECS - 1);
    assert_eq!(rig.post(&stale, &body).await.1["reason"], "StaleTime");
    assert!(rig.apns.received().is_empty());
    // Signed right: once. The same request seen again is refused.
    assert_eq!(rig.post(&good, &body).await.0, 200);
    let (status, again) = rig.post(&good, &body).await;
    assert_eq!((status, again["reason"].as_str()), (401, Some("Replayed")));
    // A retry of the same push is a new request (a new nonce): it goes.
    assert_eq!(rig.post(&rig.signed(&body, now), &body).await.0, 200);
    assert_eq!(rig.apns.received().len(), 2);
}

#[tokio::test]
async fn a_host_over_its_rate_waits() {
    let rig = rig(Rate {
        per_minute: 60,
        burst: 2,
    })
    .await;
    assert_eq!(rig.push("ab12").await.0, 200);
    rig.clock.fetch_add(1, Ordering::SeqCst);
    assert_eq!(rig.push("ab12").await.0, 200);
    rig.clock.fetch_add(1, Ordering::SeqCst);
    assert_eq!(rig.push("ab12").await.0, 429);
    // A second later, one more.
    rig.clock.fetch_add(1000, Ordering::SeqCst);
    assert_eq!(rig.push("ab12").await.0, 200);
    assert_eq!(rig.apns.received().len(), 3);
}

/// A host flooding the gateway over its rate is refused, and what the
/// replay guard keeps stays as small as the rate allows: a refused request
/// leaves nothing behind.
#[tokio::test]
async fn a_flood_over_the_rate_leaves_the_replay_guard_bounded() {
    let rig = rig(Rate {
        per_minute: 60,
        burst: 2,
    })
    .await;
    let mut refused = 0;
    for _ in 0..200 {
        let (status, _) = rig.push("ab12").await;
        if status == 429 {
            refused += 1;
        }
    }
    assert_eq!(refused, 198);
    assert_eq!(rig.apns.received().len(), 2);
    assert_eq!(
        rig.served.replay_entries(),
        2,
        "nonces kept for refused requests"
    );
    // Over a long flood at the rate, the guard keeps at most a window's worth.
    for _ in 0..400 {
        rig.clock.fetch_add(1000, Ordering::SeqCst);
        rig.push("ab12").await;
    }
    let window = usize::try_from(2 * auth::WINDOW_SECS).unwrap();
    assert!(
        rig.served.replay_entries() <= window + 2,
        "{} kept",
        rig.served.replay_entries()
    );
}

#[tokio::test]
async fn the_config_names_the_hosts_and_the_key_file() {
    let dir = tempfile::tempdir().unwrap();
    let (pem, _) = test_key().unwrap();
    std::fs::write(dir.path().join("AuthKey.p8"), pem).unwrap();
    let key = PrivateKey::random(&mut OsRng, Algorithm::Ed25519).unwrap();
    let v = json!({
        "listen": "127.0.0.1:8090",
        "hosts": [{
            "name": "studio-mac",
            "token_sha256": auth::token_digest(TOKEN),
            "public_key": key.public_key().to_openssh().unwrap(),
        }],
        "rate": { "per_minute": 30 },
        "apns": { "team_id": TEAM, "key_id": KID, "key_file": "AuthKey.p8" },
    });
    let config = Config::from_json(&v, dir.path()).unwrap();
    assert_eq!(config.hosts.len(), 1);
    assert_eq!(config.rate.per_minute, 30);
    assert_eq!(
        config.apns.production,
        Upstream::parse(connecto_push_gateway::apns::PRODUCTION).unwrap()
    );
    assert!(Gateway::new(config).is_ok());
    let mut rsa = v.clone();
    rsa["hosts"][0]["public_key"] = json!("ssh-rsa AAAA nope");
    assert!(Config::from_json(&rsa, dir.path()).is_err());
    // An APNs endpoint in clear would carry the provider token and the
    // device tokens unencrypted: the config refuses it, even in this test
    // build (which has the fake APNs).
    for key in ["sandbox_url", "production_url"] {
        let mut plain = v.clone();
        plain["apns"][key] = json!("http://127.0.0.1:9");
        let refused = Config::from_json(&plain, dir.path()).err();
        assert!(
            refused.as_deref().is_some_and(|e| e.contains("https")),
            "{key}: {refused:?}"
        );
    }
}
