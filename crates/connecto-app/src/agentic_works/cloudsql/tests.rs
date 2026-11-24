#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::testkit::{self, Behaviour, Ca, MockGoogle, SA_EMAIL};
use super::*;
use admin::{CaMode, Context};
use connecto_core::agentic_works::IpType;
use serde_json::json;
use std::collections::VecDeque;

const NAME: &str = "proj:region:inst";

fn mock(token: &str, server_ca: &Ca, client_ca: Arc<Ca>, ip: &str) -> MockGoogle {
    MockGoogle::start(Behaviour {
        token: token.into(),
        settings: (200, testkit::settings_json(&server_ca.pem, ip)),
        token_down: false,
        cert_lives: VecDeque::from([Duration::from_secs(3600)]),
        client_ca,
    })
}

fn endpoints(base: &str) -> Endpoints {
    Endpoints {
        sqladmin: base.into(),
        oauth_token: format!("{base}/token"),
        tokeninfo: format!("{base}/tokeninfo"),
    }
}

fn service_account(base: &str) -> auth::ServiceAccount {
    match Source::from_json(&testkit::sa_json(base)).unwrap() {
        Source::ServiceAccount(sa) => sa,
        _ => panic!("not a service account"),
    }
}

// ---- JWT and token exchange ----

#[test]
fn jwt_has_the_service_account_claims_and_a_valid_rs256_signature() {
    let sa = service_account("https://oauth2.googleapis.com");
    let now = epoch_plus(1_700_000_000);
    let jwt = auth::jwt(&sa, &[auth::ADMIN_SCOPE, auth::LOGIN_SCOPE], now).unwrap();
    let header = testkit::jwt_part(&jwt, 0);
    assert_eq!(
        header,
        json!({"alg": "RS256", "typ": "JWT", "kid": "kid-1"})
    );
    let claims = testkit::jwt_part(&jwt, 1);
    assert_eq!(
        claims,
        json!({
            "iss": SA_EMAIL,
            "scope": format!("{} {}", auth::ADMIN_SCOPE, auth::LOGIN_SCOPE),
            "aud": "https://oauth2.googleapis.com/token",
            "iat": 1_700_000_000,
            "exp": 1_700_003_600,
        })
    );
    let (input, sig) = jwt.rsplit_once('.').unwrap();
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    ring::signature::UnparsedPublicKey::new(
        &ring::signature::RSA_PKCS1_2048_8192_SHA256,
        &testkit::sa_key().1,
    )
    .verify(input.as_bytes(), &URL_SAFE_NO_PAD.decode(sig).unwrap())
    .unwrap();
}

fn epoch_plus(secs: u64) -> SystemTime {
    std::time::UNIX_EPOCH + Duration::from_secs(secs)
}

#[tokio::test]
async fn service_account_token_is_exchanged_at_its_token_uri_and_cached() {
    let ca = Arc::new(Ca::new("client CA"));
    let google = mock("tok-sa", &Ca::new("server CA"), ca, "127.0.0.1");
    let connector = Connector::new(endpoints(&google.base), None, PORT);
    let raw = testkit::sa_json(&google.base);
    let who = Identity::parse(&raw).unwrap();
    let token = connector.token(&who, &[auth::ADMIN_SCOPE]).await.unwrap();
    assert_eq!(token.access, "adm-tok-sa");
    assert_eq!(token.email.as_deref(), Some(SA_EMAIL));
    let seen = google.last("/token");
    assert_eq!(seen.method, "POST");
    assert!(
        seen.body
            .contains("grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Ajwt-bearer")
    );
    let assertion = seen.body.split("assertion=").nth(1).unwrap();
    assert_eq!(testkit::jwt_part(assertion, 1)["iss"], SA_EMAIL);
    // Cached: no second exchange; another scope set is another token.
    connector.token(&who, &[auth::ADMIN_SCOPE]).await.unwrap();
    assert_eq!(google.count("/token"), 1);
    connector
        .token(&who, &[auth::ADMIN_SCOPE, auth::LOGIN_SCOPE])
        .await
        .unwrap();
    assert_eq!(google.count("/token"), 2);
}

