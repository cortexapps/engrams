//! Session-scoped Google metadata-compatible ADC endpoint.
//!
//! The endpoint returns only a fixed placeholder access token. Google API
//! requests still pass through the normal egress policy, which overwrites the
//! guest Authorization header with the host-held credential.

use std::net::SocketAddr;
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::Registry;

const MAX_REQUEST_BYTES: usize = 16 * 1024;
pub const PLACEHOLDER_TOKEN: &str = "engram_google_token_placeholder";

pub async fn serve(listener: TcpListener, registry: Arc<Registry>) -> std::io::Result<()> {
    loop {
        let (stream, peer) = listener.accept().await?;
        let registry = registry.clone();
        tokio::spawn(async move {
            if let Err(error) = serve_connection(stream, peer, &registry).await {
                tracing::debug!(%peer, %error, "metadata connection ended with error");
            }
        });
    }
}

async fn serve_connection(
    mut stream: TcpStream,
    peer: SocketAddr,
    registry: &Registry,
) -> std::io::Result<()> {
    let authorized = match peer.ip() {
        std::net::IpAddr::V4(ip) => session_allows_metadata(registry, ip),
        std::net::IpAddr::V6(_) => false,
    };
    if !authorized {
        write_response(&mut stream, "403 Forbidden", "text/plain", "forbidden").await?;
        return stream.shutdown().await;
    }

    let mut request = Vec::with_capacity(1024);
    while request.len() < MAX_REQUEST_BYTES {
        let mut chunk = [0_u8; 1024];
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            return Ok(());
        }
        request.extend_from_slice(&chunk[..read]);
        if request.windows(4).any(|value| value == b"\r\n\r\n") {
            break;
        }
    }
    let request = String::from_utf8_lossy(&request);
    let mut lines = request.lines();
    let mut request_line = lines.next().unwrap_or_default().split_ascii_whitespace();
    let method = request_line.next().unwrap_or_default();
    let target = request_line.next().unwrap_or_default();
    let metadata_flavor = lines.any(|line| {
        line.split_once(':').is_some_and(|(name, value)| {
            name.eq_ignore_ascii_case("metadata-flavor") && value.trim() == "Google"
        })
    });
    let (status, content_type, body) = response(method, target, metadata_flavor);
    write_response(&mut stream, status, content_type, &body).await?;
    stream.shutdown().await
}

fn session_allows_metadata(registry: &Registry, guest_ip: std::net::Ipv4Addr) -> bool {
    registry
        .lookup(guest_ip)
        .is_some_and(|state| state.google_adc)
}

async fn write_response(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    body: &str,
) -> std::io::Result<()> {
    let response = format!(
        "HTTP/1.1 {status}\r\nMetadata-Flavor: Google\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len(),
    );
    stream.write_all(response.as_bytes()).await
}

