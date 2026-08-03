//! ADR 0026 in-guest artifact-share client.
//!
//! Invoked as a subcommand of the baked `engram-agentd` binary — the
//! dogfood image's `engram-share` wrapper runs `engram-agentd
//! share-file --file <path> [--caption "..."]`. We dial the host on
//! `UPLOAD_VSOCK_PORT`, send one [`UploadRequest`] header frame
//! authenticated by the per-session broker token (`ENGRAM_UPLOAD_TOKEN`),
//! stream the raw file body, then read one [`UploadResponse`] and print
//! the result. The shared artifact surfaces in the session's
//! conversation history (the web UI renders it inline).
//!
//! This is the *untrusted* surface: any file type may be shared, but
//! the coord is the authority on the stored media type — it stamps it
//! from magic bytes (falling back to the declared extension for types
//! with no magic bytes) and never trusts a guest-supplied MIME. The
//! serve side applies MIME-agnostic hardening.

use std::{io::ErrorKind, process::ExitCode, time::Duration};

use engram_core::SessionId;
use engram_harness_proto::{
    read_msg, write_msg, UploadOp, UploadRequest, UploadResponse, MAX_ARTIFACT_BYTES,
    UPLOAD_VSOCK_PORT,
};
use tokio::io::AsyncReadExt;

/// Entry from `main`: `rest` is the argv after `share-file`. Builds a
/// small runtime and runs one upload. On success prints a confirmation
/// (artifact id + media type) to stdout.
pub fn run(rest: Vec<String>) -> ExitCode {
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("engram-agentd share-file: runtime: {e}");
            return ExitCode::from(1);
        }
    };
    match rt.block_on(run_inner(&rest)) {
        Ok(out) => {
            print!("{out}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("engram-agentd share-file: {e}");
            ExitCode::from(1)
        }
    }
}

async fn run_inner(rest: &[String]) -> Result<String, String> {
    let session_id = env_session_id()?;
    let broker_token = std::env::var("ENGRAM_UPLOAD_TOKEN")
        .map_err(|_| "ENGRAM_UPLOAD_TOKEN not set in the guest env".to_string())?;
    let path = req_flag(rest, "--file")?;
    let caption = flag(rest, "--caption").filter(|s| !s.is_empty());

    // Any file type is shareable; the extension only helps the coord
    // stamp types that have no magic bytes (md, html, …).
    let ext = std::path::Path::new(&path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    let file_name = std::path::Path::new(&path)
        .file_name()
        .and_then(|n| n.to_str())
        .map(str::to_string)
        .ok_or_else(|| format!("{path}: no file name"))?;

    let meta = wait_for_file(&path).await?;
    if !meta.is_file() {
        return Err(format!("{path}: not a regular file"));
    }
    let size_bytes = meta.len();
    if size_bytes == 0 {
        return Err(format!("{path}: file is empty"));
    }
    if size_bytes > MAX_ARTIFACT_BYTES {
        return Err(format!(
            "{path}: {size_bytes} bytes exceeds the artifact size limit ({MAX_ARTIFACT_BYTES})"
        ));
    }

    let req = UploadRequest {
        session_id,
        broker_token,
        op: UploadOp::ShareFileNamed {
            ext,
            file_name,
            caption,
            size_bytes,
        },
    };

    let transport = engram_transport::from_env().map_err(|e| format!("transport: {e}"))?;
    let mut conn = transport
        .dial(UPLOAD_VSOCK_PORT)
        .await
        .map_err(|e| format!("dial host upload port {UPLOAD_VSOCK_PORT}: {e}"))?;
    write_msg(&mut conn, &req)
        .await
        .map_err(|e| format!("send upload request: {e}"))?;

    // Raw body: exactly `size_bytes` bytes follow the header frame. Cap
    // the copy at the stat'd size so a file that grew mid-upload can't
    // bleed into the response frame; the host reads exactly `size_bytes`.
    let file = tokio::fs::File::open(&path)
        .await
        .map_err(|e| format!("{path}: open: {e}"))?;
    let mut body = file.take(size_bytes);
    let copied = tokio::io::copy(&mut body, &mut conn)
        .await
        .map_err(|e| format!("stream file body: {e}"))?;
    if copied != size_bytes {
        return Err(format!(
            "{path}: read {copied} bytes but expected {size_bytes} (file changed under us?)"
        ));
    }

    let resp: UploadResponse = read_msg(&mut conn)
        .await
        .map_err(|e| format!("read upload response: {e}"))?;
    match resp {
        UploadResponse::Shared {
            artifact_id,
            media_type,
            size_bytes,
        } => Ok(format!(
            "shared {media_type} ({size_bytes} bytes) as artifact {artifact_id}\n"
        )),
        UploadResponse::Error { message } => Err(message),
    }
}

/// A model harness can dispatch screenshot creation and sharing concurrently.
/// Give the producer a short bounded window to publish a non-empty file so one
/// explicit share intent does not turn into a spurious retry or duplicate.
async fn wait_for_file(path: &str) -> Result<std::fs::Metadata, String> {
    let deadline = crate::time_source::metrics_now_tokio() + Duration::from_secs(2);
    loop {
        match tokio::fs::metadata(path).await {
            Ok(meta) if !meta.is_file() => return Err(format!("{path}: not a regular file")),
            Ok(meta) if meta.len() > 0 => return Ok(meta),
            Ok(_) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(format!("{path}: stat: {error}")),
        }
        if crate::time_source::metrics_now_tokio() >= deadline {
            return Err(format!(
                "{path}: file did not become ready within 2 seconds"
            ));
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

fn env_session_id() -> Result<SessionId, String> {
    let s = std::env::var("ENGRAM_SESSION_ID")
        .map_err(|_| "ENGRAM_SESSION_ID not set in the guest env".to_string())?;
    s.parse()
        .map_err(|_| format!("invalid ENGRAM_SESSION_ID: {s}"))
}

/// Value following `name` in `argv`, if present.
fn flag(argv: &[String], name: &str) -> Option<String> {
    argv.iter()
        .position(|a| a == name)
        .and_then(|i| argv.get(i + 1))
        .cloned()
}

fn req_flag(argv: &[String], name: &str) -> Result<String, String> {
    flag(argv, name).ok_or_else(|| format!("missing required flag {name}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flag_parses_value_and_presence() {
        let argv: Vec<String> = ["--file", "/tmp/shot.png", "--caption", "hi"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(flag(&argv, "--file").as_deref(), Some("/tmp/shot.png"));
        assert_eq!(flag(&argv, "--caption").as_deref(), Some("hi"));
        assert!(flag(&argv, "--missing").is_none());
        assert!(req_flag(&argv, "--file").is_ok());
        assert!(req_flag(&argv, "--nope").is_err());
    }

    #[tokio::test]
    async fn waits_for_a_concurrently_created_share_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("evidence.png");
        let producer_path = path.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(30)).await;
            tokio::fs::write(producer_path, b"png").await.unwrap();
        });

        let meta = wait_for_file(path.to_str().unwrap()).await.unwrap();
        assert_eq!(meta.len(), 3);
    }
}