#[test]
fn tokens_are_refreshed_five_minutes_before_they_expire() {
    let now = SystemTime::now();
    let token = |left: u64| Token {
        access: "t".into(),
        expires: now + Duration::from_secs(left),
        email: None,
    };
    assert!(token(301).fresh(now, REFRESH_BEFORE));
    assert!(!token(299).fresh(now, REFRESH_BEFORE));
}

// ---- Application Default Credentials ----

fn adc_file(dir: &tempfile::TempDir, value: &serde_json::Value) -> PathBuf {
    let path = dir.path().join("application_default_credentials.json");
    std::fs::write(&path, value.to_string()).unwrap();
    path
}

#[tokio::test]
async fn adc_kinds_authorized_user_impersonated_and_unsupported() {
    let ca = Arc::new(Ca::new("client CA"));
    let google = mock("tok-user", &Ca::new("server CA"), ca, "127.0.0.1");
    let dir = tempfile::tempdir().unwrap();
    let user = json!({
        "type": "authorized_user",
        "client_id": "cid",
        "client_secret": "csecret",
        "refresh_token": "rtoken",
    });
    let connector = Connector::new(endpoints(&google.base), Some(adc_file(&dir, &user)), PORT);
    let who = connector.identity(&GoogleCredential::Adc, None).unwrap();
    let token = connector.token(&who, &[auth::ADMIN_SCOPE]).await.unwrap();
    assert_eq!(token.access, "adm-tok-user");
    assert_eq!(token.email.as_deref(), Some("person@example.com"));
    let seen = google.last("/token");
    assert!(seen.body.contains("grant_type=refresh_token"));
    assert!(seen.body.contains("refresh_token=rtoken"));
    assert!(
        seen.body
            .contains("scope=https%3A%2F%2Fwww.googleapis.com%2Fauth%2Fcloud-platform")
    );
    // userinfo.email too, so the email (the default IAM user) is known.
    assert!(seen.body.contains("userinfo.email"));

    let revoked = json!({
        "type": "authorized_user", "client_id": "c", "client_secret": "s", "refresh_token": "revoked",
    });
    let connector = Connector::new(
        endpoints(&google.base),
        Some(adc_file(&dir, &revoked)),
        PORT,
    );
    let who = connector.identity(&GoogleCredential::Adc, None).unwrap();
    let Err(why) = connector.token(&who, &[auth::ADMIN_SCOPE]).await else {
        panic!("a revoked login gave a token")
    };
    assert!(
        why.contains("gcloud auth application-default login"),
        "{why}"
    );

    let impersonated = json!({
        "type": "impersonated_service_account",
        "service_account_impersonation_url": format!(
            "{}/v1/projects/-/serviceAccounts/robot@proj.iam.gserviceaccount.com:generateAccessToken",
            google.base
        ),
        "delegates": [],
        "source_credentials": user,
    });
    let connector = Connector::new(
        endpoints(&google.base),
        Some(adc_file(&dir, &impersonated)),
        PORT,
    );
    let who = connector.identity(&GoogleCredential::Adc, None).unwrap();
    let token = connector.token(&who, &[auth::ADMIN_SCOPE]).await.unwrap();
    assert_eq!(token.access, "imp-adm-tok-user");
    assert_eq!(
        token.email.as_deref(),
        Some("robot@proj.iam.gserviceaccount.com")
    );
    let seen = google.last(":generateAccessToken");
    assert_eq!(seen.bearer.as_deref(), Some("adm-tok-user"));
    let body: serde_json::Value = serde_json::from_str(&seen.body).unwrap();
    assert_eq!(body["scope"], json!([auth::ADMIN_SCOPE]));

    let external = json!({"type": "external_account", "audience": "x"});
    let connector = Connector::new(
        endpoints(&google.base),
        Some(adc_file(&dir, &external)),
        PORT,
    );
    let Err(why) = connector.identity(&GoogleCredential::Adc, None) else {
        panic!("external_account accepted")
    };
    assert!(why.contains("not supported"), "{why}");

    let none = Connector::new(
        endpoints(&google.base),
        Some(dir.path().join("missing.json")),
        PORT,
    );
    let Err(why) = none.identity(&GoogleCredential::Adc, None) else {
        panic!("no file accepted")
    };
    assert_eq!(why, auth::NO_CREDENTIAL);
    let Err(why) = none.identity(&GoogleCredential::Key("K".into()), None) else {
        panic!("no key accepted")
    };
    assert_eq!(why, auth::NO_CREDENTIAL);
}