fn response(
    method: &str,
    target: &str,
    metadata_flavor: bool,
) -> (&'static str, &'static str, String) {
    if method != "GET" {
        return (
            "405 Method Not Allowed",
            "text/plain",
            "method not allowed".into(),
        );
    }
    if !metadata_flavor {
        return (
            "403 Forbidden",
            "text/plain",
            "Metadata-Flavor: Google is required".into(),
        );
    }
    let (path, query) = target
        .split_once('?')
        .map_or((target, ""), |(path, query)| (path, query));
    let text = |value: &str| ("200 OK", "text/plain", value.to_string());
    match path {
        "/" | "/computeMetadata/v1/" => text("instance/\nproject/\n"),
        "/computeMetadata/v1/instance/service-accounts/" => text("default/\n"),
        "/computeMetadata/v1/instance/service-accounts/default/"
            if query.split('&').any(|item| item == "recursive=true") =>
        {
            (
                "200 OK",
                "application/json",
                "{\"aliases\":[\"default\"],\"email\":\"engrams-broker@invalid\",\"scopes\":[\"https://www.googleapis.com/auth/cloud-platform\"]}".into(),
            )
        }
        "/computeMetadata/v1/instance/service-accounts/default/" => {
            text("email\nscopes\ntoken\n")
        }
        "/computeMetadata/v1/instance/service-accounts/default/email" => text("engrams-broker@invalid"),
        "/computeMetadata/v1/instance/service-accounts/default/scopes" => {
            text("https://www.googleapis.com/auth/cloud-platform\n")
        }
        path if path.starts_with("/computeMetadata/v1/instance/service-accounts/")
            && path.ends_with("/token") => (
            "200 OK",
            "application/json",
            format!(
                "{{\"access_token\":\"{PLACEHOLDER_TOKEN}\",\"expires_in\":300,\"token_type\":\"Bearer\"}}"
            ),
        ),
        "/computeMetadata/v1/project/project-id" => text("engrams-broker"),
        _ => ("404 Not Found", "text/plain", "not found".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{HostList, SessionState};
    use engram_core::SessionId;

    #[test]
    fn token_is_only_a_placeholder() {
        let (_, content_type, body) = response(
            "GET",
            "/computeMetadata/v1/instance/service-accounts/default/token",
            true,
        );
        assert_eq!(content_type, "application/json");
        assert!(body.contains(PLACEHOLDER_TOKEN));
        assert!(!body.contains("ya29."));
    }

    #[test]
    fn requires_google_flavor_and_rejects_mutation() {
        assert_eq!(response("GET", "/", false).0, "403 Forbidden");
        assert_eq!(response("POST", "/", true).0, "405 Method Not Allowed");
    }

    #[test]
    fn recursive_service_account_info_matches_google_auth_adc() {
        let (status, content_type, body) = response(
            "GET",
            "/computeMetadata/v1/instance/service-accounts/default/?recursive=true",
            true,
        );
        assert_eq!(status, "200 OK");
        assert_eq!(content_type, "application/json");
        let info: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(info["email"], "engrams-broker@invalid");
        assert_eq!(
            info["scopes"][0],
            "https://www.googleapis.com/auth/cloud-platform"
        );
    }

    #[test]
    fn metadata_is_enabled_only_by_the_registered_session_policy() {
        let registry = Registry::new();
        let guest_ip = "10.200.0.2".parse().unwrap();
        let state = |google_adc| SessionState {
            session_id: SessionId::new(),
            guest_ip,
            network_allow: HostList::empty(),
            allow_all: false,
            secrets: Vec::new(),
            injects: Vec::new(),
            observes: Vec::new(),
            google_adc,
        };
        registry.register(state(false));
        assert!(!session_allows_metadata(&registry, guest_ip));
        registry.register(state(true));
        assert!(session_allows_metadata(&registry, guest_ip));
        assert!(!session_allows_metadata(
            &registry,
            "10.200.0.6".parse().unwrap(),
        ));
    }

    #[tokio::test]
    async fn socket_endpoint_returns_only_the_placeholder_to_an_enabled_session() {
        let registry = Arc::new(Registry::new());
        registry.register(SessionState {
            session_id: SessionId::new(),
            guest_ip: std::net::Ipv4Addr::LOCALHOST,
            network_allow: HostList::empty(),
            allow_all: false,
            secrets: Vec::new(),
            injects: Vec::new(),
            observes: Vec::new(),
            google_adc: true,
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let registry_for_server = registry.clone();
        let server = tokio::spawn(async move {
            let (stream, peer) = listener.accept().await.unwrap();
            serve_connection(stream, peer, &registry_for_server)
                .await
                .unwrap();
        });

        let mut client = TcpStream::connect(address).await.unwrap();
        client
            .write_all(
                b"GET /computeMetadata/v1/instance/service-accounts/default/token HTTP/1.1\r\nMetadata-Flavor: Google\r\n\r\n",
            )
            .await
            .unwrap();
        let mut response = Vec::new();
        client.read_to_end(&mut response).await.unwrap();
        server.await.unwrap();
        let response = String::from_utf8(response).unwrap();
        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(response.contains(PLACEHOLDER_TOKEN));
        assert!(!response.contains("ya29."));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn installed_gcloud_can_use_the_metadata_endpoint_as_adc() {
        if std::process::Command::new("gcloud")
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }

        let registry = Arc::new(Registry::new());
        registry.register(SessionState {
            session_id: SessionId::new(),
            guest_ip: std::net::Ipv4Addr::LOCALHOST,
            network_allow: HostList::empty(),
            allow_all: false,
            secrets: Vec::new(),
            injects: Vec::new(),
            observes: Vec::new(),
            google_adc: true,
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(serve(listener, registry));
        let config = tempfile::tempdir().unwrap();
        let config_path = config.path().to_path_buf();
        let command_config_path = config_path.clone();
        let metadata_host = address.to_string();
        let output = tokio::task::spawn_blocking(move || {
            std::process::Command::new("gcloud")
                .args([
                    "auth",
                    "application-default",
                    "print-access-token",
                    "--quiet",
                ])
                .env("CLOUDSDK_CONFIG", command_config_path)
                .env("GCE_METADATA_HOST", &metadata_host)
                .env("GCE_METADATA_IP", &metadata_host)
                .env_remove("GOOGLE_APPLICATION_CREDENTIALS")
                .output()
                .unwrap()
        })
        .await
        .unwrap();
        server.abort();
        let _ = server.await;

        assert!(
            output.status.success(),
            "gcloud metadata ADC failed with status {}",
            output.status
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).trim() == PLACEHOLDER_TOKEN,
            "gcloud returned a token other than the fixed placeholder"
        );
        assert!(!config_path.join("credentials.db").exists());
        assert!(!config_path
            .join("application_default_credentials.json")
            .exists());
    }
}
