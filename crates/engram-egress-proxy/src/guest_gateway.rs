//! Session-scoped host services on one guest-reachable link-local endpoint.
//!
//! The gateway authenticates a caller by its registered guest source address,
//! parses one bounded HTTP request, and routes it to a trusted built-in service.
//! Native Engrams routes use `/_engrams/v1/`; compatibility adapters keep the
//! paths and proof-of-intent headers expected by an existing cloud SDK.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use engram_core::types::integration::SessionTunnel;
use engram_core::SessionId;

use crate::{Registry, SessionState};

const MAX_REQUEST_BYTES: usize = 16 * 1024;
const REQUEST_HEADER_TIMEOUT: Duration = Duration::from_secs(5);
pub const PLACEHOLDER_TOKEN: &str = "engram_google_token_placeholder";
pub const GCE_METADATA_SERVICE_KIND: &str = "gcp.gce_metadata";

/// A registered host implementation for one tunnel connector kind.
#[async_trait]
pub trait TunnelConnector: Send + Sync {
    fn kind(&self) -> &'static str;

    async fn relay(
        &self,
        downstream: TcpStream,
        initial_data: Vec<u8>,
        session_id: SessionId,
        tunnel: SessionTunnel,
    ) -> std::io::Result<()>;
}

/// One registered compatibility adapter on the guest gateway.
pub trait GuestServiceAdapter: Send + Sync {
    fn kind(&self) -> &'static str;

    /// Whether this compatibility adapter owns the request path.
    fn matches(&self, path: &str) -> bool;

    /// Does this request carry the proof-of-intent header the real service
    /// demands? It is what stops a browser or a confused-deputy fetch from
    /// reading the service.
    fn authorize(&self, headers: &[(&str, &str)]) -> bool;

    /// The header every response carries, so a client can tell it reached the
    /// service it expected.
    fn response_header(&self) -> (&'static str, &'static str);

    /// Answer one authorized `GET` for `path` with `query`.
    fn respond(&self, path: &str, query: &str) -> (&'static str, &'static str, String);
}

/// Trusted host implementations for the guest gateway. Session policy can
/// select a registered kind, but it cannot add code or replace an adapter.
#[derive(Default)]
pub struct GuestGatewayRegistry {
    services: HashMap<String, Arc<dyn GuestServiceAdapter>>,
    tunnels: HashMap<String, Arc<dyn TunnelConnector>>,
}

impl GuestGatewayRegistry {
    pub fn new(
        services: impl IntoIterator<Item = Arc<dyn GuestServiceAdapter>>,
        connectors: impl IntoIterator<Item = Arc<dyn TunnelConnector>>,
    ) -> Self {
        let mut service_map = HashMap::new();
        for service in services {
            let previous = service_map.insert(service.kind().to_string(), service);
            assert!(previous.is_none(), "duplicate guest service kind");
        }
        let mut tunnel_map = HashMap::new();
        for connector in connectors {
            let previous = tunnel_map.insert(connector.kind().to_string(), connector);
            assert!(previous.is_none(), "duplicate tunnel connector kind");
        }
        Self {
            services: service_map,
            tunnels: tunnel_map,
        }
    }

    fn service(&self, kind: &str) -> Option<Arc<dyn GuestServiceAdapter>> {
        self.services.get(kind).cloned()
    }

    fn tunnel(&self, kind: &str) -> Option<Arc<dyn TunnelConnector>> {
        self.tunnels.get(kind).cloned()
    }
}

pub async fn serve(
    listener: TcpListener,
    registry: Arc<Registry>,
    gateway: Arc<GuestGatewayRegistry>,
) -> std::io::Result<()> {
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(error) => {
                tracing::warn!(%error, "guest gateway accept failed; retrying");
                continue;
            }
        };
        let registry = registry.clone();
        let gateway = gateway.clone();
        tokio::spawn(async move {
            if let Err(error) = serve_connection(stream, peer, &registry, &gateway).await {
                tracing::debug!(%peer, %error, "guest gateway connection ended with error");
            }
        });
    }
}