#[test]
fn credential_files_must_send_credentials_over_https() {
    let mut key: serde_json::Value = serde_json::from_str(&testkit::sa_json("x")).unwrap();
    key["token_uri"] = json!("http://evil.example/token");
    assert!(Source::from_json(&key.to_string()).is_err());
    key["token_uri"] = json!("https://oauth2.googleapis.com/token");
    assert!(Source::from_json(&key.to_string()).is_ok());
    key["private_key"] = json!("-----BEGIN PRIVATE KEY-----\nAAAA\n-----END PRIVATE KEY-----\n");
    let Err(why) = Source::from_json(&key.to_string()) else {
        panic!("a broken key accepted")
    };
    assert!(!why.contains("AAAA"));
    assert_eq!(auth::iam_user(SA_EMAIL), "sa@proj.iam");
    assert_eq!(auth::iam_user("me@example.com"), "me@example.com");
}

// ---- Certificate cache ----

fn material(left: Duration) -> Material {
    Material {
        settings: Arc::new(admin::Settings {
            version: "POSTGRES_16".into(),
            public_ip: None,
            private_ip: None,
            psc: false,
            psc_dns: None,
            dns_names: Vec::new(),
            ca: Vec::new(),
            ca_mode: CaMode::Internal,
        }),
        cert: Vec::new(),
        expires: SystemTime::now() + left,
    }
}

#[test]
fn certificate_refresh_thresholds_and_rate_limit() {
    let now = SystemTime::now();
    let at = Instant::now();
    let long_ago = at.checked_sub(Duration::from_secs(60));
    let recently = at.checked_sub(Duration::from_secs(10));
    let slot = |left: Option<u64>, attempt: Option<Instant>, error: Option<&str>| Slot {
        current: left.map(|s| material(Duration::from_secs(s))),
        last_attempt: attempt,
        last_error: error.map(str::to_owned),
    };
    // Nothing yet: fetch.
    assert_eq!(slot(None, None, None).plan(now, at), Plan::Refresh);
    // More than five minutes left: use it.
    assert_eq!(slot(Some(3000), long_ago, None).plan(now, at), Plan::Use);
    // Less than five minutes: refresh, keeping it if the refresh fails...
    assert_eq!(
        slot(Some(200), long_ago, None).plan(now, at),
        Plan::RefreshOrUse
    );
    // ...but at most once per 30 seconds.
    assert_eq!(slot(Some(200), recently, None).plan(now, at), Plan::Use);
    assert_eq!(
        slot(Some(200), recently, Some("x")).plan(now, at),
        Plan::Use
    );
    // Expired, or an attempt cancelled before it ended: still no second
    // request within 30 seconds.
    assert!(matches!(
        slot(Some(0), recently, None).plan(now, at),
        Plan::Fail(_)
    ));
    assert!(matches!(
        slot(None, recently, None).plan(now, at),
        Plan::Fail(_)
    ));
    // A failure within 30 seconds with nothing usable is not retried.
    assert_eq!(
        slot(Some(0), recently, Some("why")).plan(now, at),
        Plan::Fail("why".into())
    );
    assert_eq!(
        slot(None, long_ago, Some("why")).plan(now, at),
        Plan::Refresh
    );
}

// ---- Admin API answers ----

#[test]
fn admin_api_errors_in_plain_words() {
    let name = InstanceName::parse(NAME).unwrap();
    let ctx = Context {
        email: SA_EMAIL,
        name: &name,
        connection: NAME,
    };
    assert_eq!(
        admin::api_error(
            403,
            r#"{"error":{"code":403,"status":"PERMISSION_DENIED"}}"#,
            &ctx
        ),
        "`sa@proj.iam.gserviceaccount.com` needs the role Cloud SQL Client (`roles/cloudsql.client`) on `proj`."
    );
    let disabled = r#"{"error":{"code":403,"details":[{"@type":"type.googleapis.com/google.rpc.ErrorInfo","reason":"SERVICE_DISABLED"}]}}"#;
    assert_eq!(
        admin::api_error(403, disabled, &ctx),
        "Enable the Cloud SQL Admin API: `gcloud services enable sqladmin.googleapis.com --project=proj`."
    );
    assert_eq!(
        admin::api_error(404, "{}", &ctx),
        "Instance `proj:region:inst` not found (check project, region and name)."
    );
    assert!(admin::api_error(401, "{}", &ctx).contains(SA_EMAIL));
    assert_eq!(
        admin::api_error(
            500,
            r#"{"error":{"status":"INTERNAL","message":"echo ya29.secret"}}"#,
            &ctx
        ),
        "The Cloud SQL Admin API answered 500 (INTERNAL). Try again later."
    );
}

