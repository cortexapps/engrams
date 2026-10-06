//! The current binding identity, replaced by agentd before spawn or reattach.
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use engram_ids::SandboxId;
use serde::{Deserialize, Serialize};

pub const SANDBOX_ID_ENV: &str = "ENGRAM_SANDBOX_ID";
pub const BINDING_EPOCH_ENV: &str = "ENGRAM_BINDING_EPOCH";
pub const ATTACH_TOKEN_FILE: &str = "/run/engram/attach-token";
pub const ATTACH_TOKEN_FILE_ENV: &str = "ENGRAM_ATTACH_TOKEN_FILE";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachToken {
    pub sandbox_id: SandboxId,
    pub binding_epoch: u64,
}

impl AttachToken {
    pub fn from_env_map(env: &HashMap<String, String>) -> Option<Self> {
        Some(Self {
            sandbox_id: env.get(SANDBOX_ID_ENV)?.parse().ok()?,
            binding_epoch: env.get(BINDING_EPOCH_ENV)?.parse().ok()?,
        })
    }

    pub fn env(&self) -> [(String, String); 2] {
        [
            (SANDBOX_ID_ENV.into(), self.sandbox_id.to_string()),
            (BINDING_EPOCH_ENV.into(), self.binding_epoch.to_string()),
        ]
    }

    pub fn load() -> io::Result<Self> {
        let path =
            std::env::var_os(ATTACH_TOKEN_FILE_ENV).unwrap_or_else(|| ATTACH_TOKEN_FILE.into());
        match fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(io::Error::other),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                Self::from_env_map(&std::env::vars().collect()).ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NotFound,
                        "attach token file and environment missing or invalid",
                    )
                })
            }
            Err(e) => Err(e),
        }
    }

    pub fn write_atomic(&self, path: &Path) -> io::Result<()> {
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        fs::create_dir_all(parent)?;
        let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
        tmp.as_file()
            .set_permissions(fs::Permissions::from_mode(0o644))?;
        tmp.write_all(&serde_json::to_vec(self).map_err(io::Error::other)?)?;
        tmp.as_file().sync_all()?;
        tmp.persist(path).map_err(|e| e.error)?;
        File::open(parent)?.sync_all()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_token_round_trip_and_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/token");
        let mut token = AttachToken {
            sandbox_id: SandboxId::new(),
            binding_epoch: 1,
        };
        assert_eq!(AttachToken::from_env_map(&token.env().into()), Some(token));
        token.write_atomic(&path).unwrap();
        token.binding_epoch = 2;
        token.write_atomic(&path).unwrap();
        assert_eq!(
            serde_json::from_slice::<AttachToken>(&fs::read(&path).unwrap()).unwrap(),
            token
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o644
        );
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
    }

    #[test]
    fn invalid_env_is_not_a_token() {
        assert_eq!(AttachToken::from_env_map(&HashMap::new()), None);
        let token = AttachToken {
            sandbox_id: SandboxId::new(),
            binding_epoch: 1,
        };
        let mut env: HashMap<_, _> = token.env().into();
        env.insert(BINDING_EPOCH_ENV.into(), "invalid".into());
        assert_eq!(AttachToken::from_env_map(&env), None);
    }
}