async fn serve_connection(
    mut stream: TcpStream,
    peer: SocketAddr,
    registry: &Registry,
    gateway: &GuestGatewayRegistry,
) -> std::io::Result<()> {
    let session = match peer.ip() {
        std::net::IpAddr::V4(ip) => registry.lookup(ip),
        std::net::IpAddr::V6(_) => None,
    };
    let Some(session) = session else {
        // No registered session owns this guest address.
        write_response(
            &mut stream,
            ("Engram-Gateway", "1"),
            "403 Forbidden",
            "text/plain",
            "forbidden",
        )
        .await?;
        return stream.shutdown().await;
    };

    let (request, initial_data) = read_request_header(&mut stream, REQUEST_HEADER_TIMEOUT).await?;
    let request = String::from_utf8_lossy(&request);
    let mut lines = request.lines();
    let mut request_line = lines.next().unwrap_or_default().split_ascii_whitespace();
    let method = request_line.next().unwrap_or_default();
    let target = request_line.next().unwrap_or_default();
    let headers: Vec<(&str, &str)> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim(), value.trim()))
        .collect();
    if method == "GET" && target == "/_engrams/v1/tunnels" {
        if !authorized_native_request(&headers) {
            write_response(
                &mut stream,
                ("Engram-Gateway", "1"),
                "403 Forbidden",
                "text/plain",
                "forbidden",
            )
            .await?;
            return stream.shutdown().await;
        }
        let body = serde_json::to_string(
            &session
                .tunnels
                .iter()
                .map(|tunnel| {
                    serde_json::json!({
                        "id": tunnel.id,
                        "connector": tunnel.connector,
                    })
                })
                .collect::<Vec<_>>(),
        )
        .expect("tunnel summary is JSON-serializable");
        write_response(
            &mut stream,
            ("Engram-Gateway", "1"),
            "200 OK",
            "application/json",
            &body,
        )
        .await?;
        return stream.shutdown().await;
    }
    if method == "CONNECT" && target.starts_with("/_engrams/v1/tunnels/") {
        let Some(tunnel) = authorized_tunnel(&session, target, &headers) else {
            write_response(
                &mut stream,
                ("Engram-Gateway", "1"),
                "403 Forbidden",
                "text/plain",
                "forbidden",
            )
            .await?;
            return stream.shutdown().await;
        };
        let Some(connector) = gateway.tunnel(&tunnel.connector) else {
            write_response(
                &mut stream,
                ("Engram-Gateway", "1"),
                "503 Service Unavailable",
                "text/plain",
                "tunnel connector is unavailable",
            )
            .await?;
            return stream.shutdown().await;
        };
        let tunnel_id = tunnel.id.clone();
        let connector_kind = tunnel.connector.clone();
        if let Err(error) = connector
            .relay(stream, initial_data, session.session_id, tunnel)
            .await
        {
            tracing::warn!(
                %peer,
                session_id = %session.session_id,
                %tunnel_id,
                %connector_kind,
                %error,
                "guest tunnel relay failed",
            );
            return Err(error);
        }
        return Ok(());
    }

    let Some(service) = session
        .guest_services
        .iter()
        .filter_map(|service| gateway.service(service.as_str()))
        .find(|service| service.matches(target))
    else {
        write_response(
            &mut stream,
            ("Engram-Gateway", "1"),
            "403 Forbidden",
            "text/plain",
            "forbidden",
        )
        .await?;
        return stream.shutdown().await;
    };
    let (status, content_type, body) = respond(service.as_ref(), method, target, &headers);
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

async fn read_request_header(
    stream: &mut TcpStream,
    timeout: Duration,
) -> std::io::Result<(Vec<u8>, Vec<u8>)> {
    tokio::time::timeout(timeout, async {
        let mut request = Vec::with_capacity(1024);
        loop {
            let mut chunk = [0_u8; 1024];
            let read = stream.read(&mut chunk).await?;
            if read == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "guest gateway connection closed before the request header",
                ));
            }
            request.extend_from_slice(&chunk[..read]);
            if let Some(end) = request.windows(4).position(|value| value == b"\r\n\r\n") {
                let initial_data = request.split_off(end + 4);
                return Ok((request, initial_data));
            }
            if request.len() >= MAX_REQUEST_BYTES {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "guest gateway request header is too large",
                ));
            }
        }
    })
    .await
    .map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "guest gateway request header timed out",
        )
    })?
}

fn authorized_tunnel(
    session: &SessionState,
    target: &str,
    headers: &[(&str, &str)],
) -> Option<SessionTunnel> {
    if !authorized_native_request(headers) {
        return None;
    }
    let id = target.strip_prefix("/_engrams/v1/tunnels/")?;
    session
        .tunnels
        .iter()
        .find(|candidate| candidate.id == id)
        .cloned()
}

fn authorized_native_request(headers: &[(&str, &str)]) -> bool {
    headers
        .iter()
        .any(|(name, value)| name.eq_ignore_ascii_case("engram-gateway") && *value == "1")
}

/// The compatibility services this guest's session asked for.
#[cfg(test)]
fn session_guest_services(
    registry: &Registry,
    guest_ip: std::net::Ipv4Addr,
) -> Vec<engram_core::types::integration::GuestService> {
    registry
        .lookup(guest_ip)
        .map(|session| session.guest_services.clone())
        .unwrap_or_default()
}

/// Google Compute Engine's metadata server.
pub struct GceMetadataService;

