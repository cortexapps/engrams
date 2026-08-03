//! Session-scoped cloud metadata endpoint.
//!
//! A cloud SDK inside the guest looks for its credential on a well-known
//! link-local address. The host answers there instead, with a fixed
//! PLACEHOLDER token — the guest never holds a real credential. The request
//! the guest then makes still passes through the normal egress policy, which
//! replaces that placeholder with the host-held credential on the wire.
//!
//! Which service is imitated is a [`MetadataFlavor`], not a boolean. Address
//! steering, the authorization check and the request framing are shared; a
//! flavor supplies only `authorize` (what proves the caller expects THIS
//! service) and `respond` (its attribute tree). The dispatch is a
//! wildcard-free `match`, so a second cloud is a compile error here rather
//! than a silently unserved session.

use std::net::SocketAddr;
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use engram_core::types::integration::MetadataFlavor;

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
    let flavor = match peer.ip() {
        std::net::IpAddr::V4(ip) => session_metadata_flavor(registry, ip),
        std::net::IpAddr::V6(_) => None,
    };
    let Some(service) = flavor.map(service_for) else {
        // No session, or a session that asked for no metadata service.
        write_response(
            &mut stream,
            ("Metadata-Flavor", "Google"),
            "403 Forbidden",
            "text/plain",
            "forbidden",
        )
        .await?;
        return stream.shutdown().await;
    };

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
    let headers: Vec<(&str, &str)> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim(), value.trim()))
        .collect();
    let (status, content_type, body) = respond(service, method, target, &headers);
    write_response(
        &mut stream,
        service.response_header(),
        status,
        content_type,
        &body,
    )
    .await?;
    stream.shutdown().await
}

/// The flavor this guest's session asked for, or `None` when it asked for none.
fn session_metadata_flavor(
    registry: &Registry,
    guest_ip: std::net::Ipv4Addr,
) -> Option<MetadataFlavor> {
    registry.lookup(guest_ip)?.metadata_flavor
}

/// One cloud's metadata service.
pub trait MetadataService: Send + Sync {
    /// Does this request carry the proof-of-intent header the real service
    /// demands? It is what stops a browser or a confused-deputy fetch from
    /// reading the attribute tree.
    fn authorize(&self, headers: &[(&str, &str)]) -> bool;

    /// The header every response carries, so a client can tell it reached the
    /// service it expected.
    fn response_header(&self) -> (&'static str, &'static str);

    /// Answer one authorized `GET` for `path` with `query`.
    fn respond(&self, path: &str, query: &str) -> (&'static str, &'static str, String);
}

/// Resolve a flavor to its implementation.
///
/// Wildcard-free on purpose: a new [`MetadataFlavor`] variant must be given a
/// service here or this does not compile.
fn service_for(flavor: MetadataFlavor) -> &'static (dyn MetadataService + Send + Sync) {
    match flavor {
        MetadataFlavor::Gce => &GceMetadata,
    }
}

/// Google Compute Engine's metadata server.
struct GceMetadata;

impl MetadataService for GceMetadata {
    fn authorize(&self, headers: &[(&str, &str)]) -> bool {
        headers
            .iter()
            .any(|(name, value)| name.eq_ignore_ascii_case("metadata-flavor") && *value == "Google")
    }

    fn response_header(&self) -> (&'static str, &'static str) {
        ("Metadata-Flavor", "Google")
    }

    fn respond(&self, path: &str, query: &str) -> (&'static str, &'static str, String) {
        gce_response(path, query)
    }
}

async fn write_response(
    stream: &mut TcpStream,
    service_header: (&str, &str),
    status: &str,
    content_type: &str,
    body: &str,
) -> std::io::Result<()> {
    let (header_name, header_value) = service_header;
    let response = format!(
        "HTTP/1.1 {status}\r\n{header_name}: {header_value}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len(),
    );
    stream.write_all(response.as_bytes()).await
}

