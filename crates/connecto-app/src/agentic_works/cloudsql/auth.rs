//! Google access tokens: a service account key (a signed JWT exchanged at
//! its `token_uri`), or the person's gcloud Application Default Credentials.
//! Nothing here is ever logged or put in a message: no token, no key.

use super::{Endpoints, http};
use crate::agentic_works::db::Result;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// The Cloud SQL Admin API.
pub(super) const ADMIN_SCOPE: &str = "https://www.googleapis.com/auth/sqlservice.admin";
/// Signing in to the database with IAM.
pub(super) const LOGIN_SCOPE: &str = "https://www.googleapis.com/auth/sqlservice.login";
/// What an impersonating source token needs.
const CLOUD_PLATFORM: &str = "https://www.googleapis.com/auth/cloud-platform";
const EMAIL_SCOPE: &str = "https://www.googleapis.com/auth/userinfo.email";
const JWT_BEARER: &str = "urn:ietf:params:oauth:grant-type:jwt-bearer";

pub(super) const NO_CREDENTIAL: &str =
    "Import a service account key, or run `gcloud auth application-default login`.";

/// A service account key, parsed. `key` is the PKCS#8 DER of its private key.
#[derive(Clone)]
pub(super) struct ServiceAccount {
    pub email: String,
    pub key_id: Option<String>,
    pub key: Vec<u8>,
    pub token_uri: String,
}

/// Where a token comes from.
#[derive(Clone)]
pub(super) enum Source {
    ServiceAccount(ServiceAccount),
    /// `gcloud auth application-default login`.
    User {
        client_id: String,
        client_secret: String,
        refresh_token: String,
    },
    /// A service account impersonated with the source's token.
    Impersonated {
        url: String,
        source: Box<Source>,
        delegates: Vec<String>,
    },
}

/// An access token and who it belongs to. No Debug: it would print the token.
#[derive(Clone)]
pub(super) struct Token {
    pub access: String,
    pub expires: SystemTime,
    pub email: Option<String>,
}

impl Token {
    /// Whether it still has more than `margin` to live at `now`.
    pub fn fresh(&self, now: SystemTime, margin: Duration) -> bool {
        self.expires > now + margin
    }
}

fn text<'a>(v: &'a Value, field: &str) -> Option<&'a str> {
    v.get(field)
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
}

/// The DER blocks of `label` in PEM text, in order.
pub(super) fn pem_blocks(pem: &str, label: &str) -> Vec<Vec<u8>> {
    let begin = format!("-----BEGIN {label}-----");
    let end = format!("-----END {label}-----");
    let mut out = Vec::new();
    let mut rest = pem;
    while let Some(start) = rest.find(&begin) {
        let after = &rest[start + begin.len()..];
        let Some(stop) = after.find(&end) else { break };
        let body: String = after[..stop]
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        if let Ok(der) = base64::engine::general_purpose::STANDARD.decode(body) {
            out.push(der);
        }
        rest = &after[stop + end.len()..];
    }
    out
}

impl Source {
    /// A credential file: a service account key, or one of the ADC kinds.
    pub fn from_json(json: &str) -> Result<Self> {
        let v: Value = serde_json::from_str(json)
            .map_err(|_| "The Google credential is not valid JSON.".to_owned())?;
        Self::from_value(&v, true)
    }

