//! The two Cloud SQL Admin API calls a connection needs: the instance's
//! connect settings, and an ephemeral client certificate.

use super::{auth::pem_blocks, http};
use crate::agentic_works::db::Result;
use connecto_core::agentic_works::{InstanceName, IpType};
use serde_json::{Value, json};

/// How the instance's server certificate is named.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CaMode {
    /// `GOOGLE_MANAGED_INTERNAL_CA`: subject CN `project:instance`, no SAN.
    Internal,
    /// `GOOGLE_MANAGED_CAS_CA` / `CUSTOMER_MANAGED_CAS_CA`: the DNS name.
    Cas,
}

/// What `connectSettings` says about an instance.
#[derive(Clone, Debug)]
pub(super) struct Settings {
    pub version: String,
    pub public_ip: Option<String>,
    pub private_ip: Option<String>,
    pub psc: bool,
    /// The instance's Private Service Connect DNS name.
    pub psc_dns: Option<String>,
    /// Every DNS name the instance answers to (a CAS certificate names one).
    pub dns_names: Vec<String>,
    /// The DER of each certificate in `serverCaCert`.
    pub ca: Vec<Vec<u8>>,
    pub ca_mode: CaMode,
}

impl Settings {
    /// The host to connect to for `ip`, or why there is none.
    pub fn address(&self, ip: IpType) -> Result<String> {
        match ip {
            IpType::Public => self.public_ip.clone().ok_or_else(|| {
                "The instance has no public IP; choose Private or Private Service Connect.".into()
            }),
            IpType::Private => self.private_ip.clone().ok_or_else(|| {
                "The instance has no private IP; choose Public or Private Service Connect.".into()
            }),
            IpType::Psc => self.psc_dns.clone().filter(|_| self.psc).ok_or_else(|| {
                "The instance does not use Private Service Connect; choose Public or Private."
                    .into()
            }),
        }
    }
}

/// Who is asking and about what, for the messages.
pub(super) struct Context<'a> {
    pub email: &'a str,
    pub name: &'a InstanceName,
    pub connection: &'a str,
}

/// An Admin API refusal in plain words. The body is Google's error JSON; only
/// its reason codes are read, and nothing of a credential is in it.
pub(super) fn api_error(status: u16, body: &str, ctx: &Context<'_>) -> String {
    let disabled = body.contains("SERVICE_DISABLED") || body.contains("accessNotConfigured");
    if disabled {
        return format!(
            "Enable the Cloud SQL Admin API: `gcloud services enable sqladmin.googleapis.com --project={}`.",
            ctx.name.project
        );
    }
    match status {
        401 => format!(
            "Google did not accept the credential of `{}`: import the key again, or sign in again with gcloud.",
            ctx.email
        ),
        403 => format!(
            "`{}` needs the role Cloud SQL Client (`roles/cloudsql.client`) on `{}`.",
            ctx.email, ctx.name.project
        ),
        404 => format!(
            "Instance `{}` not found (check project, region and name).",
            ctx.connection
        ),
        _ => {
            // Only Google's status word: the message itself may echo the
            // request, which can hold a token.
            let v: Value = serde_json::from_str(body).unwrap_or_default();
            let word = v
                .pointer("/error/status")
                .and_then(Value::as_str)
                .filter(|w| w.len() <= 40 && w.chars().all(|c| c.is_ascii_uppercase() || c == '_'))
                .map(|w| format!(" ({w})"))
                .unwrap_or_default();
            format!("The Cloud SQL Admin API answered {status}{word}. Try again later.")
        }
    }
}

fn instance_url(base: &str, name: &InstanceName) -> String {
    format!(
        "{}/sql/v1beta4/projects/{}/instances/{}",
        base.trim_end_matches('/'),
        name.project,
        name.instance
    )
}

