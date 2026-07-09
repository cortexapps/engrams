//! Newtype wrappers around UUIDs for type-safe identifiers.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

macro_rules! id_newtype {
    ($name:ident) => {
        #[derive(
            Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(pub Uuid);

        impl $name {
            pub fn new() -> Self {
                Self(Uuid::new_v4())
            }

            pub fn as_uuid(&self) -> Uuid {
                self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl From<Uuid> for $name {
            fn from(u: Uuid) -> Self {
                Self(u)
            }
        }

        impl From<$name> for Uuid {
            fn from(id: $name) -> Self {
                id.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }

        impl FromStr for $name {
            type Err = uuid::Error;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Uuid::from_str(s).map($name)
            }
        }
    };
}

id_newtype!(SessionId);
id_newtype!(UserId);
id_newtype!(HostId);
id_newtype!(SandboxId);
id_newtype!(SnapshotId);
id_newtype!(MessageId);
id_newtype!(ToolCallId);
id_newtype!(AgentCommitId);
// ADR 0084: identifies one durable, host-executed capture job row
// (`capture_jobs.id`).
id_newtype!(CaptureJobId);

impl HostId {
    /// Deterministic `HostId` derived from a Kubernetes node name (ADR 0044
    /// K2 / GAP 1). Not an RFC-4122 versioned UUID — `HostId` is opaque; we
    /// only need it stable + collision-resistant across node names so that a
    /// same-node host-agent restart (even with a wiped work_dir) recovers the
    /// same id, and the K3 rollout operator can address a node's host without
    /// a registry lookup. Keep the seed string in lockstep with any consumer
    /// that recomputes it.
    pub fn from_node_name(node_name: &str) -> Self {
        use sha2::{Digest, Sha256};
        let digest = Sha256::digest(format!("engram-host:{node_name}").as_bytes());
        let mut bytes = [0u8; 16];
        bytes.copy_from_slice(&digest[..16]);
        Self(Uuid::from_bytes(bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_roundtrip_via_string() {
        let id = SessionId::new();
        let s = id.to_string();
        let parsed: SessionId = s.parse().expect("parse");
        assert_eq!(id, parsed);
    }

    #[test]
    fn id_roundtrip_via_json() {
        let id = HostId::new();
        let s = serde_json::to_string(&id).unwrap();
        let parsed: HostId = serde_json::from_str(&s).unwrap();
        assert_eq!(id, parsed);
    }
}
