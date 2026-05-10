use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::ids::{HostId, SandboxId, SessionId};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Pending,
    Active,
    Idle,
    /// The session's hot snapshot was flushed to cold-tier blob
    /// storage (disk-pressure flush, or admin-triggered explicit
    /// flush). Resume is still possible — the scheduler picks a host
    /// with the matching OCI manifest digest cached, downloads the
    /// blob, untars, restores. Distinct from `Idle` (which means hot
    /// snapshot still on local NVMe) and `Dead` (terminal — the cold
    /// blob is gone too). See ADR 0005.
    ColdEvicted,
    Completed,
    Failed,
    /// Terminal: no snapshot remains in either tier (host crashed
    /// before flush, blob deleted, KEK lost, intentional GC). Engram
    /// is a one-shot task runner — `Dead` ends the session. Callers
    /// either accept the loss or start a fresh session.
    Dead,
}

impl SessionStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Active => "active",
            Self::Idle => "idle",
            Self::ColdEvicted => "cold_evicted",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Dead => "dead",
        }
    }
}

/// Full OCI reference (registry host + repo path + tag) for the
/// image this session boots from. Examples:
///   - `ghcr.io/cortex/api:warm-2026-01-01T00:00:00Z`
///   - `localhost:5001/cortex/api:warm-X` (dev registry)
///   - `us-east1-docker.pkg.dev/proj/repo/img:v1` (GAR)
///
/// Engram doesn't maintain a curated catalog at the `(repo, tag)`
/// granularity — operators explicitly enable URIs via
/// `enabled_images`, sessions reference them by the full URI, and
/// the host-agent pulls them via the auth resolver.
pub type ImageRef = String;

/// Split a `host[:port]/repo[/path]:tag` reference into
/// `(host_and_path, tag)`. Used by `SecretContext` namespacing and
/// `SnapshotRecord` labelling — places that want either the path
/// portion (without the tag) or just the tag.
///
/// Returns `(uri, "")` if there's no `:tag` suffix (a digest-only
/// reference uses `@sha256:...` syntax which we don't decompose
/// here).
pub fn split_image_ref(uri: &str) -> (&str, &str) {
    // Find the LAST ':' AFTER the last '/' — protects against
    // splitting on the registry's port (e.g. `localhost:5001/...`).
    if let Some(slash) = uri.rfind('/') {
        if let Some(colon_in_tail) = uri[slash..].rfind(':') {
            let cut = slash + colon_in_tail;
            return (&uri[..cut], &uri[cut + 1..]);
        }
    } else if let Some(colon) = uri.rfind(':') {
        return (&uri[..colon], &uri[colon + 1..]);
    }
    (uri, "")
}

/// What long-running agent process (if any) attaches to the session.
/// `None` is the "VM with a shell" mode — engram-bootstrap runs but
/// never receives a `BootstrapLaunch` frame.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HarnessSpec {
    #[default]
    None,
    /// One of the harnesses declared in the image manifest's
    /// `harnesses` array. The coordinator validates `name` against
    /// the manifest at create time and returns 400 on mismatch.
    Builtin { name: String },
}

impl HarnessSpec {
    pub fn is_none(&self) -> bool {
        matches!(self, Self::None)
    }
}

/// User-facing request to create a session. Two axes:
/// image (immutable rootfs that supplies the workspace) +
/// harness (what agent attaches). ADR 0005 retired the
/// `WorkspaceSpec` axis — the bake image's `/workspace` is the
/// workspace; agents that need git push do it themselves inside the
/// sandbox using credentials mounted via `[secrets.X]`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionSpec {
    pub image: ImageRef,
    #[serde(default)]
    pub harness: HarnessSpec,
    pub user_id: Option<String>,
}

