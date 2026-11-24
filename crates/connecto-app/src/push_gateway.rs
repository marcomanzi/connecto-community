//! The push gateway that hands this host's Siri notifications to APNs
//! (research_notes/siri-intents/push-gateway.md). It is used only when
//! `remote/push-gateway.json` names one: `{"url": "https://…", "host": "…",
//! "token": "…"}`. Without it the host promises the phone no notification.
//!
//! Each request carries the gateway's bearer token and is signed with this
//! machine's device key (`connecto_push_gateway::auth`); the key to enroll
//! at the gateway is written next to the config, as `push-gateway.pub`.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use connecto_net::auth::Signer;
use connecto_net::siri::{Push, PushError, PushSender};
use connecto_push_gateway::auth;

pub struct HttpGateway {
    url: String,
    /// This machine's name at the gateway.
    host: String,
    token: String,
    signer: Arc<dyn Signer>,
    agent: ureq::Agent,
}

impl HttpGateway {
    /// The gateway `config` names, signing with `signer`; none when the file
    /// is missing or does not name an https URL, a host and a token. The
    /// signer's public key is written to `push-gateway.pub` beside it, for
    /// the gateway's config.
    #[must_use]
    pub fn load(config: &Path, signer: Arc<dyn Signer>) -> Option<Self> {
        let v: serde_json::Value = serde_json::from_slice(&std::fs::read(config).ok()?).ok()?;
        let field = |k: &str| v.get(k).and_then(|s| s.as_str()).map(str::to_owned);
        let (Some(url), Some(host), Some(token)) = (field("url"), field("host"), field("token"))
        else {
            eprintln!(
                "push gateway: {} needs a url, a host and a token",
                config.display()
            );
            return None;
        };
        if !url.starts_with("https://") || token.is_empty() || host.is_empty() {
            eprintln!("push gateway: {} needs an https url", config.display());
            return None;
        }
        if let Ok(key) = signer.public_key().to_openssh() {
            let _ = std::fs::write(config.with_extension("pub"), format!("{key}\n"));
        }
        Some(Self::new(&url, &host, &token, signer))
    }

    /// The gateway at `url` (a test's may be plain http), as `host`.
    #[must_use]
    pub fn new(url: &str, host: &str, token: &str, signer: Arc<dyn Signer>) -> Self {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(15)))
            .http_status_as_error(false)
            .max_redirects(0)
            .build()
            .into();
        Self {
            url: url.trim_end_matches('/').to_owned(),
            host: host.to_owned(),
            token: token.to_owned(),
            signer,
            agent,
        }
    }
}

/// What the host does with the gateway's answer: the gateway says `410`
/// when APNs refused the device token (kept for a new one), `400`, `404` or
/// `413` when the push itself is wrong (dropped); anything else (`503`, `429`, `401` while the
/// enrollment or the clock is being fixed) is tried again later.
fn outcome(status: u16, reason: &str) -> Result<(), PushError> {
    match status {
        200..=299 => Ok(()),
        // APNs refused the device token: kept for a new one.
        410 => Err(PushError::BadDestination(format!(
            "APNs refused the device token ({reason})"
        ))),
        400 | 404 | 413 => Err(PushError::Permanent(format!(
            "push gateway refused it ({status} {reason})"
        ))),
        _ => Err(PushError::Temporary(format!(
            "push gateway answered {status} {reason}"
        ))),
    }
}

impl PushSender for HttpGateway {
    fn send(&self, push: &Push) -> Result<(), PushError> {
        let body = push.to_json().to_string().into_bytes();
        let time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        // New for each attempt: a retry is a new request to the gateway.
        let nonce = uuid::Uuid::new_v4().simple().to_string();
        let signed = auth::Signed {
            method: "POST",
            path: "/v1/push",
            time,
            nonce: &nonce,
            body: &body,
        };
        let signature = self
            .signer
            .sign(
                auth::NAMESPACE,
                connecto_push_gateway::ssh_key::HashAlg::Sha512,
                &signed.message(),
            )
            .ok()
            .and_then(|sig| auth::encode_signature(&sig))
            .ok_or_else(|| PushError::Temporary("the device key could not sign".into()))?;
        let request = ureq::http::Request::builder()
            .method("POST")
            .uri(format!("{}/v1/push", self.url))
            .header("authorization", format!("Bearer {}", self.token))
            .header(auth::HOST_HEADER, &self.host)
            .header(auth::TIME_HEADER, time.to_string())
            .header(auth::NONCE_HEADER, &nonce)
            .header(auth::SIGNATURE_HEADER, signature)
            .header("content-type", "application/json")
            .body(body)
            .map_err(|_| PushError::Permanent("the push could not be built".into()))?;
        let response = self
            .agent
            .run(request)
            .map_err(|e| PushError::Temporary(e.to_string()))?;
        let status = response.status().as_u16();
        let reason = response
            .into_body()
            .read_to_string()
            .ok()
            .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
            .and_then(|v| v.get("reason").and_then(|r| r.as_str()).map(str::to_owned))
            .unwrap_or_default();
        outcome(status, &reason)
    }
}

