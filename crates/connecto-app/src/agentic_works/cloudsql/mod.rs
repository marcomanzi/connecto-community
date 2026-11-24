//! Google Cloud SQL for PostgreSQL, connected the way Google's Cloud SQL
//! connectors do: the Google identity is authorised through IAM, the Admin
//! API issues a short-lived client certificate, and PostgreSQL is spoken
//! inside mutual TLS to the instance's port 3307, verified against the
//! instance's own CA and name. No IP allowlist, no proxy process, no gcloud
//! at runtime. Tokens, keys and passwords never reach a log or a message.

mod admin;
mod auth;
#[cfg(test)]
pub(super) mod testkit;
#[cfg(test)]
mod tests;
mod tls;

use super::db::{self, Result};
use admin::Settings;
use auth::{Source, Token};
use connecto_core::agentic_works::{CloudSql, GoogleCredential, InstanceName};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime},
};
use tokio::{net::TcpStream, time::timeout};
use tokio_postgres::{Client, Config, NoTls, config::SslMode};

/// Refresh a token or certificate when less than this is left.
const REFRESH_BEFORE: Duration = Duration::from_secs(5 * 60);
/// A certificate with less than this left is not used at all.
const USABLE_FOR: Duration = Duration::from_secs(5);
/// At most one certificate refresh per instance in this time.
const REFRESH_EVERY: Duration = Duration::from_secs(30);
/// Google's server-side proxy port.
const PORT: u16 = 3307;

/// Where Google's services are. Tests point them at a local mock.
#[derive(Clone, Debug)]
pub(crate) struct Endpoints {
    pub sqladmin: String,
    pub oauth_token: String,
    pub tokeninfo: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            sqladmin: "https://sqladmin.googleapis.com".into(),
            oauth_token: "https://oauth2.googleapis.com/token".into(),
            tokeninfo: "https://oauth2.googleapis.com/tokeninfo".into(),
        }
    }
}

mod http {
    use crate::agentic_works::db::Result;
    use std::time::Duration;

    pub(crate) struct Reply {
        pub status: u16,
        pub body: String,
    }

