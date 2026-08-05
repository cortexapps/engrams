//! SNI peek out of a TLS ClientHello.
//!
//! Strategy: read up to 16 KiB from the inbound stream, parse via
//! `tls_parser::parse_tls_plaintext` until we get a complete record
//! with a `ClientHello`, then walk its extensions for
//! `server_name`. The bytes are *not consumed* — the caller hands
//! them to a `BytesMut`-style buffer and the proxy replays them when
//! it opens the upstream/MITM connection.
//!
//! Rationale: rustls won't accept a `ClientHello` and re-emit it on
//! a different connection (its API takes ownership of the AEAD
//! state), so the bypass path needs raw bytes. tls-parser is a
//! pure-bytes peek that doesn't carry any state.

use std::time::Duration;

use tls_parser::{
    parse_tls_extensions, parse_tls_plaintext, TlsExtension, TlsMessage, TlsMessageHandshake,
};
use tokio::io::{AsyncRead, AsyncReadExt};

/// Peek the SNI from a TLS ClientHello on `stream`, reading up to
/// `MAX_PEEK` bytes / `BUDGET` time. Returns the SNI string and the
/// raw bytes read (so the caller can replay them upstream).
pub async fn peek_sni<R>(
    stream: &mut R,
    max_peek: usize,
    budget: Duration,
) -> Result<(String, Vec<u8>), PeekError>
where
    R: AsyncRead + Unpin,
{
    let mut buf = Vec::with_capacity(2048);
    let deadline = crate::time_source::metrics_now_tokio() + budget;

    loop {
        // Try to parse what we have. tls-parser's `parse_tls_plaintext`
        // returns Incomplete when the buffer is short of a record;
        // anything else is final.
        if !buf.is_empty() {
            match parse_tls_plaintext(&buf) {
                Ok((_, plaintext)) => {
                    // The first message after parse must be the
                    // ClientHello handshake. Reject anything else
                    // (alert, app data, etc.) — the proxy is
                    // TLS-only on this port.
                    let Some(TlsMessage::Handshake(TlsMessageHandshake::ClientHello(ch))) =
                        plaintext.msg.first()
                    else {
                        return Err(PeekError::NotClientHello);
                    };
                    let ext_bytes = ch.ext.unwrap_or_default();
                    let (_, exts) = parse_tls_extensions(ext_bytes)
                        .map_err(|_| PeekError::MalformedClientHello)?;
                    for ext in exts {
                        if let TlsExtension::SNI(entries) = ext {
                            // First name takes precedence; spec
                            // allows multiple but in practice clients
                            // only send one.
                            if let Some((_, name)) = entries.first() {
                                let s = std::str::from_utf8(name)
                                    .map_err(|_| PeekError::MalformedClientHello)?;
                                return Ok((s.to_ascii_lowercase(), buf));
                            }
                        }
                    }
                    return Err(PeekError::NoSni);
                }
                Err(tls_parser::Err::Incomplete(_)) => {
                    // Need more bytes; fall through to the read loop.
                }
                Err(_) => return Err(PeekError::MalformedClientHello),
            }
        }

        if buf.len() >= max_peek {
            return Err(PeekError::TooLarge);
        }
        let now = crate::time_source::metrics_now_tokio();
        if now >= deadline {
            return Err(PeekError::Timeout);
        }
        let remaining = deadline - now;
        let mut chunk = [0u8; 4096];
        let n = match tokio::time::timeout(remaining, stream.read(&mut chunk)).await {
            Ok(Ok(0)) => return Err(PeekError::Eof),
            Ok(Ok(n)) => n,
            Ok(Err(e)) => return Err(PeekError::Io(e)),
            Err(_) => return Err(PeekError::Timeout),
        };
        buf.extend_from_slice(&chunk[..n]);
    }
}

#[derive(Debug)]
pub enum PeekError {
    Io(std::io::Error),
    Eof,
    Timeout,
    TooLarge,
    NotClientHello,
    NoSni,
    MalformedClientHello,
}

impl std::fmt::Display for PeekError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "io: {e}"),
            Self::Eof => write!(f, "stream closed before ClientHello"),
            Self::Timeout => write!(f, "ClientHello peek timed out"),
            Self::TooLarge => write!(f, "ClientHello exceeded peek budget"),
            Self::NotClientHello => {
                write!(f, "first TLS message wasn't a ClientHello")
            }
            Self::NoSni => write!(f, "ClientHello had no SNI extension"),
            Self::MalformedClientHello => write!(f, "malformed ClientHello"),
        }
    }
}