/// Parse a `connectSettings` answer, refusing what Agentic Works cannot use.
pub(super) fn parse_settings(body: &str, ctx: &Context<'_>) -> Result<Settings> {
    let v: Value =
        serde_json::from_str(body).map_err(|_| "The Admin API's answer is not JSON.".to_owned())?;
    let s = |p: &str| v.pointer(p).and_then(Value::as_str).map(str::to_owned);
    if let Some(region) = s("/region")
        && region != ctx.name.region
    {
        return Err(format!(
            "Instance `{}` is in region {region}, not {}: check the connection name.",
            ctx.connection, ctx.name.region
        ));
    }
    let version = s("/databaseVersion").unwrap_or_default();
    if !version.starts_with("POSTGRES_") {
        return Err(format!(
            "`{}` is a {} instance; Agentic Works needs Cloud SQL for PostgreSQL.",
            ctx.connection,
            if version.is_empty() {
                "non-PostgreSQL"
            } else {
                &version
            }
        ));
    }
    let ip = |kind: &str| {
        v.get("ipAddresses")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .find(|a| a.get("type").and_then(Value::as_str) == Some(kind))
            .and_then(|a| a.get("ipAddress").and_then(Value::as_str))
            .map(str::to_owned)
    };
    let ca = pem_blocks(&s("/serverCaCert/cert").unwrap_or_default(), "CERTIFICATE");
    if ca.is_empty() {
        return Err("The Admin API gave no server CA certificate for the instance.".into());
    }
    let ca_mode = match s("/serverCaMode").as_deref() {
        None | Some("" | "GOOGLE_MANAGED_INTERNAL_CA" | "CA_MODE_UNSPECIFIED") => CaMode::Internal,
        Some("GOOGLE_MANAGED_CAS_CA" | "CUSTOMER_MANAGED_CAS_CA") => CaMode::Cas,
        Some(other) => {
            return Err(format!(
                "The instance's server CA mode {other} is not supported."
            ));
        }
    };
    // `dnsNames` (newer) lists each name with its kind; `dnsName` is the
    // older single name. As Google's connectors: an instance-scoped PSC
    // entry first, then `dnsName`.
    let clean = |d: &str| d.trim_end_matches('.').to_ascii_lowercase();
    let listed: Vec<(String, String, String)> = v
        .get("dnsNames")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|e| {
            let name = e.get("name").and_then(Value::as_str)?;
            let kind = e
                .get("connectionType")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let scope = e
                .get("dnsScope")
                .and_then(Value::as_str)
                .unwrap_or_default();
            Some((clean(name), kind.to_owned(), scope.to_owned()))
        })
        .filter(|(n, ..)| !n.is_empty())
        .collect();
    let legacy = s("/dnsName").map(|d| clean(&d)).filter(|d| !d.is_empty());
    let psc_dns = listed
        .iter()
        .find(|(_, kind, scope)| kind == "PRIVATE_SERVICE_CONNECT" && scope == "INSTANCE")
        .map(|(n, ..)| n.clone())
        .or_else(|| legacy.clone());
    // Only the instance's own names identify it: a CLUSTER-scoped write
    // endpoint can move to another instance on failover.
    let mut dns_names: Vec<String> = listed
        .into_iter()
        .filter(|(_, _, scope)| scope == "INSTANCE")
        .map(|(n, ..)| n)
        .collect();
    dns_names.extend(legacy);
    dns_names.sort();
    dns_names.dedup();
    if ca_mode == CaMode::Cas && dns_names.is_empty() {
        return Err("The instance uses a CAS server CA but has no DNS name.".into());
    }
    Ok(Settings {
        version,
        public_ip: ip("PRIMARY"),
        private_ip: ip("PRIVATE"),
        psc: v
            .get("pscEnabled")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        psc_dns,
        dns_names,
        ca,
        ca_mode,
    })
}

pub(super) async fn connect_settings(
    base: &str,
    token: &str,
    ctx: &Context<'_>,
) -> Result<Settings> {
    let url = format!("{}/connectSettings", instance_url(base, ctx.name));
    let reply = http::send("GET", &url, Some(token), None).await?;
    if reply.status != 200 {
        return Err(api_error(reply.status, &reply.body, ctx));
    }
    parse_settings(&reply.body, ctx)
}

/// A client certificate for `public_pem` (DER). With IAM database sign-in,
/// `login` is the token the certificate is bound to.
pub(super) async fn ephemeral_cert(
    base: &str,
    token: &str,
    public_pem: &str,
    login: Option<&str>,
    ctx: &Context<'_>,
) -> Result<Vec<u8>> {
    let url = format!("{}:generateEphemeralCert", instance_url(base, ctx.name));
    let mut body = json!({ "public_key": public_pem });
    if let Some(login) = login {
        body["access_token"] = json!(login);
    }
    let reply = http::send(
        "POST",
        &url,
        Some(token),
        Some(("application/json", body.to_string().into_bytes())),
    )
    .await?;
    if reply.status != 200 {
        return Err(api_error(reply.status, &reply.body, ctx));
    }
    let v: Value = serde_json::from_str(&reply.body)
        .map_err(|_| "The Admin API's answer is not JSON.".to_owned())?;
    let pem = v
        .pointer("/ephemeralCert/cert")
        .and_then(Value::as_str)
        .ok_or("The Admin API gave no ephemeral certificate.")?;
    pem_blocks(pem, "CERTIFICATE")
        .into_iter()
        .next()
        .ok_or_else(|| "The ephemeral certificate is not PEM.".into())
}