    /// One request, off the async threads. `bearer` is sent as
    /// `Authorization`; nothing of it appears in an error.
    pub(crate) async fn send(
        method: &'static str,
        url: &str,
        bearer: Option<&str>,
        body: Option<(&'static str, Vec<u8>)>,
    ) -> Result<Reply> {
        let host = url
            .split("://")
            .nth(1)
            .and_then(|r| r.split('/').next())
            .unwrap_or_default()
            .to_owned();
        let mut builder = ureq::http::Request::builder().method(method).uri(url);
        if let Some(token) = bearer {
            builder = builder.header("authorization", format!("Bearer {token}"));
        }
        if let Some((kind, _)) = &body {
            builder = builder.header("content-type", *kind);
        }
        let request = builder
            .body(body.map(|b| b.1).unwrap_or_default())
            .map_err(|_| "The request could not be built.".to_owned())?;
        let unreachable = format!("Cannot reach {host}. Check the network.");
        tokio::task::spawn_blocking(move || {
            let agent: ureq::Agent = ureq::Agent::config_builder()
                .timeout_global(Some(Duration::from_secs(15)))
                .http_status_as_error(false)
                .max_redirects(0)
                .build()
                .into();
            let response = agent.run(request).map_err(|_| unreachable.clone())?;
            let status = response.status().as_u16();
            let body = response
                .into_body()
                .with_config()
                .limit(1 << 20)
                .read_to_string()
                .map_err(|_| unreachable)?;
            Ok(Reply { status, body })
        })
        .await
        .map_err(|_| "The request was interrupted.".to_owned())?
    }
}

/// The in-memory client key every ephemeral certificate is issued for, made
/// once per run (as Google's connectors do). It never touches disk.
struct ClientKey {
    pkcs8: Vec<u8>,
    public_pem: String,
}

fn make_key() -> Result<ClientKey> {
    use rsa::pkcs8::{EncodePrivateKey, EncodePublicKey, LineEnding};
    let key = rsa::RsaPrivateKey::new(&mut rand_core::OsRng, 2048)
        .map_err(|_| "Cannot make a client key.".to_owned())?;
    Ok(ClientKey {
        pkcs8: key
            .to_pkcs8_der()
            .map_err(|_| "Cannot encode the client key.".to_owned())?
            .as_bytes()
            .to_vec(),
        public_pem: key
            .to_public_key()
            .to_public_key_pem(LineEnding::LF)
            .map_err(|_| "Cannot encode the client key.".to_owned())?,
    })
}

/// An instance's settings and a client certificate for it.
#[derive(Clone)]
struct Material {
    settings: Arc<Settings>,
    cert: Vec<u8>,
    expires: SystemTime,
}

/// What is known of one instance's certificate.
#[derive(Default)]
struct Slot {
    current: Option<Material>,
    last_attempt: Option<Instant>,
    /// Why the last attempt failed, when it did.
    last_error: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
enum Plan {
    Use,
    /// Refresh; on failure use the current one (still usable).
    RefreshOrUse,
    /// Refresh; nothing usable meanwhile.
    Refresh,
    Fail(String),
}

impl Slot {
    fn plan(&self, now: SystemTime, at: Instant) -> Plan {
        let usable = self
            .current
            .as_ref()
            .is_some_and(|m| m.expires > now + USABLE_FOR);
        let fresh = self
            .current
            .as_ref()
            .is_some_and(|m| m.expires > now + REFRESH_BEFORE);
        let limited = self
            .last_attempt
            .is_some_and(|t| at.saturating_duration_since(t) < REFRESH_EVERY);
        match (usable, fresh, limited) {
            (true, true, _) | (true, false, true) => Plan::Use,
            (true, false, false) => Plan::RefreshOrUse,
            // Within 30 s of the last attempt nothing is fetched again,
            // even when that attempt never finished (it was cancelled).
            (false, _, true) => Plan::Fail(self.last_error.clone().unwrap_or_else(|| {
                "A certificate was requested less than 30 seconds ago. Try again in a moment."
                    .to_owned()
            })),
            (false, _, false) => Plan::Refresh,
        }
    }
}

/// Steps reported by Test connection, one line each.
pub(crate) type Report = Vec<String>;

/// One cache entry with its own lock, so one slow refresh never holds up
/// another key, and one key is never fetched twice at once.
type Entry<T> = Arc<tokio::sync::Mutex<T>>;

fn entry<T: Default>(map: &Mutex<HashMap<String, Entry<T>>>, key: &str) -> Entry<T> {
    map.lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(key.to_owned())
        .or_default()
        .clone()
}

/// Cloud SQL connections for the whole app: token and certificate caches.
pub(crate) struct Connector {
    endpoints: Endpoints,
    adc: Option<PathBuf>,
    port: u16,
    key: tokio::sync::OnceCell<Arc<ClientKey>>,
    tokens: Mutex<HashMap<String, Entry<Option<Token>>>>,
    certs: Mutex<HashMap<String, Entry<Slot>>>,
}

impl Default for Connector {
    fn default() -> Self {
        Self::new(Endpoints::default(), auth::adc_path(), PORT)
    }
}

/// A source with what identifies it in the caches (a digest, not the secret).
struct Identity {
    source: Source,
    id: String,
}

impl Identity {
    /// Parse `raw` and identify it by the same bytes.
    fn parse(raw: &str) -> Result<Self> {
        let digest = ring::digest::digest(&ring::digest::SHA256, raw.as_bytes());
        let id = digest
            .as_ref()
            .iter()
            .take(12)
            .map(|b| format!("{b:02x}"))
            .collect();
        Ok(Self {
            source: Source::from_json(raw)?,
            id,
        })
    }
}

impl Connector {
    pub(crate) fn new(endpoints: Endpoints, adc: Option<PathBuf>, port: u16) -> Self {
        Self {
            endpoints,
            adc,
            port,
            key: tokio::sync::OnceCell::new(),
            tokens: Mutex::default(),
            certs: Mutex::default(),
        }
    }

