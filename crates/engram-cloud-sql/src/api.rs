//! The two sqladmin calls of the connect protocol.
//!
//! Both are host-side and authenticated by the ADMIN token; the LOGIN
//! token rides inside `generateEphemeralCert` so the returned client
//! certificate carries the IAM database identity ("automatic IAM
//! authentication" — the certificate is the credential, and PostgreSQL
//! asks for no password).

use serde::Deserialize;

use crate::{Error, InstanceName};

/// Overridable for tests; production always talks to the public endpoint.
pub const DEFAULT_API_BASE: &str = "https://sqladmin.googleapis.com";

/// `GET /sql/v1beta4/projects/{p}/instances/{i}/connectSettings`.
/// Response fields are camelCase. Unknown fields stay tolerated — the
/// API grows fields routinely.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectSettings {
    #[serde(default)]
    pub ip_addresses: Vec<IpMapping>,
    pub server_ca_cert: Option<SslCert>,
    #[serde(default)]
    pub dns_name: Option<String>,
    #[serde(default)]
    pub server_ca_mode: Option<String>,
    #[serde(default)]
    pub region: Option<String>,
    #[serde(default)]
    pub database_version: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IpMapping {
    #[serde(rename = "type", default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub ip_address: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SslCert {
    pub cert: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GenerateEphemeralCertResponse {
    ephemeral_cert: SslCert,
}

pub async fn connect_settings(
    http: &reqwest::Client,
    api_base: &str,
    admin_token: &str,
    instance: &InstanceName,
) -> Result<ConnectSettings, Error> {
    let url = format!(
        "{api_base}/sql/v1beta4/projects/{}/instances/{}/connectSettings",
        instance.project, instance.name,
    );
    let response = http
        .get(url)
        .bearer_auth(admin_token)
        .send()
        .await
        .map_err(|e| Error::Api(format!("connectSettings request failed: {e}")))?;
    let status = response.status();
    if !status.is_success() {
        return Err(Error::Api(format!("connectSettings returned {status}")));
    }
    let settings: ConnectSettings = response
        .json()
        .await
        .map_err(|e| Error::Api(format!("connectSettings body is invalid: {e}")))?;
    // The instance connection name names a region; a mismatch means the
    // caller connected to a different instance than it believes.
    if let Some(region) = settings.region.as_deref() {
        if region != instance.region {
            return Err(Error::Config(format!(
                "instance {} is in region {region}, not {}",
                instance, instance.region,
            )));
        }
    }
    Ok(settings)
}

/// The request body uses snake_case field names — a v1beta4 quirk of this
/// one message (responses are camelCase). The public key travels as the
/// PKIX/SPKI DER in a PEM block labeled `RSA PUBLIC KEY`, the exact shape
/// the API is known to accept.
pub async fn generate_ephemeral_cert(
    http: &reqwest::Client,
    api_base: &str,
    admin_token: &str,
    login_token: &str,
    instance: &InstanceName,
    public_key_pem: &str,
) -> Result<String, Error> {
    let url = format!(
        "{api_base}/sql/v1beta4/projects/{}/instances/{}:generateEphemeralCert",
        instance.project, instance.name,
    );
    let body = serde_json::json!({
        "public_key": public_key_pem,
        "access_token": login_token,
    });
    let response = http
        .post(url)
        .bearer_auth(admin_token)
        .json(&body)
        .send()
        .await
        .map_err(|e| Error::Api(format!("generateEphemeralCert request failed: {e}")))?;
    let status = response.status();
    if !status.is_success() {
        return Err(Error::Api(format!(
            "generateEphemeralCert returned {status}"
        )));
    }
    let response: GenerateEphemeralCertResponse = response
        .json()
        .await
        .map_err(|e| Error::Api(format!("generateEphemeralCert body is invalid: {e}")))?;
    Ok(response.ephemeral_cert.cert)
}
