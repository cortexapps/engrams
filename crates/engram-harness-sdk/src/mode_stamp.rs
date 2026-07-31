//! The session-mode stamp (ADR 0107).
//!
//! A `HarnessCommand::Prompt.mode` directive is latched to a file on the
//! workspace disk — the thing snapshots preserve — because process env is
//! immutable and reverts to create-time values after an idle-evict → resume.
//! The stamp, not the wire field, is the durable source of truth: every
//! adapter reads it at the turn boundary (claude: to choose
//! `--permission-mode` argv on respawn; codex: to choose per-turn params) and
//! flips it when a plan decision changes the mode.
//!
//! Sits next to the other `/workspace/.engrams/` stamps (`claude-session-id`,
//! `codex-thread-id`, `codex-parked-calls.json`).

use std::path::Path;

/// Default stamp path inside the guest.
pub const MODE_STAMP_FILE: &str = "/workspace/.engrams/mode";

/// The mode every session starts in when no directive ever arrived.
pub const DEFAULT_MODE: &str = "default";

/// Read the latched mode. A missing, unreadable, or empty stamp means
/// [`DEFAULT_MODE`] — tolerant by design (the stamp is a latch, not a
/// ledger; ADR 0099 H5 crash-state rules apply: garbage never panics).
pub fn read_mode_stamp(path: &Path) -> String {
    match std::fs::read_to_string(path) {
        Ok(s) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                DEFAULT_MODE.to_string()
            } else {
                trimmed.to_string()
            }
        }
        Err(_) => DEFAULT_MODE.to_string(),
    }
}

/// Latch a mode. Write-to-temp + rename so a crash mid-write leaves either
/// the old stamp or the new one, never a torn file. Best-effort creation of
/// the parent directory (first write in a fresh workspace).
pub fn write_mode_stamp(path: &Path, mode: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, mode)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_empty_and_garbage_stamps_read_as_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mode");
        assert_eq!(read_mode_stamp(&path), DEFAULT_MODE);

        std::fs::write(&path, "").unwrap();
        assert_eq!(read_mode_stamp(&path), DEFAULT_MODE);

        std::fs::write(&path, "  \n").unwrap();
        assert_eq!(read_mode_stamp(&path), DEFAULT_MODE);
    }

    #[test]
    fn write_then_read_round_trips_and_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        // Parent dirs are created on first write (fresh workspace).
        let path = dir.path().join("nested/.engrams/mode");
        write_mode_stamp(&path, "plan").unwrap();
        assert_eq!(read_mode_stamp(&path), "plan");

        write_mode_stamp(&path, DEFAULT_MODE).unwrap();
        assert_eq!(read_mode_stamp(&path), DEFAULT_MODE);
    }

    #[test]
    fn whitespace_is_trimmed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mode");
        std::fs::write(&path, "plan\n").unwrap();
        assert_eq!(read_mode_stamp(&path), "plan");
    }
}