#[test]
fn connect_settings_choose_the_address_and_refuse_what_cannot_work() {
    let name = InstanceName::parse(NAME).unwrap();
    let ctx = Context {
        email: SA_EMAIL,
        name: &name,
        connection: NAME,
    };
    let ca = Ca::new("server CA");
    let mut v: serde_json::Value =
        serde_json::from_str(&testkit::settings_json(&ca.pem, "203.0.113.5")).unwrap();
    let s = admin::parse_settings(&v.to_string(), &ctx).unwrap();
    assert_eq!(s.ca_mode, CaMode::Internal);
    assert_eq!(s.ca, vec![ca.der.clone()]);
    assert_eq!(s.address(IpType::Public).unwrap(), "203.0.113.5");
    assert_eq!(
        s.address(IpType::Private).unwrap_err(),
        "The instance has no private IP; choose Public or Private Service Connect."
    );
    v["ipAddresses"] = json!([{ "type": "PRIVATE", "ipAddress": "10.0.0.3" }]);
    let s = admin::parse_settings(&v.to_string(), &ctx).unwrap();
    assert_eq!(
        s.address(IpType::Public).unwrap_err(),
        "The instance has no public IP; choose Private or Private Service Connect."
    );
    assert_eq!(s.address(IpType::Private).unwrap(), "10.0.0.3");
    v["pscEnabled"] = json!(true);
    v["dnsName"] = json!("abc.proj.sql.goog.");
    v["serverCaMode"] = json!("GOOGLE_MANAGED_CAS_CA");
    let s = admin::parse_settings(&v.to_string(), &ctx).unwrap();
    assert_eq!(s.ca_mode, CaMode::Cas);
    assert_eq!(s.address(IpType::Psc).unwrap(), "abc.proj.sql.goog");
    // The newer dnsNames list: its instance-scoped PSC entry comes first.
    v["dnsNames"] = json!([
        { "name": "x.region.sql.goog.", "connectionType": "PUBLIC", "dnsScope": "INSTANCE" },
        { "name": "psc.proj.sql.goog.", "connectionType": "PRIVATE_SERVICE_CONNECT", "dnsScope": "INSTANCE" },
        { "name": "write.proj.sql.goog.", "connectionType": "PUBLIC", "dnsScope": "CLUSTER" },
    ]);
    let s = admin::parse_settings(&v.to_string(), &ctx).unwrap();
    assert_eq!(s.address(IpType::Psc).unwrap(), "psc.proj.sql.goog");
    assert_eq!(
        s.dns_names,
        [
            "abc.proj.sql.goog",
            "psc.proj.sql.goog",
            "x.region.sql.goog"
        ]
    );
    v["dnsNames"] = json!([]);
    v["databaseVersion"] = json!("MYSQL_8_0");
    assert!(
        admin::parse_settings(&v.to_string(), &ctx)
            .unwrap_err()
            .contains("needs Cloud SQL for PostgreSQL")
    );
    v["databaseVersion"] = json!("POSTGRES_15");
    v["region"] = json!("us-east1");
    assert!(
        admin::parse_settings(&v.to_string(), &ctx)
            .unwrap_err()
            .contains("is in region us-east1")
    );
}

// ---- Integration: mock Admin API, TLS front, Docker PostgreSQL ----

/// A Cloud SQL-like instance on localhost: a TLS front presenting `server`,
/// before the test's PostgreSQL.
struct Rig {
    google: MockGoogle,
    connector: Connector,
    key: String,
}

