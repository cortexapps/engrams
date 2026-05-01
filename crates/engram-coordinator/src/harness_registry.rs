//! Host-side harness registry.
//!
//! Harness binaries live OUTSIDE images now. The host owns a directory
//! (configured via `cfg.harnesses_dir`, default `<local_path>/harnesses`)
//! whose entries are the closed set of `HarnessSpec::Builtin{name}`
//! values a session can reference. The directory is mounted read-only
//! into every sandbox via virtio-fs (VZ today, FC after virtiofsd
//! parity), so the in-VM bootstrap exec's the binary off the shared
//! mount — no per-image bake step.
//!
//! Why this lives at the coord and not on a per-image manifest:
//! harness selection is a deployment-wide concern (one operator
//! decides which agents are available across all images / sessions),
//! whereas image identity is workspace-runtime concern (Java vs Node
//! vs whatever). Conflating them in `engram.toml` was the original
//! sin we're undoing here.
//!
//! The registry is built once at startup by scanning the directory
//! for executable files. Re-scanning on demand isn't supported in v1
//! — operators add a binary, restart the coord. Live reload can land
//! later if it bites.

use std::path::{Path, PathBuf};

/// One harness available on this host. The name is the file stem of
/// the binary in `harnesses_dir`. Description is currently empty —
/// future work may parse a sidecar `<name>.toml` if richer metadata
/// becomes useful.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HarnessEntry {
    pub name: String,
    pub host_path: PathBuf,
    pub description: Option<String>,
}

#[derive(Clone, Debug)]
pub struct HarnessRegistry {
    /// Where the binaries live on the host. Mounted into sandboxes
    /// at [`HarnessRegistry::guest_mount_path`] read-only.
    host_dir: PathBuf,
    entries: Vec<HarnessEntry>,
}

impl HarnessRegistry {
    /// Build a registry by scanning `host_dir` for executable files.
    /// Missing dir is not an error — the registry is just empty
    /// (sessions with `harness=none` still work). Names that
    /// collide with reserved characters (`/`, leading dot) are
    /// skipped silently — the operator has filename hygiene to keep
    /// the namespace clean.
    pub fn from_dir(host_dir: PathBuf) -> std::io::Result<Self> {
        let mut entries = Vec::new();
        if !host_dir.exists() {
            tracing::info!(
                dir = %host_dir.display(),
                "harness directory does not exist; sessions with `harness != none` will be rejected",
            );
            return Ok(Self { host_dir, entries });
        }
        let read_dir = std::fs::read_dir(&host_dir)?;
        for ent in read_dir {
            let ent = ent?;
            let path = ent.path();
            if !path.is_file() {
                continue;
            }
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            // Filenames starting with `.` are convention-hidden;
            // skip so editor swap files / DS_Store don't pollute.
            if name.starts_with('.') {
                continue;
            }
            entries.push(HarnessEntry {
                name: name.to_string(),
                host_path: path,
                description: None,
            });
        }
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        tracing::info!(
            dir = %host_dir.display(),
            count = entries.len(),
            names = ?entries.iter().map(|e| &e.name).collect::<Vec<_>>(),
            "harness registry initialised",
        );
        Ok(Self { host_dir, entries })
    }

    /// Convenience constructor for tests / fixtures: empty registry.
    pub fn empty() -> Self {
        Self {
            host_dir: PathBuf::from("/dev/null"),
            entries: Vec::new(),
        }
    }

    pub fn entries(&self) -> &[HarnessEntry] {
        &self.entries
    }

    /// Resolve a harness name. Returns the registry entry; the caller
    /// uses [`HarnessRegistry::guest_mount_path`] to construct the
    /// in-VM argv since the host path doesn't apply on FC/VZ.
    pub fn lookup(&self, name: &str) -> Option<&HarnessEntry> {
        self.entries.iter().find(|e| e.name == name)
    }

    /// Path on the host the registry is rooted at. Sandboxes mount
    /// this directory read-only at [`HarnessRegistry::guest_mount_path`].
    pub fn host_dir(&self) -> &Path {
        &self.host_dir
    }

    /// In-VM mount point. The bootstrap supervisor exec's
    /// `<guest_mount_path>/<name>` for `HarnessSpec::Builtin{name}`.
    pub fn guest_mount_path() -> &'static Path {
        Path::new("/run/engram/harnesses")
    }

    /// Argv[0] for a session asking for `name`. Used by
    /// `resolve_harness` to build the `BootstrapLaunch` payload.
    pub fn guest_argv0(name: &str) -> String {
        format!("{}/{}", Self::guest_mount_path().display(), name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    fn write_executable(dir: &Path, name: &str) {
        let p = dir.join(name);
        std::fs::write(&p, b"#!/bin/sh\necho dummy\n").unwrap();
        let mut perms = std::fs::metadata(&p).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&p, perms).unwrap();
    }

    #[test]
    fn missing_dir_yields_empty_registry() {
        let reg = HarnessRegistry::from_dir("/nonexistent/path".into()).unwrap();
        assert!(reg.entries().is_empty());
        assert!(reg.lookup("anything").is_none());
    }

    #[test]
    fn scans_directory_and_lists_executable_names() {
        let dir = TempDir::new().unwrap();
        write_executable(dir.path(), "claude");
        write_executable(dir.path(), "noop");
        write_executable(dir.path(), ".hidden");

        let reg = HarnessRegistry::from_dir(dir.path().to_path_buf()).unwrap();
        let names: Vec<&str> = reg.entries().iter().map(|e| e.name.as_str()).collect();
        // sorted, hidden file excluded.
        assert_eq!(names, vec!["claude", "noop"]);
        assert!(reg.lookup("claude").is_some());
        assert!(reg.lookup("noop").is_some());
        assert!(reg.lookup(".hidden").is_none());
        assert!(reg.lookup("missing").is_none());
    }

    #[test]
    fn guest_argv0_is_under_guest_mount_path() {
        assert_eq!(
            HarnessRegistry::guest_argv0("claude"),
            "/run/engram/harnesses/claude"
        );
    }

    #[test]
    fn empty_constructor_is_truly_empty() {
        let reg = HarnessRegistry::empty();
        assert!(reg.entries().is_empty());
    }
}