    fn identity(&self, credential: &GoogleCredential, key_json: Option<&str>) -> Result<Identity> {
        match credential {
            GoogleCredential::Key(_) => Identity::parse(key_json.ok_or(auth::NO_CREDENTIAL)?),
            GoogleCredential::Adc => {
                // Read once: what is parsed is what is identified.
                let path = self.adc.as_deref().ok_or(auth::NO_CREDENTIAL)?;
                let raw =
                    std::fs::read_to_string(path).map_err(|_| auth::NO_CREDENTIAL.to_owned())?;
                Identity::parse(&raw)
            }
        }
    }

    /// A token for `scopes`, from the cache while it has more than five
    /// minutes left.
    async fn token(&self, who: &Identity, scopes: &[&str]) -> Result<Token> {
        let slot = entry(&self.tokens, &format!("{}|{}", who.id, scopes.join(" ")));
        let mut cached = slot.lock().await;
        if let Some(t) = cached
            .as_ref()
            .filter(|t| t.fresh(SystemTime::now(), REFRESH_BEFORE))
        {
            return Ok(t.clone());
        }
        let token = auth::fetch(&self.endpoints, &who.source, scopes).await?;
        *cached = Some(token.clone());
        Ok(token)
    }

    async fn client_key(&self) -> Result<Arc<ClientKey>> {
        self.key
            .get_or_try_init(|| async {
                tokio::task::spawn_blocking(make_key)
                    .await
                    .map_err(|_| "Cannot make a client key.".to_owned())?
                    .map(Arc::new)
            })
            .await
            .cloned()
    }

    async fn fetch_material(
        &self,
        ctx: &admin::Context<'_>,
        admin_token: &Token,
        login: Option<&Token>,
    ) -> Result<Material> {
        let settings =
            admin::connect_settings(&self.endpoints.sqladmin, &admin_token.access, ctx).await?;
        let key = self.client_key().await?;
        let cert = admin::ephemeral_cert(
            &self.endpoints.sqladmin,
            &admin_token.access,
            &key.public_pem,
            login.map(|t| t.access.as_str()),
            ctx,
        )
        .await?;
        let issued = tls::not_after(&cert).ok_or("The ephemeral certificate cannot be read.")?;
        // A real certificate lasts an hour: one that is nearly over as issued
        // means this Mac's clock is off.
        if issued <= SystemTime::now() + REFRESH_BEFORE + REFRESH_EVERY {
            return Err(
                "The Admin API issued a certificate that expires too soon: check this Mac's clock."
                    .into(),
            );
        }
        // With IAM sign-in the certificate is good only while its token is
        // (the token cache keeps at least five minutes of it).
        let expires = login.map_or(issued, |l| issued.min(l.expires));
        Ok(Material {
            settings: Arc::new(settings),
            cert,
            expires,
        })
    }