impl Rig {
    async fn start(
        upstream: std::net::SocketAddr,
        server_ca: &Ca,
        server: (Vec<u8>, Vec<u8>),
        token: &str,
    ) -> Self {
        let client_ca = Arc::new(Ca::new("client CA"));
        let port = testkit::tls_front(server, &client_ca, upstream).await;
        let google = mock(token, server_ca, client_ca, "127.0.0.1");
        let connector = Connector::new(endpoints(&google.base), None, port);
        let key = testkit::sa_json(&google.base);
        Self {
            google,
            connector,
            key,
        }
    }

    fn cfg(&self, user: &str, iam: bool) -> CloudSql {
        CloudSql {
            connection_name: NAME.into(),
            database: "postgres".into(),
            user: user.into(),
            ip_type: IpType::Public,
            iam_auth: iam,
            credential: GoogleCredential::Key("KEY".into()),
            password_secret: if iam { String::new() } else { "PW".into() },
        }
    }

    async fn connect(&self, cfg: &CloudSql, password: Option<&str>) -> Result<Client> {
        self.connector
            .connect(cfg, Some(&self.key), password, &mut Vec::new())
            .await
    }
}

/// A statement under way on a Cloud SQL connection is cancelled with that
/// connection's own key, through the route the connection took (the same
/// address and TLS): the statement ends at once, the connection stays.
#[tokio::test]
#[ignore = "starts a disposable PostgreSQL 16 Docker container"]
async fn a_cloudsql_statement_is_cancelled_through_its_own_route() {
    use crate::agentic_works::tests::Postgres;
    let (_pg, dsn, _admin) = Postgres::start().await;
    let parsed: tokio_postgres::Config = dsn.parse().unwrap();
    let upstream = std::net::SocketAddr::from(([127, 0, 0, 1], parsed.get_ports()[0]));
    let password = parsed
        .get_password()
        .map(|p| String::from_utf8(p.to_vec()).unwrap())
        .unwrap();
    let server_ca = Ca::new("server CA");
    let rig = Rig::start(upstream, &server_ca, server_ca.server("proj:inst"), "tok-1").await;
    let (db, route) = rig
        .connector
        .connect_routed(
            &rig.cfg("postgres", false),
            Some(&rig.key),
            Some(&password),
            &mut Vec::new(),
        )
        .await
        .unwrap();
    db.batch_execute("SET statement_timeout = 0").await.unwrap();
    let canceller = super::super::Canceller::Routed(db.cancel_token(), route);
    let db = std::sync::Arc::new(db);
    let running = {
        let db = std::sync::Arc::clone(&db);
        tokio::spawn(async move { db.batch_execute("SELECT pg_sleep(30)").await })
    };
    tokio::time::sleep(Duration::from_millis(500)).await;
    let started = std::time::Instant::now();
    canceller.cancel().await.unwrap();
    let ended = tokio::time::timeout(Duration::from_secs(5), running)
        .await
        .expect("the statement was cancelled")
        .unwrap();
    assert_eq!(
        ended.unwrap_err().code(),
        Some(&tokio_postgres::error::SqlState::QUERY_CANCELED)
    );
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(
        db.query_one("SELECT 1", &[])
            .await
            .unwrap()
            .get::<_, i32>(0),
        1
    );
}