/// The provider-neutral gate: read-only, and only for a caller that proved it
/// meant to reach a metadata service.
fn respond(
    service: &(dyn MetadataService + Send + Sync),
    method: &str,
    target: &str,
    headers: &[(&str, &str)],
) -> (&'static str, &'static str, String) {
    if method != "GET" {
        return (
            "405 Method Not Allowed",
            "text/plain",
            "method not allowed".into(),
        );
    }
    if !service.authorize(headers) {
        let (name, value) = service.response_header();
        return (
            "403 Forbidden",
            "text/plain",
            format!("{name}: {value} is required"),
        );
    }
    let (path, query) = target
        .split_once('?')
        .map_or((target, ""), |(path, query)| (path, query));
    service.respond(path, query)
}

/// Google Compute Engine's attribute tree.
fn gce_response(path: &str, query: &str) -> (&'static str, &'static str, String) {
    let text = |value: &str| ("200 OK", "text/plain", value.to_string());
    match path {
        "/" | "/computeMetadata/v1/" => text("instance/\nproject/\n"),
        "/computeMetadata/v1/instance/service-accounts/" => {
            text("default/\nengrams-broker@invalid/\n")
        }
        path if path.starts_with("/computeMetadata/v1/instance/service-accounts/")
            && path != "/computeMetadata/v1/instance/service-accounts/"
            && path.ends_with('/')
            && query.split('&').any(|item| item == "recursive=true") =>
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
        // No project id. The proxy brokers a credential; it does not know which
        // project the session's service account belongs to, and inventing one
        // is worse than admitting it: `engrams-broker` was answered here, so
        // `gcloud compute instances list` silently targeted a project that does
        // not exist until the operator passed `--project`. A 404 is what a
        // metadata server returns for an attribute it does not hold, so the
        // Cloud SDK reports a missing project and asks for one.
        "/computeMetadata/v1/project/project-id" => {
            ("404 Not Found", "text/plain", "not found".into())
        }
        // Cloud SDK uses this digits-only response to detect a GCE metadata
        // server. This is a synthetic identifier, not a customer project.
        "/computeMetadata/v1/project/numeric-project-id" => text("0"),
        _ => ("404 Not Found", "text/plain", "not found".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{HostList, SessionState};
    use engram_core::SessionId;

    /// Drive the real dispatch: pick the service for a flavor, then run the
    /// shared method/authorize gate. `flavored` says whether the caller sent
    /// the proof-of-intent header the service demands.
    fn response(
        method: &str,
        target: &str,
        flavored: bool,
    ) -> (&'static str, &'static str, String) {
        let service = service_for(MetadataFlavor::Gce);
        let (name, value) = service.response_header();
        let headers: Vec<(&str, &str)> = if flavored {
            vec![(name, value)]
        } else {
            Vec::new()
        };
        respond(service, method, target, &headers)
    }

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
    fn a_session_with_no_flavor_is_served_nothing() {
        // The endpoint is per-session. A session that asked for no metadata
        // service must not reach another flavor's attribute tree just because
        // the listener is bound.
        let registry = Registry::new();
        let guest_ip: std::net::Ipv4Addr = "10.200.0.9".parse().unwrap();
        assert!(session_metadata_flavor(&registry, guest_ip).is_none());
    }

    fn assert_service_is_well_formed(flavor: MetadataFlavor) {
        let service = service_for(flavor);
        let (name, value) = service.response_header();
        assert!(!name.is_empty() && !value.is_empty(), "{flavor:?}");
        // The proof-of-intent header is exactly the one the service names, and
        // nothing else opens the attribute tree.
        assert!(service.authorize(&[(name, value)]), "{flavor:?}");
        assert!(!service.authorize(&[]), "{flavor:?}");
        assert!(!service.authorize(&[(name, "wrong")]), "{flavor:?}");
    }

    #[test]
    fn every_flavor_resolves_to_a_service_that_states_its_own_header() {
        // Wildcard-free, like `service_for` itself: a new variant does not
        // compile until it is asserted here, so this covers the whole set by
        // construction rather than by a list someone has to remember to grow.
        match MetadataFlavor::Gce {
            MetadataFlavor::Gce => assert_service_is_well_formed(MetadataFlavor::Gce),
        }
    }

    #[test]
    fn requires_google_flavor_and_rejects_mutation() {
        assert_eq!(response("GET", "/", false).0, "403 Forbidden");
        assert_eq!(response("POST", "/", true).0, "405 Method Not Allowed");
    }

    #[test]
    fn recursive_service_account_info_matches_google_auth_adc() {
        for account in ["default", "engrams-broker@invalid"] {
            let (status, content_type, body) = response(
                "GET",
                &format!("/computeMetadata/v1/instance/service-accounts/{account}/?recursive=true"),
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
    }

    #[test]
    fn service_account_listing_matches_cloud_sdk_discovery() {
        let (status, content_type, body) = response(
            "GET",
            "/computeMetadata/v1/instance/service-accounts/",
            true,
        );
        assert_eq!(status, "200 OK");
        assert_eq!(content_type, "text/plain");
        assert_eq!(body, "default/\nengrams-broker@invalid/\n");
    }

    #[test]
    fn cloud_sdk_detection_uses_only_synthetic_project_metadata() {
        let (status, content_type, body) = response(
            "GET",
            "/computeMetadata/v1/project/numeric-project-id",
            true,
        );
        assert_eq!(status, "200 OK");
        assert_eq!(content_type, "text/plain");
        assert_eq!(body, "0");
        assert!(body.bytes().all(|byte| byte.is_ascii_digit()));

        // The proxy does not know the session's project, and answering with a
        // made-up one made `gcloud compute …` target a project that does not
        // exist until `--project` was passed. A metadata server returns 404 for
        // an attribute it does not hold.
        let (status, _, _) = response("GET", "/computeMetadata/v1/project/project-id", true);
        assert_eq!(status, "404 Not Found");
    }

    #[test]
    fn metadata_is_enabled_only_by_the_registered_session_policy() {
        let registry = Registry::new();
        let guest_ip = "10.200.0.2".parse().unwrap();
        let state = |metadata_flavor| SessionState {
            session_id: SessionId::new(),
            guest_ip,
            network_allow: HostList::empty(),
            allow_all: false,
            secrets: Vec::new(),
            injects: Vec::new(),
            observes: Vec::new(),
            metadata_flavor,
        };
        registry.register(state(None));
        assert!(session_metadata_flavor(&registry, guest_ip).is_none());
        registry.register(state(Some(MetadataFlavor::Gce)));
        assert_eq!(
            session_metadata_flavor(&registry, guest_ip),
            Some(MetadataFlavor::Gce),
        );
        // An IP with no registered session gets nothing.
        assert!(session_metadata_flavor(&registry, "10.200.0.6".parse().unwrap()).is_none());
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
            metadata_flavor: Some(MetadataFlavor::Gce),
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
            metadata_flavor: Some(MetadataFlavor::Gce),
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(serve(listener, registry));
        let config = tempfile::tempdir().unwrap();
        let config_path = config.path().to_path_buf();
        let command_config_path = config_path.clone();
        let metadata_host = address.to_string();
        let outputs = tokio::task::spawn_blocking(move || {
            let run = |args: &[&str]| {
                std::process::Command::new("gcloud")
                    .args(args)
                    .env("CLOUDSDK_CONFIG", &command_config_path)
                    .env("GCE_METADATA_HOST", &metadata_host)
                    .env("GCE_METADATA_IP", &metadata_host)
                    .env("GCE_METADATA_ROOT", &metadata_host)
                    .env("CLOUDSDK_CORE_CHECK_GCE_METADATA", "true")
                    .env_remove("GOOGLE_APPLICATION_CREDENTIALS")
                    .output()
                    .unwrap()
            };
            [
                run(&["auth", "print-access-token", "--quiet"]),
                run(&[
                    "auth",
                    "application-default",
                    "print-access-token",
                    "--quiet",
                ]),
            ]
        })
        .await
        .unwrap();
        server.abort();
        let _ = server.await;

        for output in outputs {
            assert!(
                output.status.success(),
                "gcloud metadata ADC failed with status {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr),
            );
            assert!(
                String::from_utf8_lossy(&output.stdout).trim() == PLACEHOLDER_TOKEN,
                "gcloud returned a token other than the fixed placeholder"
            );
        }
        // Standard gcloud creates its empty credential-store database while it
        // discovers metadata accounts. It must not create an ADC document that
        // could outlive the session-local metadata flow.
        assert!(!config_path
            .join("application_default_credentials.json")
            .exists());
    }
}
