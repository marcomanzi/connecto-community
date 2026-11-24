//! Test doubles for Cloud SQL: a mock Google (token endpoint, tokeninfo,
//! impersonation, Admin API) on localhost, rcgen certificate authorities, and
//! a TLS front that requires a client certificate and pipes to PostgreSQL.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rcgen::{
    BasicConstraints, CertificateParams, DnType, IsCa, Issuer, KeyPair, SubjectPublicKeyInfo,
};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    io::{BufRead, BufReader, Read, Write},
    net::TcpListener,
    sync::{Arc, Mutex, OnceLock},
    thread,
    time::{Duration, SystemTime},
};

/// One RSA-2048 service account key for all tests (PKCS#8 PEM), and its
/// public key (PKCS#1 DER) to check signatures with.
pub(crate) fn sa_key() -> &'static (String, Vec<u8>) {
    static KEY: OnceLock<(String, Vec<u8>)> = OnceLock::new();
    KEY.get_or_init(|| {
        use rsa::pkcs1::EncodeRsaPublicKey;
        use rsa::pkcs8::{EncodePrivateKey, LineEnding};
        let key = rsa::RsaPrivateKey::new(&mut rand_core::OsRng, 2048).unwrap();
        (
            key.to_pkcs8_pem(LineEnding::LF).unwrap().to_string(),
            key.to_public_key()
                .to_pkcs1_der()
                .unwrap()
                .as_bytes()
                .to_vec(),
        )
    })
}

pub(crate) const SA_EMAIL: &str = "sa@proj.iam.gserviceaccount.com";

/// A service account key file whose token_uri is the mock's.
pub(crate) fn sa_json(base: &str) -> String {
    json!({
        "type": "service_account",
        "project_id": "proj",
        "private_key_id": "kid-1",
        "private_key": sa_key().0,
        "client_email": SA_EMAIL,
        "token_uri": format!("{base}/token"),
    })
    .to_string()
}

/// A certificate authority.
pub(crate) struct Ca {
    pub issuer: Issuer<'static, KeyPair>,
    pub pem: String,
    pub der: Vec<u8>,
}

impl Ca {
    pub fn new(name: &str) -> Self {
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.distinguished_name.push(DnType::CommonName, name);
        let cert = params.self_signed(&key).unwrap();
        Self {
            pem: cert.pem(),
            der: cert.der().to_vec(),
            issuer: Issuer::new(params, key),
        }
    }

    /// A server certificate with subject CN `cn` and no SAN, and its key
    /// (PKCS#8 DER).
    pub fn server(&self, cn: &str) -> (Vec<u8>, Vec<u8>) {
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params.distinguished_name.push(DnType::CommonName, cn);
        let cert = params.signed_by(&key, &self.issuer).unwrap();
        (cert.der().to_vec(), key.serialize_der())
    }

    /// A server certificate whose SAN holds the DNS name `dns` (as given,
    /// a final dot included), as on a CAS-based instance.
    pub fn server_dns(&self, dns: &str) -> Vec<u8> {
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params.distinguished_name.push(DnType::CommonName, "cas");
        params.subject_alt_names = vec![rcgen::SanType::DnsName(
            rcgen::string::Ia5String::try_from(dns.to_owned()).unwrap(),
        )];
        params.signed_by(&key, &self.issuer).unwrap().der().to_vec()
    }

    /// A client certificate for a public key (PEM), valid for `life`.
    pub fn client(&self, public_pem: &str, life: Duration) -> String {
        let spki = SubjectPublicKeyInfo::from_pem(public_pem).unwrap();
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params
            .distinguished_name
            .push(DnType::CommonName, "ephemeral");
        let now = SystemTime::now();
        params.not_before = time::OffsetDateTime::from(now - Duration::from_secs(60));
        params.not_after = time::OffsetDateTime::from(now + life);
        params.signed_by(&spki, &self.issuer).unwrap().pem()
    }
}

