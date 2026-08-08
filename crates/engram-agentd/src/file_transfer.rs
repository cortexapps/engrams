//! Host-side driver for ADR 0113's chunked agentd file protocol.

use bytes::Bytes;
use engram_core::types::sandbox::{
    SessionFileMetadata, SessionFileSpec, SessionFileStream, MAX_SESSION_FILE_BYTES,
};
use engram_core::SandboxError;
use futures::StreamExt;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;

use crate::proto::{read_msg, write_msg, WireFileChunk, WireRequest, WireResponse};

fn vm_error(context: &str, error: impl std::fmt::Display) -> SandboxError {
    SandboxError::Vm(format!("{context}: {error}").into())
}

fn digest_hex(digest: impl AsRef<[u8]>) -> String {
    digest
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub async fn upload_file<S>(
    mut connection: S,
    spec: SessionFileSpec,
    mut source: SessionFileStream,
) -> Result<SessionFileMetadata, SandboxError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    write_msg(
        &mut connection,
        &WireRequest::UploadStream {
            path: spec.path.clone(),
            size_bytes: spec.size_bytes,
            sha256: spec.sha256.clone(),
        },
    )
    .await
    .map_err(|error| vm_error("send UploadStream metadata", error))?;
    let mut sent = 0u64;
    while let Some(item) = source.next().await {
        let chunk = item?;
        sent = sent
            .checked_add(chunk.len() as u64)
            .ok_or_else(|| SandboxError::InvalidSpec("upload byte count overflow".into()))?;
        if sent > spec.size_bytes || sent > MAX_SESSION_FILE_BYTES {
            return Err(SandboxError::InvalidSpec(
                "upload stream exceeds its declared size".into(),
            ));
        }
        write_msg(
            &mut connection,
            &WireFileChunk {
                bytes: chunk.to_vec(),
            },
        )
        .await
        .map_err(|error| vm_error("send UploadStream chunk", error))?;
    }
    if sent != spec.size_bytes {
        return Err(SandboxError::InvalidSpec(format!(
            "upload ended after {sent} bytes; expected {}",
            spec.size_bytes
        )));
    }
    match read_msg::<_, WireResponse>(&mut connection).await {
        Ok(WireResponse::UploadStreamOk { size_bytes, sha256 }) => {
            if size_bytes != spec.size_bytes || sha256 != spec.sha256 {
                return Err(SandboxError::Unavailable(format!(
                    "agentd upload verification response mismatch: expected {} bytes {}, received {size_bytes} bytes {sha256}",
                    spec.size_bytes, spec.sha256
                )));
            }
            Ok(SessionFileMetadata {
                path: spec.path,
                size_bytes,
                sha256,
            })
        }
        Ok(WireResponse::Error { kind, message }) => Err(SandboxError::Vm(
            format!("agentd rejected UploadStream ({kind}): {message}").into(),
        )),
        Ok(other) => Err(SandboxError::Vm(
            format!("unexpected UploadStream response: {other:?}").into(),
        )),
        Err(error) => Err(vm_error("read UploadStream response", error)),
    }
}

