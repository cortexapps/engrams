//! Full connect-protocol round trips against a mock sqladmin API and an
//! in-process fake instance: connect settings, ephemeral-cert signing of
//! the crate's real submitted public key, then TLS to the fake `:3307`
//! with client auth verified against the instance CA — for BOTH server
//! CA regimes (legacy per-instance CN and CAS DNS-SAN).

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::State;
use axum::routing::{get, post};
use axum::{Json, Router};
use engram_cloud_sql::{build_endpoint, EndpointRequest, InstanceName};
use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, DnType, IsCa, KeyPair,
    SubjectPublicKeyInfo,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_rustls::rustls::pki_types::PrivateKeyDer;
use tokio_rustls::rustls::server::WebPkiClientVerifier;
use tokio_rustls::rustls::{RootCertStore, ServerConfig};

const INSTANCE: &str = "proj-1:us-west2:db-1";
const LOGIN_TOKEN: &str = "login-token-value";
const ADMIN_TOKEN: &str = "admin-token-value";

struct Fixture {
    ca: Arc<CertifiedIssuer<'static, KeyPair>>,
}

impl Fixture {
    fn new() -> Self {
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params
            .distinguished_name
            .push(DnType::CommonName, "Test Instance CA");
        Self {
            ca: Arc::new(CertifiedIssuer::self_signed(params, key).unwrap()),
        }
    }

    fn ca_pem(&self) -> String {
        self.ca.pem()
    }

    /// A server certificate in the requested regime.
    fn server_cert(&self, common_name: Option<&str>, dns_san: Option<&str>) -> (String, KeyPair) {
        let key = KeyPair::generate().unwrap();
        let sans: Vec<String> = dns_san.map(str::to_string).into_iter().collect();
        let mut params = CertificateParams::new(sans).unwrap();
        if let Some(common_name) = common_name {
            params
                .distinguished_name
                .push(DnType::CommonName, common_name);
        }
        let cert = params.signed_by(&key, &*self.ca).unwrap();
        (cert.pem(), key)
    }

    /// The mock `generateEphemeralCert`: signs the SUBMITTED public key
    /// with the instance CA, exactly like the real API.
    fn sign_client_key(&self, public_key_pem: &str) -> String {
        let spki = SubjectPublicKeyInfo::from_pem(public_key_pem).unwrap();
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params
            .distinguished_name
            .push(DnType::CommonName, "ephemeral-client");
        params.signed_by(&spki, &*self.ca).unwrap().pem()
    }
}

/// Serve the two sqladmin routes. `ca_mode`/`dns_name` shape the
/// connectSettings answer; the cert route signs whatever key arrives.
async fn mock_sqladmin(
    fixture: Arc<Fixture>,
    instance_addr: SocketAddr,
    ca_mode: Option<&'static str>,
    dns_name: Option<&'static str>,
) -> String {
    #[derive(Clone)]
    struct AppState {
        fixture: Arc<Fixture>,
        instance_addr: SocketAddr,
        ca_mode: Option<&'static str>,
        dns_name: Option<&'static str>,
    }

    async fn settings(State(state): State<AppState>) -> Json<serde_json::Value> {
        Json(serde_json::json!({
            "kind": "sql#connectSettings",
            "region": "us-west2",
            "databaseVersion": "POSTGRES_16",
            "serverCaMode": state.ca_mode,
            "dnsName": state.dns_name,
            "serverCaCert": { "cert": state.fixture.ca_pem() },
            "ipAddresses": [
                { "type": "PRIMARY", "ipAddress": state.instance_addr.ip().to_string() },
            ],
            "futureField": { "tolerated": true },
        }))
    }

    async fn ephemeral(
        State(state): State<AppState>,
        Json(body): Json<serde_json::Value>,
    ) -> Json<serde_json::Value> {
        assert_eq!(body["access_token"], LOGIN_TOKEN);
        let public_key = body["public_key"].as_str().expect("a public key PEM");
        assert!(public_key.contains("RSA PUBLIC KEY"));
        Json(serde_json::json!({
            "ephemeralCert": { "cert": state.fixture.sign_client_key(public_key) },
        }))
    }

    let app = Router::new()
        .route(
            "/sql/v1beta4/projects/proj-1/instances/db-1/connectSettings",
            get(settings),
        )
        .route(
            "/sql/v1beta4/projects/proj-1/instances/db-1:generateEphemeralCert",
            post(ephemeral),
        )
        .with_state(AppState {
            fixture,
            instance_addr,
            ca_mode,
            dns_name,
        });
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    base
}

/// A fake instance: TLS on an ephemeral port, client auth REQUIRED and
/// verified against the instance CA (so the test proves the ephemeral
/// certificate chain works), then echo.
async fn fake_instance(fixture: &Fixture, cert_pem: String, key: KeyPair) -> SocketAddr {
    let mut roots = RootCertStore::empty();
    roots
        .add(engram_cloud_sql::tls::pem_to_der(&fixture.ca_pem(), "test CA").unwrap())
        .unwrap();
    let verifier = WebPkiClientVerifier::builder(Arc::new(roots))
        .build()
        .unwrap();
    let cert_der = engram_cloud_sql::tls::pem_to_der(&cert_pem, "test server cert").unwrap();
    let key_der = PrivateKeyDer::Pkcs8(key.serialize_der().into());
    let config = ServerConfig::builder()
        .with_client_cert_verifier(verifier)
        .with_single_cert(vec![cert_der], key_der)
        .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((tcp, _)) = listener.accept().await else {
                return;
            };
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let Ok(tls) = acceptor.accept(tcp).await else {
                    return;
                };
                let (mut reader, mut writer) = tokio::io::split(tls);
                let _ = tokio::io::copy(&mut reader, &mut writer).await;
            });
        }
    });
    addr
}