/// A request the mock received.
#[derive(Clone, Debug)]
pub(crate) struct Seen {
    pub method: String,
    pub path: String,
    pub bearer: Option<String>,
    pub body: String,
}

/// How the mock answers.
pub(crate) struct Behaviour {
    /// The token issued for the database login scope alone. Any other scope
    /// gets `adm-<token>`, and only that one opens the Admin API.
    pub token: String,
    /// The token endpoint answers 503.
    pub token_down: bool,
    /// The `connectSettings` answer: status and body.
    pub settings: (u16, String),
    /// Lifetimes of the ephemeral certificates, in order; the last repeats.
    pub cert_lives: VecDeque<Duration>,
    pub client_ca: Arc<Ca>,
}

pub(crate) struct MockGoogle {
    pub base: String,
    pub seen: Arc<Mutex<Vec<Seen>>>,
    pub behaviour: Arc<Mutex<Behaviour>>,
}

/// The `connectSettings` body of an instance at `ip`.
pub(crate) fn settings_json(server_ca_pem: &str, ip: &str) -> String {
    json!({
        "kind": "sql#connectSettings",
        "region": "region",
        "databaseVersion": "POSTGRES_16",
        "backendType": "SECOND_GEN",
        "ipAddresses": [{ "type": "PRIMARY", "ipAddress": ip }],
        "serverCaCert": { "cert": server_ca_pem },
        "serverCaMode": "GOOGLE_MANAGED_INTERNAL_CA",
        "pscEnabled": false,
    })
    .to_string()
}

fn form_value(body: &str, key: &str) -> Option<String> {
    body.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == key).then(|| {
            let mut out = Vec::new();
            let bytes = v.as_bytes();
            let mut i = 0;
            while i < bytes.len() {
                if bytes[i] == b'%' && i + 2 < bytes.len() + 1 {
                    let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap();
                    out.push(u8::from_str_radix(hex, 16).unwrap());
                    i += 3;
                } else {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
            String::from_utf8(out).unwrap()
        })
    })
}

/// The claims of a JWT, checking nothing.
pub(crate) fn jwt_part(jwt: &str, part: usize) -> Value {
    serde_json::from_slice(
        &URL_SAFE_NO_PAD
            .decode(jwt.split('.').nth(part).unwrap())
            .unwrap(),
    )
    .unwrap()
}

impl MockGoogle {
    pub fn start(behaviour: Behaviour) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let behaviour = Arc::new(Mutex::new(behaviour));
        let (s, b) = (seen.clone(), behaviour.clone());
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let mut parts = line.split_whitespace();
                let method = parts.next().unwrap_or_default().to_owned();
                let path = parts.next().unwrap_or_default().to_owned();
                let (mut length, mut bearer) = (0, None);
                loop {
                    let mut h = String::new();
                    reader.read_line(&mut h).unwrap();
                    let h = h.trim_end();
                    if h.is_empty() {
                        break;
                    }
                    let (k, v) = h.split_once(':').unwrap();
                    match k.to_ascii_lowercase().as_str() {
                        "content-length" => length = v.trim().parse().unwrap(),
                        "authorization" => {
                            bearer = v.trim().strip_prefix("Bearer ").map(str::to_owned);
                        }
                        _ => {}
                    }
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                let body = String::from_utf8(body).unwrap();
                s.lock().unwrap().push(Seen {
                    method: method.clone(),
                    path: path.clone(),
                    bearer: bearer.clone(),
                    body: body.clone(),
                });
                let (status, reply) = answer(&b, &method, &path, bearer.as_deref(), &body);
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                    reply.len()
                );
            }
        });
        Self {
            base,
            seen,
            behaviour,
        }
    }

    pub fn count(&self, suffix: &str) -> usize {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.path.ends_with(suffix))
            .count()
    }

    pub fn last(&self, suffix: &str) -> Seen {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|r| r.path.ends_with(suffix))
            .cloned()
            .unwrap()
    }
}

