//! The per-session harness state directory.
//!
//! Everything a harness must keep across an agent respawn or an idle-evict →
//! resume lives in ONE directory on the workspace disk: the hook and MCP
//! sockets, the generated agent config, the resumable-session stash, the Bash
//! cwd tracker, and the ADR 0107 mode stamp. Production is the fixed in-VM
//! [`DEFAULT_STATE_DIR`]; the dev Process backend (which runs the harness on a
//! shared host, not in a guest) points [`STATE_DIR_ENV`] at its own sandbox
//! directory, and every harness test injects a temp directory.
//!
//! **One root, one seam.** Each path used to carry its own optional override,
//! so a test could redirect SOME of them and still write the rest into the live
//! directory. A `cargo nextest` run inside a dogfooding session therefore
//! unlinked that session's own hook and MCP sockets and rewrote its generated
//! settings: every deferred tool then failed with "engrams hook bridge
//! unavailable" for the rest of the session. A single root makes that
//! unreachable — nothing derives a path any other way, and the field has no
//! default, so a new test cannot forget it.

use std::path::{Path, PathBuf};

/// The in-VM state directory. Per-session already (the workspace disk is), so
/// the names inside it are fixed and still collision-free across sessions.
pub const DEFAULT_STATE_DIR: &str = "/workspace/.engrams";

/// Carries the resolved root to a re-invoked harness child (the claude hook and
/// MCP bridges), which has no shared address space with the main harness.
pub const STATE_DIR_ENV: &str = "ENGRAM_STATE_DIR";

/// The harness state directory, and the name of every file in it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StateDir(PathBuf);

impl StateDir {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self(root.into())
    }

    /// Resolve the root a harness child must share with its parent.
    pub fn from_env() -> Self {
        Self::new(
            std::env::var_os(STATE_DIR_ENV)
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(DEFAULT_STATE_DIR)),
        )
    }

    pub fn root(&self) -> &Path {
        &self.0
    }

    /// ADR 0107: the latched session mode.
    pub fn mode_stamp(&self) -> PathBuf {
        self.0.join("mode")
    }

    /// ADR 0054: the claude hook ↔ harness socket.
    pub fn hook_sock(&self) -> PathBuf {
        self.0.join("hook.sock")
    }

    /// ADR 0089: the claude MCP bridge ↔ harness socket.
    pub fn mcp_sock(&self) -> PathBuf {
        self.0.join("mcp.sock")
    }

    /// The generated `claude --settings` file (the PreToolUse hook).
    pub fn claude_settings(&self) -> PathBuf {
        self.0.join("claude-settings.json")
    }

    /// The generated `claude --mcp-config` file (the injected tool server).
    pub fn claude_mcp_config(&self) -> PathBuf {
        self.0.join("mcp-config.json")
    }

    /// The harness-owned logical cwd for claude's Bash tool. Unlike the CLI's
    /// private `/tmp/claude-*-cwd` tracker, this survives a respawn.
    pub fn claude_bash_cwd(&self) -> PathBuf {
        self.0.join("bash-cwd")
    }

    /// The `claude --resume` session id.
    pub fn claude_session_id(&self) -> PathBuf {
        self.0.join("claude-session-id")
    }

    /// `CODEX_HOME` for the codex CLI.
    pub fn codex_home(&self) -> PathBuf {
        self.0.join("codex")
    }

    /// The codex thread to resume.
    pub fn codex_thread_id(&self) -> PathBuf {
        self.0.join("codex-thread-id")
    }

    /// The codex parked-call table.
    pub fn codex_parked_calls(&self) -> PathBuf {
        self.0.join("codex-parked-calls.json")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_path_stays_inside_the_root() {
        let state = StateDir::new("/tmp/engram-state-test");
        let paths = [
            state.mode_stamp(),
            state.hook_sock(),
            state.mcp_sock(),
            state.claude_settings(),
            state.claude_mcp_config(),
            state.claude_bash_cwd(),
            state.claude_session_id(),
            state.codex_home(),
            state.codex_thread_id(),
            state.codex_parked_calls(),
        ];
        for path in &paths {
            assert!(
                path.starts_with(state.root()),
                "{} escaped the state dir",
                path.display()
            );
        }
        // Distinct names: one root only works if nothing shadows a sibling.
        let mut names: Vec<_> = paths.iter().collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), paths.len(), "two state files share a name");
    }

    #[test]
    fn env_absent_or_empty_resolves_the_production_root() {
        // The env is process-global; this asserts the fallback only, which is
        // what an absent (production) and an empty (cleared) value both hit.
        let cleared = StateDir::new(
            std::env::var_os(STATE_DIR_ENV)
                .filter(|value| !value.is_empty())
                .map_or_else(|| PathBuf::from(DEFAULT_STATE_DIR), PathBuf::from),
        );
        assert_eq!(StateDir::from_env(), cleared);
    }
}
