//! The shell client against an in-process WebSocket stub: the upgrade
//! carries the key and the `tty` subprotocol, the open frame carries the
//! size, frames are decoded by command byte, input and resize frames have
//! ttyd's shape, and the server's close codes become errors.

use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::Path;
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use bytes::Bytes;

use engram_app_client::shell::{Shell, ShellMsg};
use engram_app_client::{Auth, Code, Transport};

async fn shell_route(
    Path(id): Path<String>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    assert_eq!(headers.get("x-api-key").unwrap(), "k-1");
    assert_eq!(headers.get("sec-websocket-protocol").unwrap(), "tty");
    ws.protocols(["tty"])
        .on_upgrade(move |socket| async move { serve(socket, &id).await })
}

async fn serve(mut socket: WebSocket, id: &str) {
    if id == "s-abrupt" {
        // The PTY ended and the server dropped the socket with no close
        // frame (a TCP FIN only).
        let _ = socket.recv().await;
        drop(socket);
        return;
    }
    if id == "s-gone" {
        let _ = socket
            .send(Message::Close(Some(CloseFrame {
                code: 4401,
                reason: "unauthorized".into(),
            })))
            .await;
        return;
    }
    let open = socket.recv().await.unwrap().unwrap();
    let Message::Text(t) = open else {
        panic!("open frame is text, got {open:?}");
    };
    assert_eq!(t.as_str(), r#"{"AuthToken":"","columns":120,"rows":40}"#);
    socket
        .send(Message::Binary(Bytes::from_static(b"0$ hello")))
        .await
        .unwrap();
    socket
        .send(Message::Binary(Bytes::from_static(b"1\"my shell\"")))
        .await
        .unwrap();
    socket
        .send(Message::Binary(Bytes::from_static(b"2{}")))
        .await
        .unwrap();
    // A legacy text output frame decodes the same way.
    socket.send(Message::Text("0more".into())).await.unwrap();
    let input = socket.recv().await.unwrap().unwrap();
    assert!(
        matches!(&input, Message::Text(t) if t.as_str() == "0ls\r"),
        "{input:?}"
    );
    let raw = socket.recv().await.unwrap().unwrap();
    assert!(
        matches!(&raw, Message::Binary(b) if b.as_ref() == b"0\xff\xfe"),
        "{raw:?}"
    );
    let resize = socket.recv().await.unwrap().unwrap();
    assert!(
        matches!(&resize, Message::Text(t) if t.as_str() == r#"1{"columns":80,"rows":24}"#),
        "{resize:?}"
    );
    let _ = socket.send(Message::Close(None)).await;
}

async fn start_stub() -> String {
    let app = Router::new().route("/api/v1/sessions/{id}/shell", get(shell_route));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

#[tokio::test]
async fn frames_round_trip_in_ttyd_shape() {
    let host = start_stub().await;
    let t = Transport::new(host, Auth::ApiKey("k-1".into()));
    let mut shell = Shell::open(&t, "s-1", 120, 40).await.unwrap();
    assert_eq!(
        shell.next().await.unwrap(),
        Some(ShellMsg::Output(Bytes::from_static(b"$ hello")))
    );
    assert_eq!(
        shell.next().await.unwrap(),
        Some(ShellMsg::Title("my shell".into()))
    );
    // The preferences frame is dropped; the legacy text frame is output.
    assert_eq!(
        shell.next().await.unwrap(),
        Some(ShellMsg::Output(Bytes::from_static(b"more")))
    );
    shell.input(b"ls\r").await.unwrap();
    shell.input(b"\xff\xfe").await.unwrap();
    shell.resize(80, 24).await.unwrap();
    assert_eq!(shell.next().await.unwrap(), None, "server closed");
}

#[tokio::test]
async fn a_4401_close_is_unauthenticated() {
    let host = start_stub().await;
    let t = Transport::new(host, Auth::ApiKey("k-1".into()));
    let mut shell = Shell::open(&t, "s-gone", 120, 40).await.unwrap();
    let err = shell.next().await.unwrap_err();
    assert_eq!(err.code, Code::Unauthenticated);
}

#[test]
fn a_non_http_host_is_refused() {
    let t = Transport::new("ftp://x", Auth::None);
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let code = rt
        .block_on(Shell::open(&t, "s", 1, 1))
        .err()
        .map(|e| e.code);
    assert_eq!(code, Some(Code::InvalidArgument));
}

#[tokio::test]
async fn a_socket_dropped_without_a_close_frame_is_a_normal_end() {
    let url = start_stub().await;
    let t = Transport::new(&url, Auth::ApiKey("k-1".into()));
    let mut shell = Shell::open(&t, "s-abrupt", 120, 40).await.unwrap();
    assert!(matches!(shell.next().await, Ok(None)));
}