fn answer(
    b: &Mutex<Behaviour>,
    method: &str,
    path: &str,
    bearer: Option<&str>,
    body: &str,
) -> (u16, String) {
    let mut b = b.lock().unwrap();
    let admin = format!("adm-{}", b.token);
    let issued = |scope: &str| {
        if scope == super::auth::LOGIN_SCOPE {
            b.token.clone()
        } else {
            admin.clone()
        }
    };
    let authorised = bearer.is_some_and(|t| t == admin || t == format!("imp-{admin}"));
    match (method, path) {
        ("POST", "/token") if b.token_down => (503, "{}".into()),
        ("POST", "/token") => {
            let grant = form_value(body, "grant_type").unwrap_or_default();
            if grant != "refresh_token" && grant != "urn:ietf:params:oauth:grant-type:jwt-bearer" {
                return (400, json!({"error": "unsupported_grant_type"}).to_string());
            }
            if form_value(body, "refresh_token").as_deref() == Some("revoked") {
                return (400, json!({"error": "invalid_grant"}).to_string());
            }
            let scope = form_value(body, "scope").unwrap_or_else(|| {
                let assertion = form_value(body, "assertion").unwrap_or_default();
                jwt_part(&assertion, 1)["scope"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned()
            });
            (
                200,
                json!({"access_token": issued(&scope), "expires_in": 3599, "token_type": "Bearer"})
                    .to_string(),
            )
        }
        ("POST", "/tokeninfo") => (200, json!({"email": "person@example.com"}).to_string()),
        ("POST", p) if p.ends_with(":generateAccessToken") && authorised => {
            let v: Value = serde_json::from_str(body).unwrap();
            let scope = v["scope"][0].as_str().unwrap_or_default().to_owned();
            (
                200,
                json!({"accessToken": format!("imp-{}", issued(&scope)), "expireTime": "2999-01-01T00:00:00Z"})
                    .to_string(),
            )
        }
        _ if !authorised => (401, json!({"error": {"code": 401}}).to_string()),
        ("GET", p) if p.ends_with("/connectSettings") => b.settings.clone(),
        ("POST", p) if p.ends_with(":generateEphemeralCert") => {
            if b.settings.0 != 200 {
                return b.settings.clone();
            }
            let v: Value = serde_json::from_str(body).unwrap();
            let life = if b.cert_lives.len() > 1 {
                b.cert_lives.pop_front().unwrap()
            } else {
                b.cert_lives[0]
            };
            let cert = b.client_ca.client(v["public_key"].as_str().unwrap(), life);
            (
                200,
                json!({"ephemeralCert": {"cert": cert, "kind": "sql#sslCert"}}).to_string(),
            )
        }
        _ => (404, json!({"error": {"code": 404}}).to_string()),
    }
}

/// A TLS front on 127.0.0.1 like Cloud SQL's port 3307: it presents
/// `server` (DER cert, PKCS#8 key), requires a client certificate from
/// `client_ca`, and pipes the plain stream to `upstream`.
pub(crate) async fn tls_front(
    server: (Vec<u8>, Vec<u8>),
    client_ca: &Ca,
    upstream: std::net::SocketAddr,
) -> u16 {
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(CertificateDer::from(client_ca.der.clone()))
        .unwrap();
    let verifier = rustls::server::WebPkiClientVerifier::builder_with_provider(
        Arc::new(roots),
        provider.clone(),
    )
    .build()
    .unwrap();
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .with_client_cert_verifier(verifier)
        .with_single_cert(
            vec![CertificateDer::from(server.0)],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(server.1)),
        )
        .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((tcp, _)) = listener.accept().await else {
                return;
            };
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let Ok(mut tls) = acceptor.accept(tcp).await else {
                    return;
                };
                let Ok(mut pg) = tokio::net::TcpStream::connect(upstream).await else {
                    return;
                };
                let _ = tokio::io::copy_bidirectional(&mut tls, &mut pg).await;
            });
        }
    });
    port
}
