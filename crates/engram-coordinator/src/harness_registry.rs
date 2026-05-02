//! Host-side harness registry.
//!
//! Each harness is a *directory* under `cfg.harnesses_dir` containing
//! at least one file named `harness` (the entry point bootstrap
//! exec's). Sidecar binaries — Bun-bundled `claude`, future runtime
//! deps — live alongside in the same directory and are resolved
//! relative to `argv[0]` by the harness wrapper itself.
//!
//! Why directories instead of single binaries: it keeps each harness
//! self-contained. The `claude` harness ships its bundled CLI next to
//! the wrapper; a future Python-based harness could ship its
//! interpreter the same way. The image stops needing node / python /
//! whatever — it only carries what the *workspace* runtime needs.
//!
//! Layout on the host:
//!
//! ```text
//! <cfg.harnesses_dir>/
//!   claude/
//!     harness         (engram-harness-claude wrapper)
//!     claude          (bundled Bun-CLI binary)
//!   noop/
//!     harness         (engram-harness-noop wrapper, no sidecar)
//! ```
//!
//! Mounted into every sandbox at `/run/engram/harnesses` read-only.
//! Bootstrap exec's `/run/engram/harnesses/<name>/harness`.
//!
//! The registry scans on startup and re-scanning is not supported
//! — operators add a pack, restart the coord. Live reload can land
//! later if it bites.

use std::path::{Path, PathBuf};

/// One harness pack available on this host.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HarnessEntry {
    pub name: String,
    /// Host path to the entry-point binary (`<dir>/harness`). The
    /// wrapper finds its sidecars relative to `argv[0]`, so we don't
    /// surface those here.
    pub host_path: PathBuf,
    /// Free-form one-liner for the dashboard's dropdown label.
    pub description: Option<String>,
}

#[derive(Clone, Debug)]
pub struct HarnessRegistry {
    host_dir: PathBuf,
    entries: Vec<HarnessEntry>,
}

const ENTRY_POINT_NAME: &str = "harness";

impl HarnessRegistry {
    /// Build a registry by scanning `host_dir` for harness packs.
    /// Each subdirectory becomes a harness named after the directory,
    /// provided it contains an executable file named `harness`. Other
    /// shapes are skipped silently — operators have filename hygiene
    /// to keep the namespace clean. A missing `host_dir` is not an
    /// error: the registry is just empty.
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
            let pack_dir = ent.path();
            if !pack_dir.is_dir() {
                continue;
            }
            let Some(name) = pack_dir.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if name.starts_with('.') {
                continue;
            }
            let entry_point = pack_dir.join(ENTRY_POINT_NAME);
            if !entry_point.is_file() {
                tracing::warn!(
                    name = %name,
                    expected = %entry_point.display(),
                    "skipping harness pack: missing `harness` entry-point file",
                );
                continue;
            }
            entries.push(HarnessEntry {
                name: name.to_string(),
                host_path: entry_point,
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

    pub fn lookup(&self, name: &str) -> Option<&HarnessEntry> {
        self.entries.iter().find(|e| e.name == name)
    }

    pub fn host_dir(&self) -> &Path {
        &self.host_dir
    }

    /// In-VM mount point. The pack tree is mounted here read-only;
    /// `<mount>/<name>/harness` is what bootstrap exec's.
    pub fn guest_mount_path() -> &'static Path {
        Path::new("/run/engram/harnesses")
    }

    /// Argv[0] for a session asking for `name`. Used by
    /// `resolve_harness` to build the `BootstrapLaunch` payload.
    pub fn guest_argv0(name: &str) -> String {
        format!(
            "{}/{}/{}",
            Self::guest_mount_path().display(),
            name,
            ENTRY_POINT_NAME,
        )
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

    /// Build a harness pack: `<dir>/<name>/harness`, plus optional sidecars.
    fn write_pack(dir: &Path, name: &str, sidecars: &[&str]) {
        let pack = dir.join(name);
        std::fs::create_dir_all(&pack).unwrap();
        write_executable(&pack, ENTRY_POINT_NAME);
        for s in sidecars {
            write_executable(&pack, s);
        }
    }

    #[test]
    fn missing_dir_yields_empty_registry() {
        let reg = HarnessRegistry::from_dir("/nonexistent/path".into()).unwrap();
        assert!(reg.entries().is_empty());
        assert!(reg.lookup("anything").is_none());
    }

    #[test]
    fn scans_packs_and_lists_them_alphabetically() {
        let dir = TempDir::new().unwrap();
        write_pack(dir.path(), "claude", &["claude"]); // wrapper + sidecar
        write_pack(dir.path(), "noop", &[]); // wrapper only
                                             // Hidden dir: ignored.
        std::fs::create_dir(dir.path().join(".hidden")).unwrap();
        // Loose file: ignored (registry only takes dirs).
        std::fs::write(dir.path().join("loose"), b"").unwrap();
        // Directory missing the entry point: skipped with a warning.
        std::fs::create_dir(dir.path().join("malformed")).unwrap();

        let reg = HarnessRegistry::from_dir(dir.path().to_path_buf()).unwrap();
        let names: Vec<&str> = reg.entries().iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["claude", "noop"]);
        assert_eq!(
            reg.lookup("claude").unwrap().host_path,
            dir.path().join("claude").join(ENTRY_POINT_NAME),
        );
        assert!(reg.lookup("malformed").is_none());
    }

    #[test]
    fn guest_argv0_points_inside_pack_directory() {
        assert_eq!(
            HarnessRegistry::guest_argv0("claude"),
            "/run/engram/harnesses/claude/harness"
        );
    }

    #[test]
    fn empty_constructor_is_truly_empty() {
        let reg = HarnessRegistry::empty();
        assert!(reg.entries().is_empty());
    }
}
