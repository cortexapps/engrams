//! Native Cloud SQL connect protocol (issue #1201 redesign).
//!
//! The protocol has three steps: fetch the instance's connect settings
//! (addresses + server CA) from sqladmin, mint an ephemeral client
//! certificate for a locally generated RSA key (the LOGIN token rides in
//! the request, so the certificate carries the IAM database identity),
//! then open TLS to the instance's server-side proxy on `:3307` with that
//! certificate as client auth. PostgreSQL bytes flow through the TLS
//! stream verbatim.
//!
//! This crate replaces the `cloud-sql-proxy` child process. The tokens
//! are plain inputs with expiries the caller already knows, so there is
//! no token source, no refresh cache, and no rate limiter — the class of
//! bug behind #1201's 30-second connect tail cannot recur here. An
//! endpoint is immutable after build; the caller (the egress tunnel
//! pool) rotates whole endpoints before their `stale_after`.

use std::net::IpAddr;
use std::sync::Arc;

use chrono::{DateTime, TimeZone, Utc};
use rsa::pkcs8::{EncodePrivateKey, EncodePublicKey};
use rustls::pki_types::{PrivateKeyDer, ServerName};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use x509_parser::prelude::{FromDer, X509Certificate};

pub mod api;
pub mod tls;

/// The instance's server-side proxy port.
pub const SERVER_PROXY_PORT: u16 = 3307;
const CONNECT_BUDGET: std::time::Duration = std::time::Duration::from_secs(30);

#[derive(Debug)]
pub enum Error {
    /// The instance connection name or the API response contradicts the
    /// caller's configuration.
    Config(String),
    /// A sqladmin call failed.
    Api(String),
    /// Certificate or TLS-configuration handling failed.
    Tls(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Config(m) => write!(f, "cloud sql config: {m}"),
            Self::Api(m) => write!(f, "cloud sql api: {m}"),
            Self::Tls(m) => write!(f, "cloud sql tls: {m}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<Error> for std::io::Error {
    fn from(error: Error) -> Self {
        std::io::Error::other(error.to_string())
    }
}

/// A parsed `project:region:instance` connection name.
#[derive(Debug, Clone)]
pub struct InstanceName {
    pub project: String,
    pub region: String,
    pub name: String,
}

impl InstanceName {
    pub fn parse(connection_name: &str) -> Result<Self, Error> {
        let mut parts = connection_name.split(':');
        match (parts.next(), parts.next(), parts.next(), parts.next()) {
            (Some(project), Some(region), Some(name), None)
                if !project.is_empty() && !region.is_empty() && !name.is_empty() =>
            {
                Ok(Self {
                    project: project.into(),
                    region: region.into(),
                    name: name.into(),
                })
            }
            _ => Err(Error::Config(format!(
                "instance connection name {connection_name:?} is not project:region:instance",
            ))),
        }
    }
}

impl std::fmt::Display for InstanceName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}:{}", self.project, self.region, self.name)
    }
}

/// Inputs for one endpoint build. Both tokens come pre-minted; the crate
/// never fetches or refreshes credentials.
pub struct EndpointRequest<'a> {
    pub http: &'a reqwest::Client,
    /// Override for tests; `None` uses the public sqladmin endpoint.
    pub api_base: Option<&'a str>,
    pub instance: &'a InstanceName,
    /// Bearer for the two sqladmin calls (`sqlservice.admin` scope).
    pub admin_token: &'a str,
    /// Rides inside `generateEphemeralCert` (`sqlservice.login` scope);
    /// the returned certificate carries this identity.
    pub login_token: &'a str,
    /// Override for tests; `None` uses [`SERVER_PROXY_PORT`].
    pub server_proxy_port_override: Option<u16>,
}

/// One immutable, connectable Cloud SQL endpoint: a resolved address, a
/// TLS client config holding the ephemeral certificate, and the
/// certificate's own expiry. The caller combines `cert_not_after` with
/// its token expiries to decide when to build a replacement.
pub struct CloudSqlEndpoint {
    address: IpAddr,
    port: u16,
    server_name: ServerName<'static>,
    connector: TlsConnector,
    cert_not_after: DateTime<Utc>,
    instance: InstanceName,
}

/// A relayed connection: TLS over TCP to the instance.
pub type CloudSqlStream = tokio_rustls::client::TlsStream<TcpStream>;