/// The host's sender, through the real gateway, to a fake APNs.
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use connecto_net::auth::KeySigner;
    use connecto_net::siri::{PushEnv, PushType};
    use connecto_push_gateway::apns::{ApnsConfig, Upstream};
    use connecto_push_gateway::fake_apns::{FakeApns, test_key};
    use connecto_push_gateway::server::{Config, Gateway, HostEntry, Rate};
    use connecto_push_gateway::ssh_key::{Algorithm, PrivateKey, rand_core::OsRng};

    fn push() -> Push {
        Push {
            device_token: "ab12".into(),
            env: PushEnv::Sandbox,
            topic: "studio.connecto.connectoFlutterApp".into(),
            push_type: PushType::Alert,
            collapse_id: "0123456789abcdef".into(),
            priority: 10,
            payload: serde_json::json!({ "aps": { "alert": { "title": "Connecto · p", "body": "La risposta è pronta" } }, "r": "req-1" }),
        }
    }

    #[test]
    fn a_hosts_notification_reaches_apns_through_the_gateway() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let key = PrivateKey::random(&mut OsRng, Algorithm::Ed25519).unwrap();
        let (apns, gateway) = runtime.block_on(async {
            let (pem, verifying) = test_key().unwrap();
            let apns = FakeApns::start(verifying, "TEAM123456", "KEY1234567")
                .await
                .unwrap();
            let upstream = Upstream::plain_loopback(&apns.url()).unwrap();
            let gateway = Arc::new(
                Gateway::new(Config {
                    listen: "127.0.0.1:0".parse().unwrap(),
                    hosts: vec![HostEntry {
                        name: "studio-mac".into(),
                        token_digest: auth::token_digest("t0ken"),
                        key: key.public_key().clone(),
                    }],
                    rate: Rate {
                        per_minute: 600,
                        burst: 50,
                    },
                    topics: vec!["studio.connecto.connectoFlutterApp".into()],
                    apns: ApnsConfig {
                        team_id: "TEAM123456".into(),
                        key_id: "KEY1234567".into(),
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
            tokio::spawn(gateway.serve(listener));
            (apns, addr)
        });
        let sender = HttpGateway::new(
            &format!("http://{gateway}"),
            "studio-mac",
            "t0ken",
            Arc::new(KeySigner(key)),
        );
        assert_eq!(sender.send(&push()), Ok(()));
        let got = apns.received();
        assert_eq!(got.len(), 1);
        assert!(got[0].token_ok);
        assert_eq!(got[0].path, "/3/device/ab12");
        assert_eq!(got[0].body["r"], "req-1");
        // APNs busy: tried again later. The token gone: never again. The
        // same push sent twice in a second is two requests, both taken.
        apns.answer(&[(503, "ServiceUnavailable"), (410, "Unregistered")]);
        assert!(matches!(sender.send(&push()), Err(PushError::Temporary(_))));
        assert!(matches!(
            sender.send(&push()),
            Err(PushError::BadDestination(_))
        ));
        assert_eq!(sender.send(&push()), Ok(()));
        assert_eq!(sender.send(&push()), Ok(()));
        // A host the gateway does not know is not dropped for it, only
        // retried: enrolling it fixes that.
        let stranger = HttpGateway::new(
            &format!("http://{gateway}"),
            "studio-mac",
            "t0ken",
            Arc::new(KeySigner(
                PrivateKey::random(&mut OsRng, Algorithm::Ed25519).unwrap(),
            )),
        );
        assert!(matches!(
            stranger.send(&push()),
            Err(PushError::Temporary(_))
        ));
        assert_eq!(apns.received().len(), 5);
        drop(runtime);
    }
}