/// A persisted session row.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Session {
    pub id: SessionId,
    pub user_id: Option<String>,
    pub status: SessionStatus,
    pub host_id: Option<HostId>,
    /// In-memory `SandboxId` of the live sandbox serving this
    /// session. `None` in `Pending` (sandbox not created yet) /
    /// `Idle` (sandbox evicted) / `Dead` (host died) /
    /// `Completed` / `Failed`.
    #[serde(default)]
    pub sandbox_id: Option<SandboxId>,
    pub image: ImageRef,
    #[serde(default)]
    pub harness: HarnessSpec,
    pub created_at: DateTime<Utc>,
    pub last_active_at: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_status_serializes_lowercase() {
        let payload = serde_json::to_value(SessionStatus::Active).unwrap();
        assert_eq!(payload, serde_json::json!("active"));
        let parsed: SessionStatus = serde_json::from_str(r#""idle""#).unwrap();
        assert_eq!(parsed, SessionStatus::Idle);
    }

    #[test]
    fn session_status_unknown_string_rejected() {
        let res: Result<SessionStatus, _> = serde_json::from_str(r#""running""#);
        assert!(res.is_err(), "unknown variants must fail to deserialize");
    }

    #[test]
    fn session_status_as_str_matches_serde_form() {
        for s in [
            SessionStatus::Pending,
            SessionStatus::Active,
            SessionStatus::Idle,
            SessionStatus::ColdEvicted,
            SessionStatus::Completed,
            SessionStatus::Failed,
            SessionStatus::Dead,
        ] {
            let via_serde = serde_json::to_string(&s).unwrap();
            let trimmed = via_serde.trim_matches('"');
            assert_eq!(s.as_str(), trimmed, "as_str must match wire format");
        }
    }

    #[test]
    fn cold_evicted_serializes_as_snake_case() {
        let payload = serde_json::to_value(SessionStatus::ColdEvicted).unwrap();
        assert_eq!(payload, serde_json::json!("cold_evicted"));
        let parsed: SessionStatus = serde_json::from_str(r#""cold_evicted""#).unwrap();
        assert_eq!(parsed, SessionStatus::ColdEvicted);
    }

    #[test]
    fn split_image_ref_handles_typical_shapes() {
        // GHCR-style: host + multi-segment repo + tag.
        let (rest, tag) = split_image_ref("ghcr.io/cortex/api:warm-2026");
        assert_eq!(rest, "ghcr.io/cortex/api");
        assert_eq!(tag, "warm-2026");

        // Local registry with port — port colon must NOT be treated
        // as the tag separator.
        let (rest, tag) = split_image_ref("localhost:5001/cortex/api:warm-X");
        assert_eq!(rest, "localhost:5001/cortex/api");
        assert_eq!(tag, "warm-X");

        // No tag → returns whole input as `rest`, empty `tag`.
        let (rest, tag) = split_image_ref("ghcr.io/cortex/api");
        assert_eq!(rest, "ghcr.io/cortex/api");
        assert_eq!(tag, "");
    }

    #[test]
    fn harness_spec_default_is_none() {
        assert!(HarnessSpec::default().is_none());
        assert!(!HarnessSpec::Builtin {
            name: "claude".into()
        }
        .is_none());
    }

    #[test]
    fn harness_spec_round_trips_through_json() {
        let cases = vec![
            HarnessSpec::None,
            HarnessSpec::Builtin {
                name: "claude".into(),
            },
        ];
        for h in cases {
            let blob = serde_json::to_string(&h).unwrap();
            let back: HarnessSpec = serde_json::from_str(&blob).unwrap();
            assert_eq!(back, h);
        }
    }

    #[test]
    fn image_ref_round_trips_through_json() {
        // ImageRef is now a flat String URI; serialization is just
        // the string. Pin the wire shape so changes that move it
        // back to a tagged enum fail loudly.
        let img: ImageRef = "ghcr.io/cortex/api:warm-2026".to_string();
        let blob = serde_json::to_string(&img).unwrap();
        assert_eq!(blob, "\"ghcr.io/cortex/api:warm-2026\"");
        let back: ImageRef = serde_json::from_str(&blob).unwrap();
        assert_eq!(back, img);
    }

    #[test]
    fn session_round_trips_through_json() {
        let original = Session {
            id: SessionId::new(),
            user_id: Some("u1".into()),
            status: SessionStatus::Active,
            host_id: Some(HostId::new()),
            sandbox_id: Some(SandboxId::new()),
            image: "ghcr.io/cortex/api:warm-20260101T000000Z".into(),
            harness: HarnessSpec::Builtin {
                name: "claude".into(),
            },
            created_at: Utc::now(),
            last_active_at: Utc::now(),
        };
        let blob = serde_json::to_string(&original).unwrap();
        let back: Session = serde_json::from_str(&blob).unwrap();
        assert_eq!(back.id, original.id);
        assert_eq!(back.image, original.image);
        assert_eq!(back.harness, original.harness);
    }
}
