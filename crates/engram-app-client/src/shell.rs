//! The session shell: ttyd frames over the orchestrator's WebSocket
//! (`GET /api/v1/sessions/:id/shell`, subprotocol `tty`).
//!
//! The framing is ttyd's, the same the web terminal speaks:
//!
//! ```text
//! client → server   first frame   JSON {AuthToken: "", columns, rows}
//!                   "0" + input   keystrokes (a text frame, or binary
//!                                 when the bytes are not UTF-8)
//!                   "1" + JSON    {columns, rows} on resize
//! server → client   0x30 + bytes  terminal output
//!                   0x31 + JSON   window title
//!                   0x32 + JSON   preferences (ignored)
//! ```
//!
//! The server authenticates after the upgrade; a refused caller sees the
//! close code 4401 (bad key) or 4404 (no such session) as the end of the
//! stream.

use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{Connector, MaybeTlsStream, WebSocketStream};

use crate::connect::{Auth, Transport};
use crate::error::{Code, Error};
use crate::sse::percent_encode_path;

const SERVER_OUTPUT: u8 = b'0';
const SERVER_TITLE: u8 = b'1';

/// One frame from the shell.
#[derive(Debug, PartialEq, Eq)]
pub enum ShellMsg {
    Output(Bytes),
    Title(String),
}

/// An open shell. Dropping it closes the socket; the server then ends
/// the PTY, so a reconnect always starts a fresh shell.
pub struct Shell {
    ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
}

impl Shell {
    /// Dial the shell and send the open frame with the terminal size.
    pub async fn open(
        transport: &Transport,
        session_id: &str,
        cols: u16,
        rows: u16,
    ) -> Result<Shell, Error> {
        let host = transport.host();
        let (scheme, rest) = if let Some(r) = host.strip_prefix("https://") {
            ("wss", r)
        } else if let Some(r) = host.strip_prefix("http://") {
            ("ws", r)
        } else {
            return Err(Error::new(
                Code::InvalidArgument,
                format!("host is not an http(s) URL: {host}"),
            ));
        };
        let url = format!(
            "{scheme}://{rest}/api/v1/sessions/{}/shell",
            percent_encode_path(session_id)
        );
        let mut req = url
            .into_client_request()
            .map_err(|e| Error::new(Code::InvalidArgument, e.to_string()))?;
        let headers = req.headers_mut();
        headers.insert("sec-websocket-protocol", HeaderValue::from_static("tty"));
        let cred = match transport.auth() {
            Auth::ApiKey(k) => Some(("x-api-key", k.clone())),
            Auth::Bearer(t) => Some(("authorization", format!("Bearer {t}"))),
            Auth::None => None,
        };
        if let Some((name, value)) = cred {
            let value = HeaderValue::from_str(&value).map_err(|_| {
                Error::new(Code::InvalidArgument, "credential is not a header value")
            })?;
            headers.insert(name, value);
        }
        let connector = if scheme == "wss" {
            let cfg = engram_tls::client_config()
                .map_err(|e| Error::new(Code::Internal, format!("tls config: {e}")))?;
            Some(Connector::Rustls(cfg))
        } else {
            Some(Connector::Plain)
        };
        let config = WebSocketConfig::default().max_message_size(Some(16 << 20));
        let (mut ws, _resp) =
            tokio_tungstenite::connect_async_tls_with_config(req, Some(config), true, connector)
                .await
                .map_err(ws_error)?;
        ws.send(Message::Text(
            format!("{{\"AuthToken\":\"\",\"columns\":{cols},\"rows\":{rows}}}").into(),
        ))
        .await
        .map_err(ws_error)?;
        Ok(Shell { ws })
    }

    /// Keystrokes. A text frame when the bytes are UTF-8 (the web's shape),
    /// else a binary frame: ttyd reads the command byte either way.
    pub async fn input(&mut self, bytes: &[u8]) -> Result<(), Error> {
        let mut payload = Vec::with_capacity(bytes.len() + 1);
        payload.push(b'0');
        payload.extend_from_slice(bytes);
        let msg = match String::from_utf8(payload) {
            Ok(s) => Message::Text(s.into()),
            Err(e) => Message::Binary(e.into_bytes().into()),
        };
        self.ws.send(msg).await.map_err(ws_error)
    }

