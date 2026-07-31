//! Session-local Google metadata emulator (ADR 0107).
//!
//! It returns only a fixed, useless placeholder. Google clients can use their
//! normal metadata ADC path, while the host egress proxy replaces the header
//! after it authorizes the session, connection, operation, and endpoint.

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

pub const GOOGLE_METADATA_PORT: u16 = 13_338;
const MAX_REQUEST_BYTES: usize = 16 * 1024;
const PLACEHOLDER: &str = "engram_google_token_placeholder";

pub async fn run() {
    let listener = match TcpListener::bind(("127.0.0.1", GOOGLE_METADATA_PORT)).await {
        Ok(listener) => listener,
        Err(error) => {
            tracing::warn!(%error, "Google metadata emulator did not bind");
            return;
        }
    };
    tracing::info!(
        port = GOOGLE_METADATA_PORT,
        "Google metadata emulator listening"
    );
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                tokio::spawn(async move {
                    let _ = serve(stream).await;
                });
            }
            Err(error) => tracing::warn!(%error, "Google metadata emulator accept failed"),
        }
    }
}

async fn serve(mut stream: TcpStream) -> std::io::Result<()> {
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
    let mut parts = request
        .lines()
        .next()
        .unwrap_or_default()
        .split_ascii_whitespace();
    let method = parts.next().unwrap_or_default();
    let path = parts
        .next()
        .unwrap_or_default()
        .split('?')
        .next()
        .unwrap_or_default();
    let (status, content_type, body) = response(method, path);
    let response = format!(
        "HTTP/1.1 {status}\r\nMetadata-Flavor: Google\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len(),
    );
    stream.write_all(response.as_bytes()).await?;
    stream.shutdown().await
}

fn response(method: &str, path: &str) -> (&'static str, &'static str, String) {
    if method != "GET" {
        return (
            "405 Method Not Allowed",
            "text/plain",
            "method not allowed".into(),
        );
    }
    let text = |value: &str| ("200 OK", "text/plain", value.to_string());
    match path {
        "/" | "/computeMetadata/v1/" => text("instance/\nproject/\n"),
        "/computeMetadata/v1/instance/service-accounts/" => text("default/\n"),
        "/computeMetadata/v1/instance/service-accounts/default/" => {
            text("email\nscopes\ntoken\n")
        }
        "/computeMetadata/v1/instance/service-accounts/default/email" => {
            text("engrams-broker@invalid")
        }
        "/computeMetadata/v1/instance/service-accounts/default/scopes" => {
            text("https://www.googleapis.com/auth/cloud-platform\n")
        }
        "/computeMetadata/v1/instance/service-accounts/default/token" => (
            "200 OK",
            "application/json",
            format!(
                "{{\"access_token\":\"{PLACEHOLDER}\",\"expires_in\":300,\"token_type\":\"Bearer\"}}"
            ),
        ),
        "/computeMetadata/v1/project/project-id" => text("engrams-broker"),
        _ => ("404 Not Found", "text/plain", "not found".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_is_an_opaque_placeholder() {
        let (_, content_type, body) = response(
            "GET",
            "/computeMetadata/v1/instance/service-accounts/default/token",
        );
        assert_eq!(content_type, "application/json");
        assert!(body.contains(PLACEHOLDER));
        assert!(!body.contains("ya29."));
    }

    #[test]
    fn rejects_unknown_and_mutating_requests() {
        assert_eq!(response("POST", "/").0, "405 Method Not Allowed");
        assert_eq!(
            response("GET", "/computeMetadata/v1/instance/attributes").0,
            "404 Not Found"
        );
    }
}