impl std::error::Error for PeekError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Build a synthetic ClientHello with the given SNI. Wire format
    /// is fiddly, so we hand-roll it here rather than relying on
    /// rustls (which would couple the test to its serialization).
    fn client_hello_with_sni(sni: &str) -> Vec<u8> {
        // SNI extension body: list_length:u16 + (name_type:u8 + name_length:u16 + name)
        let name_bytes = sni.as_bytes();
        let mut sni_ext = Vec::new();
        let entry_len = 1 + 2 + name_bytes.len();
        sni_ext.extend_from_slice(&(entry_len as u16).to_be_bytes());
        sni_ext.push(0u8); // name_type = host_name
        sni_ext.extend_from_slice(&(name_bytes.len() as u16).to_be_bytes());
        sni_ext.extend_from_slice(name_bytes);

        // Extension wrapper: ext_type:u16 (0x0000 = server_name) + ext_data_length:u16 + body
        let mut ext = Vec::new();
        ext.extend_from_slice(&0u16.to_be_bytes());
        ext.extend_from_slice(&(sni_ext.len() as u16).to_be_bytes());
        ext.extend_from_slice(&sni_ext);

        let mut extensions = Vec::new();
        extensions.extend_from_slice(&(ext.len() as u16).to_be_bytes());
        extensions.extend_from_slice(&ext);

        // ClientHello body:
        //   client_version(2) + random(32) + session_id(1+0)
        //   + cipher_suites(2 + content) + compression_methods(1+1)
        //   + extensions(2 + content)
        let mut hello = Vec::new();
        hello.extend_from_slice(&[0x03, 0x03]); // TLS 1.2 record version
        hello.extend_from_slice(&[0u8; 32]); // random
        hello.push(0); // session_id length 0
        hello.extend_from_slice(&[0x00, 0x02, 0x13, 0x01]); // 1 cipher suite (TLS_AES_128_GCM_SHA256)
        hello.extend_from_slice(&[0x01, 0x00]); // compression: 1 method, null
        hello.extend_from_slice(&extensions);

        // Handshake header: msg_type(1)=ClientHello + length(3) + body
        let mut hs = Vec::new();
        hs.push(1u8);
        let hs_len = hello.len() as u32;
        hs.extend_from_slice(&[
            ((hs_len >> 16) & 0xff) as u8,
            ((hs_len >> 8) & 0xff) as u8,
            (hs_len & 0xff) as u8,
        ]);
        hs.extend_from_slice(&hello);

        // TLS record: content_type(1)=Handshake + version(2) + length(2) + payload
        let mut record = Vec::new();
        record.push(0x16);
        record.extend_from_slice(&[0x03, 0x01]); // record version (TLS 1.0 fine)
        record.extend_from_slice(&(hs.len() as u16).to_be_bytes());
        record.extend_from_slice(&hs);
        record
    }

    #[tokio::test]
    async fn extracts_sni_from_well_formed_client_hello() {
        let bytes = client_hello_with_sni("api.github.com");
        let mut cur = Cursor::new(bytes.clone());
        let (sni, replay) = peek_sni(&mut cur, 16 * 1024, Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(sni, "api.github.com");
        assert_eq!(replay, bytes);
    }

    #[tokio::test]
    async fn lowercases_sni() {
        let bytes = client_hello_with_sni("API.GitHub.COM");
        let mut cur = Cursor::new(bytes);
        let (sni, _) = peek_sni(&mut cur, 16 * 1024, Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(sni, "api.github.com");
    }

    #[tokio::test]
    async fn rejects_garbage() {
        // Plain HTTP. tls-parser reads the first 5 bytes as a TLS
        // record header — `G` (0x47) becomes a content type,
        // `ET HT` becomes the length+version. With only 18 bytes we
        // run out before the parser declares Failure (it asks for
        // more). On Eof we return PeekError::Eof. Either Eof or
        // MalformedClientHello is acceptable — the point of this
        // test is "non-TLS data does not produce a successful
        // SNI extraction."
        let mut cur = Cursor::new(b"GET / HTTP/1.1\r\n\r\n".to_vec());
        let err = peek_sni(&mut cur, 16 * 1024, Duration::from_secs(1))
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            PeekError::MalformedClientHello | PeekError::NotClientHello | PeekError::Eof,
        ));
    }

    #[tokio::test]
    async fn times_out_on_truncated_client_hello() {
        // Send only the record header, never complete the body.
        let mut cur = Cursor::new(vec![0x16, 0x03, 0x01, 0x01, 0x00]);
        let err = peek_sni(&mut cur, 16 * 1024, Duration::from_millis(50))
            .await
            .unwrap_err();
        assert!(matches!(err, PeekError::Timeout | PeekError::Eof));
    }
}
