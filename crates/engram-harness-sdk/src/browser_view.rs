//! Local, read-only image bridge for the ADR 0097 browser capability.
//!
//! Browser CLIs create PNG/JPEG/WebP observations in the guest. Text-only
//! shell output cannot make those pixels model-visible, so each harness
//! exposes a local `browser_view` tool that returns image content through its
//! native protocol. No bytes leave the guest through the Engram event stream.

use std::path::{Path, PathBuf};

pub const TOOL_NAME: &str = "browser_view";
pub const TOOL_DESCRIPTION: &str = "Inspect a browser screenshot as an internal visual observation. The image is returned to the model but is not shared with the user.";
pub const MAX_IMAGE_BYTES: u64 = 12 * 1024 * 1024;
const PENDING_VIEW_FILE: &str = "/tmp/engram-browser-observations/.pending-view";

#[derive(Debug, PartialEq, Eq)]
pub struct BrowserImage {
    pub mime_type: &'static str,
    pub base64: String,
}

pub fn enabled() -> bool {
    std::env::var("ENGRAM_BROWSER_VIEW_ENABLED").as_deref() == Ok("1")
}

pub fn input_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "path": {
                "type": "string",
                "description": "Absolute path to a PNG, JPEG, or WebP browser screenshot"
            }
        },
        "required": ["path"],
        "additionalProperties": false
    })
}

/// Load a browser observation from the only two directories the browser skill
/// is instructed to use. Canonical path checks prevent a symlink in either
/// directory from turning this model-input tool into an arbitrary file reader.
pub fn load(path: &str) -> Result<BrowserImage, String> {
    let path = Path::new(path);
    let image = load_from_roots(
        path,
        &[
            PathBuf::from("/tmp/engram-browser-observations"),
            PathBuf::from("/workspace"),
        ],
    )?;
    // The primary browser wrapper places a small interlock after an annotated
    // decision screenshot. A successful model view is the only operation that
    // clears it; this makes the skill invariant enforceable rather than merely
    // advisory, while ordinary/final screenshots remain unaffected.
    clear_matching_pending_view(path, Path::new(PENDING_VIEW_FILE));
    Ok(image)
}

fn clear_matching_pending_view(viewed: &Path, pending_file: &Path) {
    let Ok(required) = std::fs::read_to_string(pending_file) else {
        return;
    };
    let (Ok(viewed), Ok(required)) = (
        viewed.canonicalize(),
        Path::new(required.trim()).canonicalize(),
    ) else {
        return;
    };
    if viewed != required {
        return;
    }
    match std::fs::remove_file(pending_file) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => tracing::warn!(%error, "could not clear browser visual interlock"),
    }
}

fn load_from_roots(path: &Path, roots: &[PathBuf]) -> Result<BrowserImage, String> {
    if !path.is_absolute() {
        return Err("browser_view path must be absolute".into());
    }
    let canonical = path
        .canonicalize()
        .map_err(|error| format!("browser_view cannot resolve {}: {error}", path.display()))?;
    let allowed = roots.iter().any(|root| {
        root.canonicalize()
            .map(|canonical_root| canonical.starts_with(canonical_root))
            .unwrap_or(false)
    });
    if !allowed {
        return Err(format!(
            "browser_view path must be under /tmp/engram-browser-observations or /workspace: {}",
            canonical.display()
        ));
    }
    let metadata = canonical
        .metadata()
        .map_err(|error| format!("browser_view cannot stat {}: {error}", canonical.display()))?;
    if !metadata.is_file() {
        return Err("browser_view path is not a regular file".into());
    }
    if metadata.len() > MAX_IMAGE_BYTES {
        return Err(format!(
            "browser_view image is {} bytes; limit is {MAX_IMAGE_BYTES}",
            metadata.len()
        ));
    }
    let bytes = std::fs::read(&canonical)
        .map_err(|error| format!("browser_view cannot read {}: {error}", canonical.display()))?;
    let mime_type = detect_mime(&bytes)?;
    Ok(BrowserImage {
        mime_type,
        base64: encode_base64(&bytes),
    })
}

fn detect_mime(bytes: &[u8]) -> Result<&'static str, String> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Ok("image/png")
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Ok("image/jpeg")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Ok("image/webp")
    } else {
        Err("browser_view supports PNG, JPEG, and WebP content only".into())
    }
}

fn encode_base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let a = chunk[0];
        let b = chunk.get(1).copied().unwrap_or(0);
        let c = chunk.get(2).copied().unwrap_or(0);
        out.push(TABLE[(a >> 2) as usize] as char);
        out.push(TABLE[(((a & 0x03) << 4) | (b >> 4)) as usize] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(((b & 0x0f) << 2) | (c >> 6)) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[(c & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_png_inside_allowed_root() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("observation.png");
        std::fs::write(&path, b"\x89PNG\r\n\x1a\nabc").unwrap();
        let image = load_from_roots(&path, &[root.path().to_path_buf()]).unwrap();
        assert_eq!(image.mime_type, "image/png");
        assert_eq!(image.base64, "iVBORw0KGgphYmM=");
    }

    #[test]
    fn rejects_symlink_escape() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.path(), root.path().join("escape.png")).unwrap();
        #[cfg(unix)]
        assert!(load_from_roots(
            &root.path().join("escape.png"),
            &[root.path().to_path_buf()]
        )
        .is_err());
    }

    #[test]
    fn rejects_non_image_content() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("not-image.png");
        std::fs::write(&path, b"secret text").unwrap();
        assert!(load_from_roots(&path, &[root.path().to_path_buf()]).is_err());
    }

    #[test]
    fn clears_pending_view_only_for_the_required_image() {
        let root = tempfile::tempdir().unwrap();
        let required = root.path().join("required.png");
        let unrelated = root.path().join("unrelated.png");
        let pending = root.path().join(".pending-view");
        std::fs::write(&required, b"\x89PNG\r\n\x1a\nrequired").unwrap();
        std::fs::write(&unrelated, b"\x89PNG\r\n\x1a\nunrelated").unwrap();
        std::fs::write(&pending, format!("{}\n", required.display())).unwrap();

        clear_matching_pending_view(&unrelated, &pending);
        assert!(pending.exists());
        clear_matching_pending_view(&required, &pending);
        assert!(!pending.exists());
    }
}