    fn from_value(v: &Value, outer: bool) -> Result<Self> {
        match text(v, "type").unwrap_or_default() {
            "service_account" => {
                let email = text(v, "client_email").ok_or("The key has no client_email.")?;
                let pem = text(v, "private_key").ok_or("The key has no private_key.")?;
                let key = pem_blocks(pem, "PRIVATE KEY")
                    .into_iter()
                    .next()
                    .ok_or("The key's private_key is not a PKCS#8 PEM key.")?;
                ring::signature::RsaKeyPair::from_pkcs8(&key)
                    .map_err(|_| "The key's private_key is not a usable RSA key.")?;
                let token_uri = text(v, "token_uri")
                    .unwrap_or("https://oauth2.googleapis.com/token")
                    .to_owned();
                if !token_uri_ok(&token_uri) {
                    return Err(
                        "The key's token_uri is not Google's token endpoint; Connecto will not send it there."
                            .into(),
                    );
                }
                Ok(Self::ServiceAccount(ServiceAccount {
                    email: email.to_owned(),
                    key_id: text(v, "private_key_id").map(str::to_owned),
                    key,
                    token_uri,
                }))
            }
            "authorized_user" => Ok(Self::User {
                client_id: text(v, "client_id")
                    .ok_or("Your gcloud login has no client_id: sign in again.")?
                    .to_owned(),
                client_secret: text(v, "client_secret")
                    .ok_or("Your gcloud login has no client_secret: sign in again.")?
                    .to_owned(),
                refresh_token: text(v, "refresh_token")
                    .ok_or("Your gcloud login has no refresh_token: sign in again.")?
                    .to_owned(),
            }),
            "impersonated_service_account" if outer => {
                let url = text(v, "service_account_impersonation_url")
                    .ok_or("The impersonation login has no service_account_impersonation_url.")?;
                if !impersonation_url_ok(url) {
                    return Err("The impersonation login names an address that is not Google's IAM Credentials API; Connecto will not send a token there.".into());
                }
                let source = v
                    .get("source_credentials")
                    .ok_or("The impersonation login has no source_credentials.")?;
                Ok(Self::Impersonated {
                    url: url.to_owned(),
                    source: Box::new(Self::from_value(source, false)?),
                    delegates: v
                        .get("delegates")
                        .and_then(Value::as_array)
                        .map(|d| {
                            d.iter()
                                .filter_map(Value::as_str)
                                .map(str::to_owned)
                                .collect()
                        })
                        .unwrap_or_default(),
                })
            }
            other => Err(format!(
                "A Google credential of type {:?} is not supported: import a service account key instead.",
                other.chars().take(40).collect::<String>()
            )),
        }
    }

    /// The identity's email, when the file says it.
    pub fn email(&self) -> Option<String> {
        match self {
            Self::ServiceAccount(sa) => Some(sa.email.clone()),
            Self::User { .. } => None,
            Self::Impersonated { url, .. } => url
                .rsplit_once("/serviceAccounts/")
                .and_then(|(_, tail)| tail.split(':').next())
                .filter(|e| e.contains('@'))
                .map(str::to_owned),
        }
    }
}

/// A local mock stands in for Google in this crate's own tests only.
#[cfg(test)]
fn loopback(url: &str) -> bool {
    url.strip_prefix("http://127.0.0.1:").is_some()
}
#[cfg(not(test))]
fn loopback(_: &str) -> bool {
    false
}

fn token_uri_ok(url: &str) -> bool {
    connecto_vault::google_token_uri_ok(url) || loopback(url)
}

/// `https://iamcredentials.googleapis.com/v1/projects/-/serviceAccounts/<email>:generateAccessToken`.
pub(super) fn impersonation_url_ok(url: &str) -> bool {
    let account = url
        .strip_prefix("https://iamcredentials.googleapis.com/v1/projects/-/serviceAccounts/")
        .or_else(|| {
            url.strip_prefix("http://127.0.0.1:")
                .filter(|_| loopback(url))
                .and_then(|r| r.split_once("/v1/projects/-/serviceAccounts/"))
                .map(|(_, a)| a)
        })
        .and_then(|r| r.strip_suffix(":generateAccessToken"));
    account.is_some_and(|a| {
        a.contains('@')
            && a.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '@' | '.' | '-' | '_'))
    })
}