pub async fn read_file<S>(
    mut connection: S,
    path: String,
) -> Result<(SessionFileMetadata, SessionFileStream), SandboxError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    write_msg(
        &mut connection,
        &WireRequest::DownloadStream { path: path.clone() },
    )
    .await
    .map_err(|error| vm_error("send DownloadStream request", error))?;
    let (size_bytes, sha256) = match read_msg::<_, WireResponse>(&mut connection).await {
        Ok(WireResponse::DownloadStreamReady { size_bytes, sha256 }) => (size_bytes, sha256),
        Ok(WireResponse::Error { kind, message }) => {
            return Err(SandboxError::Vm(
                format!("agentd rejected DownloadStream ({kind}): {message}").into(),
            ));
        }
        Ok(other) => {
            return Err(SandboxError::Vm(
                format!("unexpected DownloadStream response: {other:?}").into(),
            ));
        }
        Err(error) => return Err(vm_error("read DownloadStream metadata", error)),
    };
    if size_bytes > MAX_SESSION_FILE_BYTES {
        return Err(SandboxError::InvalidSpec(format!(
            "file exceeds {MAX_SESSION_FILE_BYTES} bytes"
        )));
    }
    let metadata = SessionFileMetadata {
        path,
        size_bytes,
        sha256: sha256.clone(),
    };
    let (tx, rx) = mpsc::channel(8);
    tokio::spawn(async move {
        let mut received = 0u64;
        let mut hasher = Sha256::new();
        while received < size_bytes {
            let WireFileChunk { bytes } = match read_msg(&mut connection).await {
                Ok(frame) => frame,
                Err(error) => {
                    let _ = tx
                        .send(Err(vm_error("read DownloadStream chunk", error)))
                        .await;
                    return;
                }
            };
            let next = match received.checked_add(bytes.len() as u64) {
                Some(next) if next <= size_bytes => next,
                _ => {
                    let _ = tx
                        .send(Err(SandboxError::Unavailable(
                            "download stream exceeds declared size".into(),
                        )))
                        .await;
                    return;
                }
            };
            hasher.update(&bytes);
            received = next;
            if tx.send(Ok(Bytes::from(bytes))).await.is_err() {
                return;
            }
        }
        let actual_sha256 = digest_hex(hasher.finalize());
        if actual_sha256 != sha256 {
            let _ = tx
                .send(Err(SandboxError::Unavailable(format!(
                    "download SHA-256 mismatch: expected {sha256}, received {actual_sha256}"
                ))))
                .await;
        }
    });
    Ok((
        metadata,
        Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx)),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::{read_msg, write_msg, WireFileChunk, WireRequest, WireResponse};
    use futures::stream;

    #[tokio::test]
    async fn upload_streams_more_than_the_old_unary_limit_with_exact_bytes() {
        let bytes = vec![0x5a; 3 * 1024 * 1024 + 17];
        let sha256 = digest_hex(Sha256::digest(&bytes));
        let expected = bytes.clone();
        let (client, mut server) = tokio::io::duplex(128 * 1024);
        let server_task = tokio::spawn(async move {
            let request = read_msg::<_, WireRequest>(&mut server).await.unwrap();
            let (size_bytes, sha256) = match request {
                WireRequest::UploadStream {
                    size_bytes, sha256, ..
                } => (size_bytes, sha256),
                other => panic!("unexpected request: {other:?}"),
            };
            let mut received = Vec::with_capacity(size_bytes as usize);
            while received.len() < size_bytes as usize {
                let WireFileChunk { bytes } = read_msg(&mut server).await.unwrap();
                received.extend_from_slice(&bytes);
            }
            assert_eq!(received, expected);
            write_msg(
                &mut server,
                &WireResponse::UploadStreamOk { size_bytes, sha256 },
            )
            .await
            .unwrap();
        });
        let chunks = bytes
            .chunks(64 * 1024)
            .map(|chunk| Ok(Bytes::copy_from_slice(chunk)))
            .collect::<Vec<_>>();
        let result = upload_file(
            client,
            SessionFileSpec {
                path: "/tmp/uploads/019fe2ff-0464-75f3-bb20-a8c1844579b9/large.bin".into(),
                size_bytes: bytes.len() as u64,
                sha256: sha256.clone(),
            },
            Box::pin(stream::iter(chunks)),
        )
        .await
        .unwrap();
        assert_eq!(result.size_bytes, bytes.len() as u64);
        assert_eq!(result.sha256, sha256);
        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn upload_rejects_a_truncated_source_before_acceptance() {
        let (client, _server) = tokio::io::duplex(64 * 1024);
        let error = upload_file(
            client,
            SessionFileSpec {
                path: "/tmp/uploads/019fe2ff-0464-75f3-bb20-a8c1844579b9/short.bin".into(),
                size_bytes: 4,
                sha256: digest_hex(Sha256::digest(b"four")),
            },
            Box::pin(stream::iter([Ok(Bytes::from_static(b"bad"))])),
        )
        .await
        .unwrap_err();
        assert!(matches!(error, SandboxError::InvalidSpec(_)));
    }

    #[tokio::test]
    async fn download_reports_checksum_mismatch_after_the_stream() {
        let (client, mut server) = tokio::io::duplex(64 * 1024);
        let server_task = tokio::spawn(async move {
            assert!(matches!(
                read_msg::<_, WireRequest>(&mut server).await.unwrap(),
                WireRequest::DownloadStream { .. }
            ));
            write_msg(
                &mut server,
                &WireResponse::DownloadStreamReady {
                    size_bytes: 3,
                    sha256: digest_hex(Sha256::digest(b"other")),
                },
            )
            .await
            .unwrap();
            write_msg(
                &mut server,
                &WireFileChunk {
                    bytes: b"bad".to_vec(),
                },
            )
            .await
            .unwrap();
        });
        let (_, mut stream) = read_file(client, "/tmp/uploads/id/file".into())
            .await
            .unwrap();
        assert_eq!(
            stream.next().await.unwrap().unwrap(),
            Bytes::from_static(b"bad")
        );
        assert!(stream.next().await.unwrap().is_err());
        server_task.await.unwrap();
    }
}