    /// The instance's settings and a certificate with time left, refreshed
    /// as the plan says. `admin_token` is only needed to refresh: when it
    /// could not be had, that counts as a failed refresh.
    async fn material(
        &self,
        slot_key: &str,
        ctx: &admin::Context<'_>,
        admin_token: std::result::Result<&Token, &String>,
        login: Option<&Token>,
    ) -> Result<Material> {
        let slot = entry(&self.certs, slot_key);
        let mut slot = slot.lock().await;
        let plan = slot.plan(SystemTime::now(), Instant::now());
        if let Plan::Fail(why) = plan {
            return Err(why);
        }
        if plan == Plan::Use
            && let Some(m) = &slot.current
        {
            return Ok(m.clone());
        }
        slot.last_attempt = Some(Instant::now());
        // Kept if this attempt is cancelled before it ends.
        slot.last_error =
            Some("The last certificate request did not finish. Try again in a moment.".to_owned());
        let fetched = match admin_token {
            Ok(token) => self.fetch_material(ctx, token, login).await,
            Err(why) => Err(why.clone()),
        };
        match fetched {
            Ok(m) => {
                slot.last_error = None;
                slot.current = Some(m.clone());
                Ok(m)
            }
            Err(why) => {
                slot.last_error = Some(why.clone());
                match (&plan, &slot.current) {
                    (Plan::RefreshOrUse, Some(m)) => Ok(m.clone()),
                    _ => Err(why),
                }
            }
        }
    }

    /// Connect to `cfg` as Agentic Works does everywhere; `report` gets a
    /// line for each step that succeeded.
    #[cfg(test)]
    pub(crate) async fn connect(
        &self,
        cfg: &CloudSql,
        key_json: Option<&str>,
        password: Option<&str>,
        report: &mut Report,
    ) -> Result<Client> {
        self.connect_routed(cfg, key_json, password, report)
            .await
            .map(|(client, _)| client)
    }