async fn built_endpoint(
    api_base: &str,
    instance_port: u16,
) -> Result<engram_cloud_sql::CloudSqlEndpoint, engram_cloud_sql::Error> {
    let http = reqwest::Client::new();
    let instance = InstanceName::parse(INSTANCE).unwrap();
    build_endpoint(EndpointRequest {
        http: &http,
        api_base: Some(api_base),
        instance: &instance,
        admin_token: ADMIN_TOKEN,
        login_token: LOGIN_TOKEN,
        server_proxy_port_override: Some(instance_port),
    })
    .await
}

async fn assert_echo(endpoint: &engram_cloud_sql::CloudSqlEndpoint) {
    let mut conn = endpoint.connect().await.unwrap();
    conn.write_all(b"startup").await.unwrap();
    let mut reply = [0_u8; 7];
    conn.read_exact(&mut reply).await.unwrap();
    assert_eq!(&reply, b"startup");
}

#[tokio::test]
async fn legacy_cn_regime_connects_end_to_end() {
    let fixture = Arc::new(Fixture::new());
    let (cert, key) = fixture.server_cert(Some(INSTANCE), None);
    let instance = fake_instance(&fixture, cert, key).await;
    let api = mock_sqladmin(fixture, instance, None, None).await;

    let endpoint = built_endpoint(&api, instance.port()).await.unwrap();
    assert_echo(&endpoint).await;
    assert!(endpoint.cert_not_after() > chrono::Utc::now() - chrono::Duration::days(1));
}

#[tokio::test]
async fn legacy_cn_regime_rejects_a_wrong_instance_name() {
    let fixture = Arc::new(Fixture::new());
    let (cert, key) = fixture.server_cert(Some("proj-1:us-west2:other-db"), None);
    let instance = fake_instance(&fixture, cert, key).await;
    let api = mock_sqladmin(fixture, instance, None, None).await;

    let endpoint = built_endpoint(&api, instance.port()).await.unwrap();
    let error = endpoint.connect().await.expect_err("wrong CN must fail");
    assert!(error.to_string().contains("not the instance"), "{error}");
}

#[tokio::test]
async fn cas_regime_connects_with_dns_san_verification() {
    let fixture = Arc::new(Fixture::new());
    let (cert, key) = fixture.server_cert(None, Some("db-1.us-west2.sql.internal"));
    let instance = fake_instance(&fixture, cert, key).await;
    let api = mock_sqladmin(
        fixture,
        instance,
        Some("GOOGLE_MANAGED_CAS_CA"),
        Some("db-1.us-west2.sql.internal"),
    )
    .await;

    let endpoint = built_endpoint(&api, instance.port()).await.unwrap();
    assert_echo(&endpoint).await;
}

#[tokio::test]
async fn cas_regime_rejects_a_wrong_dns_name() {
    let fixture = Arc::new(Fixture::new());
    let (cert, key) = fixture.server_cert(None, Some("some-other-instance.sql.internal"));
    let instance = fake_instance(&fixture, cert, key).await;
    let api = mock_sqladmin(
        fixture,
        instance,
        Some("GOOGLE_MANAGED_CAS_CA"),
        Some("db-1.us-west2.sql.internal"),
    )
    .await;

    let endpoint = built_endpoint(&api, instance.port()).await.unwrap();
    endpoint.connect().await.expect_err("wrong SAN must fail");
}

#[tokio::test]
async fn a_region_mismatch_is_a_config_error() {
    let fixture = Arc::new(Fixture::new());
    let (cert, key) = fixture.server_cert(Some(INSTANCE), None);
    let instance = fake_instance(&fixture, cert, key).await;
    let api = mock_sqladmin(fixture, instance, None, None).await;

    let http = reqwest::Client::new();
    let wrong_region = InstanceName::parse("proj-1:europe-west1:db-1").unwrap();
    let error = build_endpoint(EndpointRequest {
        http: &http,
        api_base: Some(&api),
        instance: &wrong_region,
        admin_token: ADMIN_TOKEN,
        login_token: LOGIN_TOKEN,
        server_proxy_port_override: Some(instance.port()),
    })
    .await
    .map(|_| ())
    .expect_err("a region mismatch must fail");
    assert!(error.to_string().contains("region"), "{error}");
}

#[test]
fn instance_names_parse_strictly() {
    assert!(InstanceName::parse("proj:region:name").is_ok());
    for bad in [
        "proj:name",
        "proj:region:name:extra",
        "",
        "::",
        "proj::name",
    ] {
        assert!(InstanceName::parse(bad).is_err(), "{bad:?} must not parse");
    }
}
