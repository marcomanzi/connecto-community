//! Mutual TLS to the instance's server-side proxy (port 3307): rustls,
//! TLS 1.3 only, the instance's own CA as the only root.

use super::admin::{CaMode, Settings};
use crate::agentic_works::db::Result;
use rustls::{
    CertificateError, ClientConfig, DigitallySignedStruct, Error, RootCertStore, SignatureScheme,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    crypto::CryptoProvider,
    pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime},
    server::ParsedCertificate,
};
use std::{
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// The subject common name of a certificate.
pub(super) fn common_name(der: &[u8]) -> Option<String> {
    let (_, cert) = x509_parser::parse_x509_certificate(der).ok()?;
    let cn = cert.subject().iter_common_name().next()?;
    cn.as_str().ok().map(str::to_owned)
}

/// The DNS names a certificate's SAN holds, lowercase, without a final dot
/// (Cloud SQL CAS certificates may end theirs with one, which webpki's own
/// name check refuses).
pub(super) fn dns_names(der: &[u8]) -> Vec<String> {
    let Ok((_, cert)) = x509_parser::parse_x509_certificate(der) else {
        return Vec::new();
    };
    let Ok(Some(san)) = cert.subject_alternative_name() else {
        return Vec::new();
    };
    san.value
        .general_names
        .iter()
        .filter_map(|n| match n {
            x509_parser::extensions::GeneralName::DNSName(d) => {
                Some(d.trim_end_matches('.').to_ascii_lowercase())
            }
            _ => None,
        })
        .collect()
}

/// What names the instance's server certificate.
#[derive(Debug, Clone)]
pub(super) enum Expect {
    /// `GOOGLE_MANAGED_INTERNAL_CA`: the subject CN is `project:instance`.
    Cn(String),
    /// CAS modes: a SAN DNS name is one of the instance's DNS names.
    Dns(Vec<String>),
}

/// When a certificate stops being valid.
pub(super) fn not_after(der: &[u8]) -> Option<SystemTime> {
    let (_, cert) = x509_parser::parse_x509_certificate(der).ok()?;
    let secs = u64::try_from(cert.validity().not_after.timestamp()).ok()?;
    Some(UNIX_EPOCH + Duration::from_secs(secs))
}

/// Checks the chain against the instance's CA (validity dates included),
/// then that the certificate names the instance: its CN, or for a CAS CA
/// one of its DNS names.
#[derive(Debug)]
pub(super) struct InstanceVerifier {
    pub roots: RootCertStore,
    pub expect: Expect,
    pub provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for InstanceVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, Error> {
        let cert = ParsedCertificate::try_from(end_entity)?;
        rustls::client::verify_server_cert_signed_by_trust_anchor(
            &cert,
            &self.roots,
            intermediates,
            now,
            self.provider.signature_verification_algorithms.all,
        )?;
        let named = match &self.expect {
            Expect::Cn(cn) => common_name(end_entity).as_deref() == Some(cn.as_str()),
            Expect::Dns(names) => dns_names(end_entity).iter().any(|d| names.contains(d)),
        };
        if !named {
            return Err(Error::InvalidCertificate(CertificateError::NotValidForName));
        }
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// The client configuration for one instance, and the name to verify.
pub(super) fn client_config(
    settings: &Settings,
    server_cn: &str,
    cert: &[u8],
    key_pkcs8: &[u8],
    host: &str,
) -> Result<(ClientConfig, ServerName<'static>)> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut roots = RootCertStore::empty();
    for der in &settings.ca {
        roots
            .add(CertificateDer::from(der.clone()))
            .map_err(|_| "The instance's server CA certificate cannot be read.".to_owned())?;
    }
    let (expect, name) = match settings.ca_mode {
        // Not checked by rustls (the verifier checks the CN); an address
        // sends no SNI.
        CaMode::Internal => (
            Expect::Cn(server_cn.to_owned()),
            ServerName::try_from(host.to_owned())
                .map_err(|_| format!("{host} is not a valid address."))?,
        ),
        CaMode::Cas => {
            let sni = settings
                .dns_names
                .first()
                .cloned()
                .ok_or("The instance has no DNS name to verify.")?;
            (
                Expect::Dns(settings.dns_names.clone()),
                ServerName::try_from(sni)
                    .map_err(|_| "The instance's DNS name is not valid.".to_owned())?,
            )
        }
    };
    let verifier: Arc<dyn ServerCertVerifier> = Arc::new(InstanceVerifier {
        roots,
        expect,
        provider: provider.clone(),
    });
    let config = ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|_| "TLS 1.3 is not available.".to_owned())?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_client_auth_cert(
            vec![CertificateDer::from(cert.to_vec())],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_pkcs8.to_vec())),
        )
        .map_err(|_| "The ephemeral certificate does not match its key.".to_owned())?;
    Ok((config, name))
}

/// A failed handshake in plain words.
pub(super) fn handshake_error(err: &std::io::Error, connection: &str) -> String {
    let tls = err.get_ref().and_then(|e| e.downcast_ref::<Error>());
    match tls {
        Some(Error::InvalidCertificate(
            CertificateError::NotValidForName | CertificateError::NotValidForNameContext { .. },
        )) => format!("The server is not `{connection}`."),
        Some(Error::InvalidCertificate(
            CertificateError::Expired | CertificateError::ExpiredContext { .. },
        )) => "The server's certificate has expired.".into(),
        Some(Error::InvalidCertificate(_)) => format!(
            "The server's certificate is not signed by the CA of `{connection}`: TLS not verified."
        ),
        Some(Error::AlertReceived(alert)) => {
            format!("The server refused the client certificate ({alert:?}). Try again in a minute.")
        }
        Some(other) => format!("TLS to the server failed: {other}."),
        None => format!("TLS to the server failed: {}.", err.kind()),
    }
}