pub async fn build_endpoint(request: EndpointRequest<'_>) -> Result<CloudSqlEndpoint, Error> {
    let api_base = request.api_base.unwrap_or(api::DEFAULT_API_BASE);
    let settings = api::connect_settings(
        request.http,
        api_base,
        request.admin_token,
        request.instance,
    )
    .await?;

    let server_ca_pem = settings
        .server_ca_cert
        .as_ref()
        .map(|cert| cert.cert.as_str())
        .ok_or_else(|| Error::Api("connectSettings carries no server CA".into()))?;
    let address = pick_address(&settings)?;
    let verify = server_verify(&settings, request.instance)?;

    // 2048-bit RSA matches the documented protocol; generation is CPU
    // work, so it leaves the async thread.
    let key =
        tokio::task::spawn_blocking(|| rsa::RsaPrivateKey::new(&mut rand_core06::OsRng, 2048))
            .await
            .map_err(|e| Error::Tls(format!("key generation task failed: {e}")))?
            .map_err(|e| Error::Tls(format!("RSA key generation failed: {e}")))?;
    let spki_der = key
        .to_public_key()
        .to_public_key_der()
        .map_err(|e| Error::Tls(format!("public key encoding failed: {e}")))?;
    let public_key_pem = pem::encode(&pem::Pem::new("RSA PUBLIC KEY", spki_der.into_vec()));

    let client_cert_pem = api::generate_ephemeral_cert(
        request.http,
        api_base,
        request.admin_token,
        request.login_token,
        request.instance,
        &public_key_pem,
    )
    .await?;
    let cert_not_after = cert_not_after(&client_cert_pem)?;

    let key_der = key
        .to_pkcs8_der()
        .map_err(|e| Error::Tls(format!("private key encoding failed: {e}")))?;
    let config = tls::client_config(
        server_ca_pem,
        &verify,
        &client_cert_pem,
        PrivateKeyDer::Pkcs8(key_der.as_bytes().to_vec().into()),
    )?;

    let server_name = match &verify {
        tls::ServerVerify::StandardDns { dns_name } => ServerName::try_from(dns_name.clone())
            .map_err(|e| Error::Tls(format!("instance DNS name is invalid: {e}")))?,
        // The legacy verifier ignores the name; an IP name also disables
        // SNI, which the legacy server-side proxy predates.
        tls::ServerVerify::LegacyCn { .. } => ServerName::from(address),
    };

    Ok(CloudSqlEndpoint {
        address,
        port: request
            .server_proxy_port_override
            .unwrap_or(SERVER_PROXY_PORT),
        server_name,
        connector: TlsConnector::from(Arc::new(config)),
        cert_not_after,
        instance: request.instance.clone(),
    })
}

impl CloudSqlEndpoint {
    /// Open one connection: TCP to `:3307`, then the TLS handshake with
    /// the ephemeral certificate. One budget covers both.
    pub async fn connect(&self) -> std::io::Result<CloudSqlStream> {
        tokio::time::timeout(CONNECT_BUDGET, async {
            let tcp = TcpStream::connect((self.address, self.port)).await?;
            tcp.set_nodelay(true)?;
            self.connector.connect(self.server_name.clone(), tcp).await
        })
        .await
        .map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("connect to {} timed out", self.instance),
            )
        })?
    }

    /// The ephemeral certificate's `NotAfter`. Google caps it at the
    /// login token's expiry, so this alone bounds the endpoint's life;
    /// the caller still takes the minimum with its own token expiries.
    pub fn cert_not_after(&self) -> DateTime<Utc> {
        self.cert_not_after
    }
}

fn pick_address(settings: &api::ConnectSettings) -> Result<IpAddr, Error> {
    // The public address first, then private — the AutoIP order.
    let pick = ["PRIMARY", "PRIVATE"].iter().find_map(|wanted| {
        settings.ip_addresses.iter().find_map(|mapping| {
            (mapping.kind.as_deref() == Some(*wanted))
                .then_some(mapping.ip_address.as_deref())
                .flatten()
        })
    });
    let address = pick.ok_or_else(|| {
        Error::Api("connectSettings carries no PRIMARY or PRIVATE address".into())
    })?;
    address
        .parse()
        .map_err(|e| Error::Api(format!("instance address {address:?} is invalid: {e}")))
}

fn server_verify(
    settings: &api::ConnectSettings,
    instance: &InstanceName,
) -> Result<tls::ServerVerify, Error> {
    match settings.server_ca_mode.as_deref() {
        // CAS regimes name the instance via a DNS SAN. Deliberate deviation
        // from the reference connector: no CN fallback when the SAN check
        // fails (the Go connector tolerates certificates whose SANs lag the
        // metadata DNS name); a CAS instance in that transient state fails
        // the build and the pool retries.
        Some(mode) if mode.contains("CAS") => {
            let dns_name = settings
                .dns_name
                .clone()
                .ok_or_else(|| Error::Api(format!("CA mode {mode} without a DNS name")))?;
            Ok(tls::ServerVerify::StandardDns { dns_name })
        }
        // The legacy per-instance CA (or an old API that omits the mode).
        // The certificate CN is `project:instance` — TWO fields, no region
        // (reference: cloudsqlconn `verifyCn` builds `Project():Name()`).
        _ => Ok(tls::ServerVerify::LegacyCn {
            expected_cn: format!("{}:{}", instance.project, instance.name),
        }),
    }
}

fn cert_not_after(cert_pem: &str) -> Result<DateTime<Utc>, Error> {
    let der = tls::pem_to_der(cert_pem, "ephemeral client certificate")?;
    let (_, cert) = X509Certificate::from_der(&der)
        .map_err(|e| Error::Tls(format!("ephemeral certificate does not parse: {e}")))?;
    let timestamp = cert.validity().not_after.timestamp();
    Utc.timestamp_opt(timestamp, 0)
        .single()
        .ok_or_else(|| Error::Tls("ephemeral certificate expiry is out of range".into()))
}