/// The Application Default Credentials file gcloud writes. Read explicitly:
/// an app opened from the Finder does not get `GOOGLE_APPLICATION_CREDENTIALS`.
pub(super) fn adc_path() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("APPDATA")
            .map(|d| PathBuf::from(d).join("gcloud/application_default_credentials.json"))
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("HOME")
            .map(|h| PathBuf::from(h).join(".config/gcloud/application_default_credentials.json"))
    }
}

/// The signed assertion a service account exchanges for a token.
pub(super) fn jwt(sa: &ServiceAccount, scopes: &[&str], now: SystemTime) -> Result<String> {
    let iat = now
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "The clock is before 1970.")?
        .as_secs();
    let mut header = json!({ "alg": "RS256", "typ": "JWT" });
    if let Some(kid) = &sa.key_id {
        header["kid"] = json!(kid);
    }
    let claims = json!({
        "iss": sa.email,
        "scope": scopes.join(" "),
        "aud": sa.token_uri,
        "iat": iat,
        "exp": iat + 3600,
    });
    let signing_input = format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(header.to_string()),
        URL_SAFE_NO_PAD.encode(claims.to_string())
    );
    let pair = ring::signature::RsaKeyPair::from_pkcs8(&sa.key)
        .map_err(|_| "The key's private_key is not a usable RSA key.")?;
    let mut signature = vec![0; pair.public().modulus_len()];
    pair.sign(
        &ring::signature::RSA_PKCS1_SHA256,
        &ring::rand::SystemRandom::new(),
        signing_input.as_bytes(),
        &mut signature,
    )
    .map_err(|_| "Cannot sign with the service account key.")?;
    Ok(format!(
        "{signing_input}.{}",
        URL_SAFE_NO_PAD.encode(signature)
    ))
}

/// Form-encode pairs.
fn form(pairs: &[(&str, &str)]) -> Vec<u8> {
    let enc = |s: &str| {
        let mut out = String::with_capacity(s.len());
        for b in s.bytes() {
            if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
                out.push(char::from(b));
            } else {
                out.push_str(&format!("%{b:02X}"));
            }
        }
        out
    };
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", enc(k), enc(v)))
        .collect::<Vec<_>>()
        .join("&")
        .into_bytes()
}

const FORM: &str = "application/x-www-form-urlencoded";

/// What a token endpoint refused, in plain words.
fn refused(source: &Source, status: u16, body: &str) -> String {
    let v: Value = serde_json::from_str(body).unwrap_or_default();
    let code = text(&v, "error").unwrap_or_default();
    match source {
        Source::User { .. } if code == "invalid_grant" || code == "invalid_rapt" => {
            "Your gcloud login has expired or was revoked: run `gcloud auth application-default login` again.".into()
        }
        Source::ServiceAccount(sa) if code == "invalid_grant" => format!(
            "Google refused the key of {} (deleted, disabled, or this Mac's clock is wrong): create a new key and import it.",
            sa.email
        ),
        _ => format!(
            "Google refused the credential ({status}{}).",
            if code.is_empty() {
                String::new()
            } else {
                format!(", {}", code.chars().take(40).collect::<String>())
            }
        ),
    }
}

fn parse_token(body: &str, email: Option<String>, now: SystemTime) -> Result<Token> {
    let v: Value = serde_json::from_str(body).map_err(|_| "Google's token answer is not JSON.")?;
    let access = text(&v, "access_token").ok_or("Google's token answer has no access_token.")?;
    let ttl = v.get("expires_in").and_then(Value::as_u64).unwrap_or(3600);
    Ok(Token {
        access: access.to_owned(),
        expires: now + Duration::from_secs(ttl),
        email,
    })
}

