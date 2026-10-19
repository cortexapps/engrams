//! The Connect protocol over HTTP/1.1: unary calls and server streams.
//!
//! Hand-rolled on purpose. The client needs exactly two call shapes against
//! one server, and the protocol is small:
//!
//! - **Unary**: `POST {host}/rpc/engram.app.v1.<Service>/<Method>` with
//!   `content-type: application/proto` and the encoded request as the body.
//!   `200` carries the encoded response; any other status carries a JSON
//!   `{"code": "...", "message": "..."}` (or nothing, in which case the HTTP
//!   status maps to a code).
//! - **Server streaming**: the same POST with `application/connect+proto`.
//!   Both directions are *enveloped*: `[flags: u8][len: u32 BE][payload]`.
//!   Flag `0x02` marks the end-of-stream envelope, whose payload is JSON
//!   `{"error": {...}?, "metadata": {...}?}`.
//!
//! Compression is never negotiated (no `accept-encoding`, flag `0x01` is an
//! error), the binary codec is used throughout, and no timeouts are set
//! because streams stay open for as long as the session does.

use bytes::{Buf, Bytes, BytesMut};
use futures::StreamExt;
use prost::Message;

use crate::error::{Code, Error};

/// How a request authenticates. `x-api-key` is the normal path; `Bearer` is
/// the device-flow session used once, during `auth login`, to mint the key.
#[derive(Debug, Clone)]
pub enum Auth {
    ApiKey(String),
    Bearer(String),
    None,
}

/// One (host, credential) pair and the HTTP client behind it.
#[derive(Debug, Clone)]
pub struct Transport {
    http: reqwest::Client,
    host: String,
    auth: Auth,
}

const PROTOCOL_VERSION: &str = "1";
const FLAG_COMPRESSED: u8 = 0x01;
const FLAG_END_STREAM: u8 = 0x02;

