//! The hot-set hint a post-copy destination stages for its handler: the
//! source's hot chunks, `{"hot_chunks": [<32-byte hash>, ...]}`, hottest
//! first.

use engram_chunk_store::manifest::ChunkHash;

#[derive(serde::Deserialize)]
struct HotHint {
    hot_chunks: Vec<[u8; 32]>,
}

/// Wait up to 120 s for the hint at `path`, then read it. The hint arrives
/// after the peer seal, so only an independent hint task calls this; the
/// drain never waits for it.
pub async fn load(path: &std::path::Path) -> std::io::Result<Vec<ChunkHash>> {
    tokio::time::timeout(std::time::Duration::from_secs(120), async {
        loop {
            match tokio::fs::read(path).await {
                Ok(bytes) => {
                    let hint: HotHint =
                        serde_json::from_slice(&bytes).map_err(std::io::Error::other)?;
                    let hashes = hint.hot_chunks.into_iter().map(ChunkHash::from_bytes);
                    return Ok(hashes.collect());
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    tokio::time::sleep(std::time::Duration::from_millis(2)).await;
                }
                Err(error) => return Err(error),
            }
        }
    })
    .await
    .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "hot-set hint did not arrive"))?
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[tokio::test]
    async fn read_and_parse_failures_reach_the_hint_task_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hint");
        tokio::fs::write(&path, b"invalid").await.unwrap();
        assert!(load(&path).await.is_err());
        assert!(load(dir.path()).await.is_err());
        tokio::fs::remove_file(&path).await.unwrap();
        let pending = load(&path);
        tokio::pin!(pending);
        assert!(tokio::time::timeout(Duration::from_millis(5), &mut pending)
            .await
            .is_err());
        tokio::fs::write(&path, br#"{"hot_chunks":[]}"#)
            .await
            .unwrap();
        assert!(pending.await.unwrap().is_empty());
    }
}
