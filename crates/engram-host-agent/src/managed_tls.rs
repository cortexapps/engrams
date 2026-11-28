//! Managed host mode authenticates the single lifecycle listener.
use serde::Deserialize;
use std::{collections::BTreeSet, path::PathBuf};
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedHostTls {
    pub certificate: PathBuf,
    pub key: PathBuf,
    pub client_ca: PathBuf,
    pub coordinator_pins: BTreeSet<String>,
}
impl ManagedHostTls {
    pub fn from_env() -> Result<Option<Self>, String> {
        let Some(path) = std::env::var_os("ENGRAM_HOST_MANAGED_TLS_CONFIG") else {
            return Ok(None);
        };
        let bytes =
            std::fs::read(path).map_err(|_| "cannot read managed host TLS configuration")?;
        let value: Self =
            serde_json::from_slice(&bytes).map_err(|_| "invalid managed host TLS configuration")?;
        engram_protocol::host_tls::validate_pins(&value.coordinator_pins)?;
        Ok(Some(value))
    }
    pub fn server_config(&self) -> Result<tonic::transport::ServerTlsConfig, String> {
        engram_protocol::host_tls::validate_pins(&self.coordinator_pins)?;
        engram_tls::install_provider();
        let cert =
            std::fs::read(&self.certificate).map_err(|_| "cannot read managed host certificate")?;
        let key = std::fs::read(&self.key).map_err(|_| "cannot read managed host key")?;
        let ca = std::fs::read(&self.client_ca).map_err(|_| "cannot read coordinator CA")?;
        Ok(tonic::transport::ServerTlsConfig::new()
            .identity(tonic::transport::Identity::from_pem(cert, key))
            .client_ca_root(tonic::transport::Certificate::from_pem(ca)))
    }
}