/// A new token from `source` for `scopes`.
pub(super) async fn fetch(
    endpoints: &Endpoints,
    source: &Source,
    scopes: &[&str],
) -> Result<Token> {
    let now = SystemTime::now();
    match source {
        Source::ServiceAccount(sa) => {
            let assertion = jwt(sa, scopes, now)?;
            let reply = http::send(
                "POST",
                &sa.token_uri,
                None,
                Some((
                    FORM,
                    form(&[("grant_type", JWT_BEARER), ("assertion", &assertion)]),
                )),
            )
            .await?;
            if reply.status != 200 {
                return Err(refused(source, reply.status, &reply.body));
            }
            parse_token(&reply.body, Some(sa.email.clone()), now)
        }
        Source::User {
            client_id,
            client_secret,
            refresh_token,
        } => {
            let reply = http::send(
                "POST",
                &endpoints.oauth_token,
                None,
                Some((
                    FORM,
                    form(&[
                        ("grant_type", "refresh_token"),
                        ("client_id", client_id),
                        ("client_secret", client_secret),
                        ("refresh_token", refresh_token),
                        // A gcloud login grants cloud-platform (which covers
                        // the Admin API) and sqlservice.login, not
                        // sqlservice.admin: ask for a granted one.
                        ("scope", &user_scopes(scopes)),
                    ]),
                )),
            )
            .await?;
            if reply.status != 200 {
                return Err(refused(source, reply.status, &reply.body));
            }
            let mut token = parse_token(&reply.body, None, now)?;
            token.email = user_email(endpoints, &token.access).await;
            Ok(token)
        }
        Source::Impersonated {
            url,
            source: inner,
            delegates,
        } => {
            let base = Box::pin(fetch(endpoints, inner, &[CLOUD_PLATFORM])).await?;
            let body = json!({ "scope": scopes, "delegates": delegates, "lifetime": "3600s" });
            let reply = http::send(
                "POST",
                url,
                Some(&base.access),
                Some(("application/json", body.to_string().into_bytes())),
            )
            .await?;
            if reply.status != 200 {
                return Err(match reply.status {
                    403 => format!(
                        "Your gcloud login may not impersonate {}: it needs the role Service Account Token Creator on it.",
                        source
                            .email()
                            .unwrap_or_else(|| "that service account".into())
                    ),
                    _ => refused(source, reply.status, &reply.body),
                });
            }
            let v: Value = serde_json::from_str(&reply.body)
                .map_err(|_| "Google's impersonation answer is not JSON.")?;
            let access =
                text(&v, "accessToken").ok_or("Google's impersonation answer has no token.")?;
            let expires = text(&v, "expireTime")
                .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
                .and_then(|t| u64::try_from(t.timestamp()).ok())
                .map_or(now + Duration::from_secs(3600), |s| {
                    UNIX_EPOCH + Duration::from_secs(s)
                });
            Ok(Token {
                access: access.to_owned(),
                expires,
                email: source.email(),
            })
        }
    }
}

/// What a gcloud login is asked for: the database login scope as it is,
/// the Admin API through cloud-platform.
/// Both add userinfo.email (which gcloud also grants) so the token's email
/// can be looked up: it is the default IAM database user.
pub(super) fn user_scopes(scopes: &[&str]) -> String {
    scopes
        .iter()
        .map(|s| if *s == ADMIN_SCOPE { CLOUD_PLATFORM } else { s })
        .chain([EMAIL_SCOPE])
        .collect::<Vec<_>>()
        .join(" ")
}

/// The email of a user token, best effort (for messages only).
async fn user_email(endpoints: &Endpoints, access: &str) -> Option<String> {
    let reply = http::send(
        "POST",
        &endpoints.tokeninfo,
        None,
        Some((FORM, form(&[("access_token", access)]))),
    )
    .await
    .ok()?;
    let v: Value = serde_json::from_str(&reply.body).ok()?;
    text(&v, "email").map(str::to_owned)
}

/// The IAM database user of an identity: a service account's email without
/// `.gserviceaccount.com`, a person's email as it is.
pub(super) fn iam_user(email: &str) -> String {
    email
        .strip_suffix(".gserviceaccount.com")
        .unwrap_or(email)
        .to_owned()
}