impl Transport {
    /// `host` is the orchestrator base URL with no trailing slash
    /// (see [`crate::config::normalize_host`]).
    pub fn new(host: impl Into<String>, auth: Auth) -> Self {
        let http = engram_tls::client_builder()
            .no_gzip()
            .no_brotli()
            .no_deflate()
            .no_zstd()
            .build()
            .expect("reqwest client builds once the TLS provider is installed");
        Transport {
            http,
            host: host.into(),
            auth,
        }
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn auth(&self) -> &Auth {
        &self.auth
    }

    /// The reqwest client, for the plain HTTP routes (SSE, device flow).
    pub fn http(&self) -> &reqwest::Client {
        &self.http
    }

    /// Add this transport's credential to a request.
    pub fn authorize(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.auth {
            Auth::ApiKey(k) => req.header("x-api-key", k),
            Auth::Bearer(t) => req.header("authorization", format!("Bearer {t}")),
            Auth::None => req,
        }
    }

    fn rpc_url(&self, service: &str, method: &str) -> String {
        format!("{}/rpc/engram.app.v1.{service}/{method}", self.host)
    }

    /// One unary call.
    pub async fn unary<Req: Message, Resp: Message + Default>(
        &self,
        service: &str,
        method: &str,
        req: &Req,
    ) -> Result<Resp, Error> {
        let resp = self
            .authorize(self.http.post(self.rpc_url(service, method)))
            .header("content-type", "application/proto")
            .header("connect-protocol-version", PROTOCOL_VERSION)
            .body(req.encode_to_vec())
            .send()
            .await?;
        let status = resp.status().as_u16();
        let body = resp.bytes().await?;
        if status != 200 {
            return Err(error_from_body(status, &body));
        }
        Resp::decode(body).map_err(|e| Error::new(Code::Internal, format!("decode response: {e}")))
    }

    /// One server-streaming call. Messages arrive through
    /// [`ServerStream::next`]; the end-of-stream error (if any) is the last
    /// item.
    pub async fn server_stream<Req: Message, Resp: Message + Default>(
        &self,
        service: &str,
        method: &str,
        req: &Req,
    ) -> Result<ServerStream<Resp>, Error> {
        let encoded = req.encode_to_vec();
        let mut body = Vec::with_capacity(5 + encoded.len());
        body.push(0);
        body.extend_from_slice(&(encoded.len() as u32).to_be_bytes());
        body.extend_from_slice(&encoded);
        let resp = self
            .authorize(self.http.post(self.rpc_url(service, method)))
            .header("content-type", "application/connect+proto")
            .header("connect-protocol-version", PROTOCOL_VERSION)
            .body(body)
            .send()
            .await?;
        let status = resp.status().as_u16();
        if status != 200 {
            let body = resp.bytes().await.unwrap_or_default();
            return Err(error_from_body(status, &body));
        }
        Ok(ServerStream {
            body: Box::pin(resp.bytes_stream()),
            buf: BytesMut::new(),
            done: false,
            _resp: std::marker::PhantomData,
        })
    }
}

/// Decode a non-200 response: a Connect error JSON body if there is one,
/// else the HTTP status alone.
fn error_from_body(status: u16, body: &[u8]) -> Error {
    #[derive(serde::Deserialize)]
    struct Wire {
        code: Option<String>,
        message: Option<String>,
    }
    if let Ok(w) = serde_json::from_slice::<Wire>(body) {
        if let Some(code) = w.code {
            return Error::new(Code::from_wire(&code), w.message.unwrap_or_default());
        }
    }
    Error::new(Code::from_http_status(status), format!("HTTP {status}"))
}

type BodyStream =
    std::pin::Pin<Box<dyn futures::Stream<Item = Result<Bytes, reqwest::Error>> + Send>>;

/// The response side of a server-streaming call.
pub struct ServerStream<Resp> {
    body: BodyStream,
    buf: BytesMut,
    done: bool,
    _resp: std::marker::PhantomData<fn() -> Resp>,
}

impl<Resp: Message + Default> ServerStream<Resp> {
    /// The next message, `None` once the end-of-stream envelope has been
    /// consumed. A stream that ends without that envelope is an error (the
    /// connection dropped mid-call).
    pub async fn next(&mut self) -> Option<Result<Resp, Error>> {
        if self.done {
            return None;
        }
        loop {
            if let Some((flags, payload)) = take_envelope(&mut self.buf) {
                if flags & FLAG_COMPRESSED != 0 {
                    self.done = true;
                    return Some(Err(Error::new(
                        Code::Internal,
                        "received a compressed envelope but none was negotiated",
                    )));
                }
                if flags & FLAG_END_STREAM != 0 {
                    self.done = true;
                    return end_stream_error(&payload).map(Err);
                }
                return Some(
                    Resp::decode(payload)
                        .map_err(|e| Error::new(Code::Internal, format!("decode message: {e}"))),
                );
            }
            match self.body.next().await {
                Some(Ok(chunk)) => self.buf.extend_from_slice(&chunk),
                Some(Err(e)) => {
                    self.done = true;
                    return Some(Err(Error::transport(e)));
                }
                None => {
                    self.done = true;
                    return Some(Err(Error::new(
                        Code::Unavailable,
                        "stream ended before the end-of-stream envelope",
                    )));
                }
            }
        }
    }
}

/// Split one complete envelope off the front of `buf`, if present.
fn take_envelope(buf: &mut BytesMut) -> Option<(u8, Bytes)> {
    if buf.len() < 5 {
        return None;
    }
    let flags = buf[0];
    let len = u32::from_be_bytes([buf[1], buf[2], buf[3], buf[4]]) as usize;
    if buf.len() < 5 + len {
        return None;
    }
    buf.advance(5);
    Some((flags, buf.split_to(len).freeze()))
}

/// The error carried by an end-of-stream envelope, if any.
fn end_stream_error(payload: &[u8]) -> Option<Error> {
    #[derive(serde::Deserialize)]
    struct End {
        error: Option<EndError>,
    }
    #[derive(serde::Deserialize)]
    struct EndError {
        code: Option<String>,
        message: Option<String>,
    }
    match serde_json::from_slice::<End>(payload) {
        Ok(End { error: Some(e) }) => Some(Error::new(
            e.code
                .as_deref()
                .map(Code::from_wire)
                .unwrap_or(Code::Unknown),
            e.message.unwrap_or_default(),
        )),
        Ok(End { error: None }) => None,
        Err(e) => Some(Error::new(
            Code::Internal,
            format!("decode end-of-stream: {e}"),
        )),
    }
}

/// Encode one envelope (for tests and stub servers).
pub fn envelope(flags: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(5 + payload.len());
    out.push(flags);
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(payload);
    out
}

/// The end-of-stream envelope with no error.
pub fn end_stream_ok() -> Vec<u8> {
    envelope(FLAG_END_STREAM, b"{}")
}

/// The end-of-stream envelope carrying an error.
pub fn end_stream_err(code: &str, message: &str) -> Vec<u8> {
    let json = serde_json::json!({ "error": { "code": code, "message": message } });
    envelope(FLAG_END_STREAM, json.to_string().as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelopes_split_at_frame_boundaries_only() {
        let mut buf = BytesMut::new();
        buf.extend_from_slice(&envelope(0, b"abc")[..6]);
        assert!(take_envelope(&mut buf).is_none(), "partial frame must wait");
        buf.extend_from_slice(&envelope(0, b"abc")[6..]);
        buf.extend_from_slice(&end_stream_ok());
        assert_eq!(
            take_envelope(&mut buf),
            Some((0, Bytes::from_static(b"abc")))
        );
        let (flags, payload) = take_envelope(&mut buf).unwrap();
        assert_eq!(flags, FLAG_END_STREAM);
        assert!(end_stream_error(&payload).is_none());
        assert!(buf.is_empty());
    }

    #[test]
    fn end_stream_error_maps_code() {
        let e = end_stream_error(br#"{"error":{"code":"not_found","message":"gone"}}"#).unwrap();
        assert_eq!(e.code, Code::NotFound);
        assert_eq!(e.to_string(), "NotFound: gone");
    }

    #[test]
    fn http_error_body_and_status_fallback() {
        let e = error_from_body(401, br#"{"code":"unauthenticated","message":"bad key"}"#);
        assert_eq!(e.to_string(), "Unauthenticated: bad key");
        let e = error_from_body(503, b"<html>");
        assert_eq!(e.to_string(), "Unavailable: HTTP 503");
    }
}