    /// [`Self::connect`], with the way to that very server: where a
    /// cancellation of its statements is sent.
    pub(crate) async fn connect_routed(
        &self,
        cfg: &CloudSql,
        key_json: Option<&str>,
        password: Option<&str>,
        report: &mut Report,
    ) -> Result<(Client, Route)> {
        let name = InstanceName::parse(&cfg.connection_name)?;
        let connection = cfg.connection_name.trim();
        let who = self.identity(&cfg.credential, key_json)?;
        // The Admin API token, and with IAM sign-in a separate token scoped
        // to signing in only: that one is the database password.
        let admin_token = self.token(&who, &[auth::ADMIN_SCOPE]).await;
        let login = if cfg.iam_auth {
            Some(self.token(&who, &[auth::LOGIN_SCOPE]).await?)
        } else {
            None
        };
        let email = admin_token
            .as_ref()
            .ok()
            .or(login.as_ref())
            .and_then(|t| t.email.clone())
            .or_else(|| who.source.email())
            .unwrap_or_else(|| "your gcloud login".to_owned());
        if admin_token.is_ok() || login.is_some() {
            report.push(format!("✓ Credential loaded: {email}"));
        }
        let ctx = admin::Context {
            email: &email,
            name: &name,
            connection,
        };
        let slot = format!("{connection}|{}|{}", who.id, cfg.iam_auth);
        let material = self
            .material(&slot, &ctx, admin_token.as_ref(), login.as_ref())
            .await?;
        let host = material.settings.address(cfg.ip_type)?;
        report.push(format!(
            "✓ Instance found: {} at {host}",
            material.settings.version
        ));
        let left = material
            .expires
            .duration_since(SystemTime::now())
            .unwrap_or_default();
        report.push(format!(
            "✓ Ephemeral certificate issued ({} minutes left)",
            left.as_secs() / 60
        ));
        let key = self.client_key().await?;
        let (config, server_name) = tls::client_config(
            &material.settings,
            &name.server_cn(),
            &material.cert,
            &key.pkcs8,
            &host,
        )?;
        let route = Route {
            host: host.clone(),
            port: self.port,
            tls: Arc::new(config),
            server_name: server_name.clone(),
        };
        let tcp = timeout(Duration::from_secs(5), TcpStream::connect((host.as_str(), self.port)))
            .await
            .map_err(|_| format!("Cannot reach {host}:{} (timed out). A private address needs this Mac on the instance's network.", self.port))?
            .map_err(|_| format!("Cannot reach {host}:{}. A private address needs this Mac on the instance's network.", self.port))?;
        let stream = timeout(
            Duration::from_secs(5),
            tokio_rustls::TlsConnector::from(Arc::clone(&route.tls)).connect(server_name, tcp),
        )
        .await
        .map_err(|_| "The TLS handshake timed out.".to_owned())?
        .map_err(|e| tls::handshake_error(&e, connection))?;
        report.push("✓ TLS to the server verified".to_owned());
        let user = match cfg.user.trim() {
            // The identity's email, wherever it is known: a login-only token
            // may carry none.
            "" if cfg.iam_auth => login
                .as_ref()
                .and_then(|t| t.email.clone())
                .or_else(|| admin_token.as_ref().ok().and_then(|t| t.email.clone()))
                .or_else(|| who.source.email())
                .map(|e| auth::iam_user(&e))
                .ok_or("Enter the database user.")?,
            "" => return Err("Enter the database user.".into()),
            user => user.to_owned(),
        };
        let secret = if let Some(login) = &login {
            login.access.clone()
        } else {
            password
                .filter(|p| !p.is_empty())
                .ok_or("Enter the database password, or turn on Sign in to the database with IAM.")?
                .to_owned()
        };
        let mut pg = Config::new();
        pg.user(&user)
            .dbname(cfg.database.trim())
            .password(secret)
            .application_name("connecto")
            .ssl_mode(SslMode::Disable);
        // Port 3307 is Google's server-side proxy: PostgreSQL is spoken
        // straight inside the TLS stream, with no SSLRequest.
        let (client, conn) = timeout(Duration::from_secs(5), pg.connect_raw(stream, NoTls))
            .await
            .map_err(|_| "Signing in to PostgreSQL timed out.".to_owned())?
            .map_err(|e| login_error(&e, &user, cfg))?;
        tokio::spawn(async move {
            let _ = conn.await;
        });
        db::init(&client).await?;
        report.push(format!("✓ Logged in as {user}"));
        Ok((client, route))
    }
}

/// The way to the server a connection was made to: its address, as
/// resolved then, and the TLS that proved it the instance.
#[derive(Clone)]
pub(crate) struct Route {
    host: String,
    port: u16,
    tls: Arc<rustls::ClientConfig>,
    server_name: rustls::pki_types::ServerName<'static>,
}

impl Route {
    /// A new stream to that same server, as the connection was made.
    pub(crate) async fn stream(&self) -> Result<tokio_rustls::client::TlsStream<TcpStream>> {
        let tcp = timeout(
            Duration::from_secs(5),
            TcpStream::connect((self.host.as_str(), self.port)),
        )
        .await
        .map_err(|_| "Cannot reach the database server (timed out).".to_owned())?
        .map_err(|_| "Cannot reach the database server.".to_owned())?;
        timeout(
            Duration::from_secs(5),
            tokio_rustls::TlsConnector::from(Arc::clone(&self.tls))
                .connect(self.server_name.clone(), tcp),
        )
        .await
        .map_err(|_| "The TLS handshake timed out.".to_owned())?
        .map_err(|e| format!("TLS to the database server failed: {e}"))
    }
}

fn login_error(err: &tokio_postgres::Error, user: &str, cfg: &CloudSql) -> String {
    let code = err.code().map(tokio_postgres::error::SqlState::code);
    match code {
        Some("28P01") if cfg.iam_auth => format!(
            "IAM sign-in failed for {user}: check that the instance has cloudsql.iam_authentication=on and the IAM user exists."
        ),
        Some("28P01") => format!("Password authentication failed for {user}."),
        Some("28000") if cfg.iam_auth => format!(
            "The IAM database user {user} does not exist: create it with `gcloud sql users create … --type=cloud_iam_service_account`."
        ),
        Some("28000") => format!("The database refused the user {user}."),
        Some("3D000") => format!("The database {} does not exist.", cfg.database.trim()),
        Some(other) => format!("PostgreSQL refused the sign-in ({other})."),
        None => "The connection closed while signing in to PostgreSQL.".into(),
    }
}