impl GuestServiceAdapter for GceMetadataService {
    fn kind(&self) -> &'static str {
        GCE_METADATA_SERVICE_KIND
    }

    fn matches(&self, path: &str) -> bool {
        path == "/" || path.starts_with("/computeMetadata/")
    }

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
    service: &(dyn GuestServiceAdapter + Send + Sync),
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
    use engram_core::types::integration::GuestService;
    use engram_core::SessionId;

    struct CapturingConnector {
        received: Arc<parking_lot::Mutex<Vec<u8>>>,
    }

    #[async_trait]
    impl TunnelConnector for CapturingConnector {
        fn kind(&self) -> &'static str {
            "test.capture"
        }

        async fn relay(
            &self,
            mut downstream: TcpStream,
            initial_data: Vec<u8>,
            _session_id: SessionId,
            _tunnel: SessionTunnel,
        ) -> std::io::Result<()> {
            let mut received = initial_data;
            downstream.read_to_end(&mut received).await?;
            *self.received.lock() = received;
            downstream
                .write_all(b"HTTP/1.1 200 Connection Established\r\nEngram-Gateway: 1\r\n\r\n")
                .await
        }
    }

    fn gateway_registry() -> GuestGatewayRegistry {
        GuestGatewayRegistry::new(
            [Arc::new(GceMetadataService) as Arc<dyn GuestServiceAdapter>],
            std::iter::empty::<Arc<dyn TunnelConnector>>(),
        )
    }

    /// Drive the real dispatch: pick the compatibility service, then run the
    /// shared method/authorize gate. `flavored` says whether the caller sent
    /// the proof-of-intent header the service demands.
    fn response(
        method: &str,
        target: &str,
        flavored: bool,
    ) -> (&'static str, &'static str, String) {
        let service = GceMetadataService;
        let (name, value) = service.response_header();
        let headers: Vec<(&str, &str)> = if flavored {
            vec![(name, value)]
        } else {
            Vec::new()
        };
        respond(&service, method, target, &headers)
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
    fn a_session_with_no_compatibility_service_is_served_nothing() {
        // The endpoint is per-session. A session that asked for no metadata
        // service must not reach another service's attribute tree just because
        // the listener is bound.
        let registry = Registry::new();
        let guest_ip: std::net::Ipv4Addr = "10.200.0.9".parse().unwrap();
        assert!(session_guest_services(&registry, guest_ip).is_empty());
    }

    fn assert_service_is_well_formed(service: &dyn GuestServiceAdapter) {
        let (name, value) = service.response_header();
        assert!(!name.is_empty() && !value.is_empty(), "{}", service.kind());
        // The proof-of-intent header is exactly the one the service names, and
        // nothing else opens the attribute tree.
        assert!(service.authorize(&[(name, value)]), "{}", service.kind());
        assert!(!service.authorize(&[]), "{}", service.kind());
        assert!(!service.authorize(&[(name, "wrong")]), "{}", service.kind());
    }

    #[test]
    fn registered_compatibility_service_states_its_own_header() {
        let gateway = gateway_registry();
        let service = gateway.service(GCE_METADATA_SERVICE_KIND).unwrap();
        assert_service_is_well_formed(service.as_ref());
        assert!(gateway.service("unknown.service").is_none());
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
        let state = |guest_services| SessionState {
            session_id: SessionId::new(),
            guest_ip,
            network_allow: HostList::empty(),
            allow_all: false,
            secrets: Vec::new(),
            injects: Vec::new(),
            observes: Vec::new(),
            guest_services,
            tunnels: Vec::new(),
        };
        registry.register(state(Vec::new()));
        assert!(session_guest_services(&registry, guest_ip).is_empty());
        registry.register(state(vec![GuestService::new("gcp.gce_metadata")]));
        assert_eq!(
            session_guest_services(&registry, guest_ip),
            vec![GuestService::new("gcp.gce_metadata")],
        );
        // An IP with no registered session gets nothing.
        assert!(session_guest_services(&registry, "10.200.0.6".parse().unwrap()).is_empty());
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
            guest_services: vec![GuestService::new("gcp.gce_metadata")],
            tunnels: Vec::new(),
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let registry_for_server = registry.clone();
        let server = tokio::spawn(async move {
            let (stream, peer) = listener.accept().await.unwrap();
            serve_connection(stream, peer, &registry_for_server, &gateway_registry())
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

    #[tokio::test]
    async fn tunnel_listing_exposes_only_opaque_ids_and_connector_kinds() {
        use engram_core::types::integration::CredentialMintSource;

        let registry = Arc::new(Registry::new());
        registry.register(SessionState {
            session_id: SessionId::new(),
            guest_ip: std::net::Ipv4Addr::LOCALHOST,
            network_allow: HostList::empty(),
            allow_all: false,
            secrets: Vec::new(),
            injects: Vec::new(),
            observes: Vec::new(),
            guest_services: Vec::new(),
            tunnels: vec![SessionTunnel {
                id: "prod-readonly".into(),
                connector: "gcp.cloud_sql".into(),
                config_json:
                    r#"{"instance":"customer:region:prod","database_user":"reader@customer.iam"}"#
                        .into(),
                mint_source: Some(CredentialMintSource::Connection {
                    connection_id: "secret-connection-id".into(),
                    provider: "gcp".into(),
                }),
            }],
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, peer) = listener.accept().await.unwrap();
            serve_connection(stream, peer, &registry, &GuestGatewayRegistry::default())
                .await
                .unwrap();
        });

        let mut client = TcpStream::connect(address).await.unwrap();
        client
            .write_all(b"GET /_engrams/v1/tunnels HTTP/1.1\r\nEngram-Gateway: 1\r\n\r\n")
            .await
            .unwrap();
        let mut response = Vec::new();
        client.read_to_end(&mut response).await.unwrap();
        server.await.unwrap();
        let response = String::from_utf8(response).unwrap();
        assert!(response.contains(r#""id":"prod-readonly""#));
        assert!(response.contains(r#""connector":"gcp.cloud_sql""#));
        assert!(!response.contains("customer:region:prod"));
        assert!(!response.contains("reader@customer.iam"));
        assert!(!response.contains("secret-connection-id"));
    }

    #[tokio::test]
    async fn request_header_read_times_out_on_a_silent_client() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let _client = TcpStream::connect(address).await.unwrap();
        let (mut server, _) = listener.accept().await.unwrap();
        let error = read_request_header(&mut server, Duration::from_millis(20))
            .await
            .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
    }

    #[tokio::test]
    async fn connect_preserves_bytes_coalesced_after_the_request_header() {
        let registry = Arc::new(Registry::new());
        registry.register(SessionState {
            session_id: SessionId::new(),
            guest_ip: std::net::Ipv4Addr::LOCALHOST,
            network_allow: HostList::empty(),
            allow_all: false,
            secrets: Vec::new(),
            injects: Vec::new(),
            observes: Vec::new(),
            guest_services: Vec::new(),
            tunnels: vec![SessionTunnel {
                id: "capture".into(),
                connector: "test.capture".into(),
                config_json: "{}".into(),
                mint_source: None,
            }],
        });
        let received = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let gateway = Arc::new(GuestGatewayRegistry::new(
            std::iter::empty::<Arc<dyn GuestServiceAdapter>>(),
            [Arc::new(CapturingConnector {
                received: received.clone(),
            }) as Arc<dyn TunnelConnector>],
        ));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, peer) = listener.accept().await.unwrap();
            serve_connection(stream, peer, &registry, &gateway)
                .await
                .unwrap();
        });

        let mut client = TcpStream::connect(address).await.unwrap();
        client
            .write_all(
                b"CONNECT /_engrams/v1/tunnels/capture HTTP/1.1\r\nEngram-Gateway: 1\r\n\r\npostgres-startup",
            )
            .await
            .unwrap();
        client.shutdown().await.unwrap();
        let mut response = Vec::new();
        client.read_to_end(&mut response).await.unwrap();
        server.await.unwrap();
        assert!(response.starts_with(b"HTTP/1.1 200 Connection Established\r\n"));
        assert_eq!(&*received.lock(), b"postgres-startup");
    }

    #[test]
    fn tunnel_connect_requires_the_exact_compiled_id() {
        use engram_core::types::integration::CredentialMintSource;

        let session = SessionState {
            session_id: SessionId::new(),
            guest_ip: std::net::Ipv4Addr::LOCALHOST,
            network_allow: HostList::empty(),
            allow_all: false,
            secrets: Vec::new(),
            injects: Vec::new(),
            observes: Vec::new(),
            guest_services: vec![GuestService::new("gcp.gce_metadata")],
            tunnels: vec![SessionTunnel {
                id: "prod-readonly".into(),
                connector: "gcp.cloud_sql".into(),
                config_json: "{}".into(),
                mint_source: Some(CredentialMintSource::Connection {
                    connection_id: "gcp-prod".into(),
                    provider: "gcp".into(),
                }),
            }],
        };
        let good = [("Engram-Gateway", "1")];
        assert!(authorized_tunnel(&session, "/_engrams/v1/tunnels/other", &good,).is_none());
        assert!(authorized_tunnel(&session, "/_engrams/v1/tunnels/prod-readonly", &[],).is_none());
        assert_eq!(
            authorized_tunnel(&session, "/_engrams/v1/tunnels/prod-readonly", &good,)
                .unwrap()
                .id,
            "prod-readonly"
        );
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
            guest_services: vec![GuestService::new("gcp.gce_metadata")],
            tunnels: Vec::new(),
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(serve(listener, registry, Arc::new(gateway_registry())));
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