#[tokio::test]
#[ignore = "starts a disposable PostgreSQL 16 Docker container"]
async fn cloudsql_link_identity_ca_refresh_role_and_iam_through_a_mock_instance() {
    use crate::agentic_works::{store::Store, tests::Postgres};
    let (_pg, dsn, admin_db) = Postgres::start().await;
    let parsed: tokio_postgres::Config = dsn.parse().unwrap();
    let upstream = std::net::SocketAddr::from(([127, 0, 0, 1], parsed.get_ports()[0]));
    let password = parsed
        .get_password()
        .map(|p| String::from_utf8(p.to_vec()).unwrap())
        .unwrap();
    let server_ca = Ca::new("server CA");

    // Create queue and link succeeds, and the link survives the store.
    let rig = Rig::start(upstream, &server_ca, server_ca.server("proj:inst"), "tok-1").await;
    let cfg = rig.cfg("postgres", false);
    let mut report = Vec::new();
    let mut db = rig
        .connector
        .connect(&cfg, Some(&rig.key), Some(&password), &mut report)
        .await
        .unwrap();
    assert_eq!(report.len(), 5, "{report:?}");
    assert!(report[0].contains(SA_EMAIL), "{report:?}");
    let queue = db::setup(&mut db, "p", true).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let database = connecto_core::agentic_works::Database::CloudSql(cfg.clone());
    store.link("p", "default", &database, &queue).unwrap();
    let (linked, linked_queue) = store.linked("p", "default").unwrap().unwrap();
    assert_eq!((linked, linked_queue.as_str()), (database, queue.as_str()));
    let mut again = rig.connect(&cfg, Some(&password)).await.unwrap();
    assert_eq!(db::setup(&mut again, "p", false).await.unwrap(), queue);
    assert!(
        db::claim(&mut again, "p", &queue, "p", 1, &|| true)
            .await
            .unwrap()
            .is_none()
    );
    // The wrong password is said plainly.
    let Err(why) = rig.connect(&cfg, Some("wrong-password")).await else {
        panic!("wrong password accepted")
    };
    assert_eq!(why, "Password authentication failed for postgres.");

    // Wrong CN: the server is another instance.
    let rig = Rig::start(
        upstream,
        &server_ca,
        server_ca.server("proj:other"),
        "tok-2",
    )
    .await;
    let Err(why) = rig
        .connect(&rig.cfg("postgres", false), Some(&password))
        .await
    else {
        panic!("wrong CN accepted")
    };
    assert_eq!(why, "The server is not `proj:region:inst`.");

    // A server signed by another CA.
    let other_ca = Ca::new("other CA");
    let rig = Rig::start(upstream, &server_ca, other_ca.server("proj:inst"), "tok-3").await;
    let Err(why) = rig
        .connect(&rig.cfg("postgres", false), Some(&password))
        .await
    else {
        panic!("foreign CA accepted")
    };
    assert!(why.contains("not signed by the CA"), "{why}");

    // An ephemeral certificate that ran out is refreshed before connecting.
    let rig = Rig::start(upstream, &server_ca, server_ca.server("proj:inst"), "tok-4").await;
    rig.connect(&rig.cfg("postgres", false), Some(&password))
        .await
        .unwrap();
    assert_eq!(rig.google.count(":generateEphemeralCert"), 1);
    // An hour later, as the cache sees it.
    for slot in rig.connector.certs.lock().unwrap().values() {
        let mut slot = slot.try_lock().unwrap();
        if let Some(m) = &mut slot.current {
            m.expires = SystemTime::now() - Duration::from_secs(1);
        }
        slot.last_attempt = Instant::now().checked_sub(Duration::from_secs(3600));
    }
    rig.connect(&rig.cfg("postgres", false), Some(&password))
        .await
        .unwrap();
    assert_eq!(rig.google.count(":generateEphemeralCert"), 2);
    rig.connect(&rig.cfg("postgres", false), Some(&password))
        .await
        .unwrap();
    assert_eq!(rig.google.count(":generateEphemeralCert"), 2);

    // 403: the role message.
    let rig = Rig::start(upstream, &server_ca, server_ca.server("proj:inst"), "tok-5").await;
    rig.google.behaviour.lock().unwrap().settings = (
        403,
        r#"{"error":{"code":403,"status":"PERMISSION_DENIED"}}"#.into(),
    );
    let Err(why) = rig
        .connect(&rig.cfg("postgres", false), Some(&password))
        .await
    else {
        panic!("403 accepted")
    };
    assert_eq!(
        why,
        "`sa@proj.iam.gserviceaccount.com` needs the role Cloud SQL Client (`roles/cloudsql.client`) on `proj`."
    );

    // IAM sign-in: the OAuth token is the password, the user is the
    // service account's IAM user, and the certificate is bound to the token.
    let token = format!("ya29.iam-{}", uuid::Uuid::new_v4());
    admin_db
        .batch_execute(&format!(
            "CREATE ROLE \"sa@proj.iam\" LOGIN PASSWORD '{token}'"
        ))
        .await
        .unwrap();
    let rig = Rig::start(upstream, &server_ca, server_ca.server("proj:inst"), &token).await;
    let db = rig.connect(&rig.cfg("", true), None).await.unwrap();
    let user: String = db
        .query_one("SELECT current_user::text", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(user, "sa@proj.iam");
    let cert_request: serde_json::Value =
        serde_json::from_str(&rig.google.last(":generateEphemeralCert").body).unwrap();
    // The certificate is bound to the login-only token, which is also the
    // password; the Admin API token is a different, admin-only one.
    assert_eq!(cert_request["access_token"], json!(token));
    assert_eq!(
        rig.google.last(":generateEphemeralCert").bearer,
        Some(format!("adm-{token}"))
    );
    let scopes: Vec<String> = rig
        .google
        .seen
        .lock()
        .unwrap()
        .iter()
        .filter(|r| r.path == "/token")
        .map(|r| {
            let assertion = r.body.split("assertion=").nth(1).unwrap().to_owned();
            testkit::jwt_part(&assertion, 1)["scope"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    assert_eq!(scopes, [auth::ADMIN_SCOPE, auth::LOGIN_SCOPE]);
}

/// Against a real instance: `CONNECTO_CLOUDSQL_TEST_INSTANCE` (connection
/// name), `CONNECTO_CLOUDSQL_TEST_KEY` (path to a service account key), and
/// optionally `CONNECTO_CLOUDSQL_TEST_DATABASE`, `_USER` and `_PASSWORD`
/// (without a password, IAM database sign-in).
#[tokio::test]
#[ignore = "connects to a real Cloud SQL instance named in the environment"]
async fn live_cloudsql_select_one() {
    let env = |k: &str| std::env::var(format!("CONNECTO_CLOUDSQL_TEST_{k}")).ok();
    let (Some(instance), Some(key)) = (env("INSTANCE"), env("KEY")) else {
        eprintln!("CONNECTO_CLOUDSQL_TEST_INSTANCE and _KEY are not set: skipped");
        return;
    };
    let key = std::fs::read_to_string(key).unwrap();
    let password = env("PASSWORD");
    let cfg = CloudSql {
        connection_name: instance,
        database: env("DATABASE").unwrap_or_else(|| "postgres".into()),
        user: env("USER").unwrap_or_default(),
        ip_type: IpType::Public,
        iam_auth: password.is_none(),
        credential: GoogleCredential::Key("live".into()),
        password_secret: String::new(),
    };
    let mut report = Vec::new();
    let db = Connector::default()
        .connect(&cfg, Some(&key), password.as_deref(), &mut report)
        .await
        .unwrap_or_else(|e| panic!("{e}\n{report:?}"));
    let one: i32 = db.query_one("SELECT 1", &[]).await.unwrap().get(0);
    assert_eq!(one, 1);
    drop(db);
}

// ---- Server identity ----

fn verify(ca: &Ca, expect: tls::Expect, server: &[u8]) -> std::result::Result<(), rustls::Error> {
    use rustls::client::danger::ServerCertVerifier;
    use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
    let mut roots = rustls::RootCertStore::empty();
    roots.add(CertificateDer::from(ca.der.clone())).unwrap();
    tls::InstanceVerifier {
        roots,
        expect,
        provider: Arc::new(rustls::crypto::ring::default_provider()),
    }
    .verify_server_cert(
        &CertificateDer::from(server.to_vec()),
        &[],
        &ServerName::try_from("127.0.0.1").unwrap(),
        &[],
        UnixTime::now(),
    )
    .map(drop)
}

#[test]
fn the_server_must_be_the_instance_signed_by_its_ca() {
    let ca = Ca::new("server CA");
    let cn = || tls::Expect::Cn("proj:inst".into());
    assert!(verify(&ca, cn(), &ca.server("proj:inst").0).is_ok());
    assert!(matches!(
        verify(&ca, cn(), &ca.server("proj:other").0),
        Err(rustls::Error::InvalidCertificate(
            rustls::CertificateError::NotValidForName
        ))
    ));
    assert!(verify(&ca, cn(), &Ca::new("other").server("proj:inst").0).is_err());
    // CAS: the SAN names the instance, with or without a final dot.
    let dns = || tls::Expect::Dns(vec!["abc.proj.sql.goog".into()]);
    assert!(verify(&ca, dns(), &ca.server_dns("abc.proj.sql.goog.")).is_ok());
    assert!(verify(&ca, dns(), &ca.server_dns("ABC.proj.sql.goog")).is_ok());
    assert!(verify(&ca, dns(), &ca.server_dns("evil.proj.sql.goog")).is_err());
    assert!(
        verify(
            &ca,
            dns(),
            &Ca::new("other").server_dns("abc.proj.sql.goog")
        )
        .is_err()
    );
    // A CN-only certificate does not pass a CAS check, nor a SAN a CN one.
    assert!(verify(&ca, dns(), &ca.server("abc.proj.sql.goog").0).is_err());
}

#[test]
fn credentials_go_only_to_google() {
    assert!(auth::impersonation_url_ok(
        "https://iamcredentials.googleapis.com/v1/projects/-/serviceAccounts/robot@proj.iam.gserviceaccount.com:generateAccessToken"
    ));
    for bad in [
        "https://evil.example/v1/projects/-/serviceAccounts/robot@p.iam.gserviceaccount.com:generateAccessToken",
        "https://iamcredentials.googleapis.com.evil.example/v1/projects/-/serviceAccounts/r@p:generateAccessToken",
        "https://iamcredentials.googleapis.com/v1/projects/-/serviceAccounts/../x@y:generateAccessToken",
        "http://iamcredentials.googleapis.com/v1/projects/-/serviceAccounts/r@p:generateAccessToken",
    ] {
        assert!(!auth::impersonation_url_ok(bad), "{bad}");
    }
    let mut key: serde_json::Value = serde_json::from_str(&testkit::sa_json("x")).unwrap();
    key["token_uri"] = json!("https://evil.example/token");
    assert!(Source::from_json(&key.to_string()).is_err());
}

fn unreachable_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

#[tokio::test]
async fn a_too_short_certificate_is_refused_and_not_refetched_at_once() {
    let client_ca = Arc::new(Ca::new("client CA"));
    let google = mock("tok", &Ca::new("server CA"), client_ca, "127.0.0.1");
    // Shorter than five and a half minutes: it would need a refresh within
    // the 30 seconds a refresh must wait.
    google.behaviour.lock().unwrap().cert_lives = VecDeque::from([Duration::from_secs(300)]);
    let connector = Connector::new(endpoints(&google.base), None, unreachable_port());
    let key = testkit::sa_json(&google.base);
    let cfg = CloudSql {
        connection_name: NAME.into(),
        database: "postgres".into(),
        user: "u".into(),
        credential: GoogleCredential::Key("K".into()),
        ..CloudSql::default()
    };
    for _ in 0..2 {
        let Err(why) = connector
            .connect(&cfg, Some(&key), Some("pw"), &mut Vec::new())
            .await
        else {
            panic!("connected")
        };
        assert!(why.contains("expires too soon"), "{why}");
    }
    assert_eq!(google.count(":generateEphemeralCert"), 1);
}

#[tokio::test]
async fn a_valid_certificate_outlives_an_oauth_outage() {
    let client_ca = Arc::new(Ca::new("client CA"));
    let google = mock("tok", &Ca::new("server CA"), client_ca, "127.0.0.1");
    let connector = Connector::new(endpoints(&google.base), None, unreachable_port());
    let key = testkit::sa_json(&google.base);
    let cfg = CloudSql {
        connection_name: NAME.into(),
        database: "postgres".into(),
        user: "u".into(),
        credential: GoogleCredential::Key("K".into()),
        ..CloudSql::default()
    };
    // Nothing listens on the port: each attempt ends at the TCP step.
    let Err(why) = connector
        .connect(&cfg, Some(&key), Some("pw"), &mut Vec::new())
        .await
    else {
        panic!("connected")
    };
    assert!(why.starts_with("Cannot reach"), "{why}");
    google.behaviour.lock().unwrap().token_down = true;
    connector.tokens.lock().unwrap().clear();
    let mut report = Vec::new();
    let Err(why) = connector
        .connect(&cfg, Some(&key), Some("pw"), &mut report)
        .await
    else {
        panic!("connected")
    };
    assert!(why.starts_with("Cannot reach"), "{why} {report:?}");
    assert_eq!(google.count(":generateEphemeralCert"), 1);
}