    pub async fn resize(&mut self, cols: u16, rows: u16) -> Result<(), Error> {
        self.ws
            .send(Message::Text(
                format!("1{{\"columns\":{cols},\"rows\":{rows}}}").into(),
            ))
            .await
            .map_err(ws_error)
    }

    /// The next frame; `None` once the server closes the shell. Pings are
    /// answered by the socket, title and preference frames are decoded or
    /// dropped here.
    pub async fn next(&mut self) -> Result<Option<ShellMsg>, Error> {
        loop {
            let msg = match self.ws.next().await {
                None => return Ok(None),
                Some(Err(e)) if ended_abruptly(&e) => return Ok(None),
                Some(Err(e)) => return Err(ws_error(e)),
                Some(Ok(m)) => m,
            };
            let body = match msg {
                Message::Binary(b) => b,
                Message::Text(t) => Bytes::from(t.as_str().to_owned()),
                Message::Close(frame) => {
                    return match frame {
                        Some(f) if f.code == 4401.into() => Err(Error::new(
                            Code::Unauthenticated,
                            "the shell refused the credential",
                        )),
                        Some(f) if f.code == 4404.into() => {
                            Err(Error::new(Code::NotFound, "no such session"))
                        }
                        _ => Ok(None),
                    };
                }
                _ => continue,
            };
            match decode(body) {
                Some(m) => return Ok(Some(m)),
                None => continue,
            }
        }
    }

    pub async fn close(mut self) {
        let _ = self.ws.close(None).await;
    }
}

/// Decode one server frame; `None` for an empty frame, a preferences
/// frame or an unknown command byte.
pub fn decode(body: Bytes) -> Option<ShellMsg> {
    match *body.first()? {
        SERVER_OUTPUT => Some(ShellMsg::Output(body.slice(1..))),
        SERVER_TITLE => {
            let title = serde_json::from_slice::<String>(&body[1..])
                .unwrap_or_else(|_| String::from_utf8_lossy(&body[1..]).into_owned());
            Some(ShellMsg::Title(title))
        }
        _ => None,
    }
}

/// The server ended the connection without a close handshake: a TCP FIN
/// with no WebSocket close frame, or a TLS session cut without
/// `close_notify`. The PTY behind the socket is gone either way, so the
/// shell has simply ended; a front end that drops the TLS session when
/// the backend closes makes this the normal exit.
fn ended_abruptly(e: &tokio_tungstenite::tungstenite::Error) -> bool {
    use tokio_tungstenite::tungstenite::error::ProtocolError;
    use tokio_tungstenite::tungstenite::Error as E;
    match e {
        E::Protocol(ProtocolError::ResetWithoutClosingHandshake) => true,
        E::Io(io) => io.kind() == std::io::ErrorKind::UnexpectedEof,
        _ => false,
    }
}

fn ws_error(e: tokio_tungstenite::tungstenite::Error) -> Error {
    use tokio_tungstenite::tungstenite::Error as E;
    match e {
        E::ConnectionClosed | E::AlreadyClosed => {
            Error::new(Code::Unavailable, "the shell connection is closed")
        }
        E::Http(resp) => Error::new(
            Code::from_http_status(resp.status().as_u16()),
            format!("shell upgrade failed: HTTP {}", resp.status().as_u16()),
        ),
        E::Io(e) => Error::new(Code::Unavailable, e.to_string()),
        E::Tls(e) => Error::new(Code::Unavailable, e.to_string()),
        other => Error::new(Code::Unknown, other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_frames_decode_by_command_byte() {
        assert_eq!(
            decode(Bytes::from_static(b"0hello")),
            Some(ShellMsg::Output(Bytes::from_static(b"hello")))
        );
        assert_eq!(
            decode(Bytes::from_static(b"1\"my title\"")),
            Some(ShellMsg::Title("my title".into()))
        );
        assert_eq!(
            decode(Bytes::from_static(b"1raw")),
            Some(ShellMsg::Title("raw".into()))
        );
        assert_eq!(decode(Bytes::from_static(b"2{}")), None);
        assert_eq!(decode(Bytes::new()), None);
        assert_eq!(decode(Bytes::from_static(b"9x")), None);
    }
}
