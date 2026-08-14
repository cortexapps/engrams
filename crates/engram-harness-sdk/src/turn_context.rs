//! Turn context: per-turn workspace context every harness injects the same way.
//!
//! ADR 0114 D9 (amended): the orchestrator keeps a context file fresh in the
//! workspace — today the spec digest, which names the sections humans changed
//! since the agent's last turn. Each adapter calls [`with_turn_context`] on
//! the prompt text at the CONSUMPTION boundary (the moment it writes the
//! prompt to its vendor agent), which is what makes the mechanism
//! harness-agnostic and delivery-fresh:
//!
//! - Every adapter owns its prompt queue and writes to the vendor only when a
//!   turn starts, so the file is read after any queue wait — a queued prompt
//!   never carries a stale digest (N10).
//! - The wrapped text goes only to the vendor process. The coordinator's
//!   `role:user` message is the single source of the person's bubble, so the
//!   injected block never renders as the person's words.
//! - No vendor hook system is involved, so a new adapter inherits the
//!   behavior by calling one function instead of porting a Claude-only hook.

use std::path::Path;

/// Where the orchestrator's projection pipeline publishes the spec digest
/// (`SPEC_DIGEST_PATH` in `orchestrator/src/specs/projection.ts`). Absent in
/// every non-spec session.
pub const SPEC_DIGEST_PATH: &str = "/workspace/.engrams/spec/digest.md";

/// The injected block is bounded so a runaway digest cannot crowd out the
/// prompt; the digest service writes far less than this.
const MAX_CONTEXT_BYTES: usize = 8192;

/// Prepend the workspace turn context to a vendor-facing prompt.
///
/// Returns the text unchanged when the context file is absent or empty — the
/// non-spec-session case, and the failure mode for any read error (context is
/// a quality signal; the write fence is the correctness mechanism).
pub fn with_turn_context(text: &str) -> String {
    with_turn_context_from(Path::new(SPEC_DIGEST_PATH), text)
}

/// [`with_turn_context`] with an explicit file, for adapters and tests.
pub fn with_turn_context_from(context_file: &Path, text: &str) -> String {
    let context = match std::fs::read_to_string(context_file) {
        Ok(raw) => crate::truncate_utf8(raw.trim(), MAX_CONTEXT_BYTES),
        Err(_) => return text.to_owned(),
    };
    if context.is_empty() {
        return text.to_owned();
    }
    format!("<engrams-turn-context>\n{context}\n</engrams-turn-context>\n\n{text}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_passes_text_through() {
        let dir = std::env::temp_dir().join(format!("turn-ctx-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            with_turn_context_from(&dir.join("digest.md"), "prompt"),
            "prompt"
        );
    }

    #[test]
    fn empty_and_whitespace_files_pass_text_through() {
        let dir = std::env::temp_dir().join(format!("turn-ctx-empty-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("digest.md");
        std::fs::write(&file, "  \n\n").unwrap();
        assert_eq!(with_turn_context_from(&file, "prompt"), "prompt");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn context_wraps_ahead_of_the_prompt_and_is_bounded() {
        let dir = std::env::temp_dir().join(format!("turn-ctx-wrap-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("digest.md");
        std::fs::write(
            &file,
            "# Spec changes since your last turn\n\n## Design\n- Ada updated this section.\n",
        )
        .unwrap();
        let wrapped = with_turn_context_from(&file, "tighten §Design");
        assert!(wrapped.starts_with("<engrams-turn-context>\n# Spec changes"));
        assert!(wrapped.ends_with("</engrams-turn-context>\n\ntighten §Design"));

        std::fs::write(&file, "x".repeat(MAX_CONTEXT_BYTES * 2)).unwrap();
        let bounded = with_turn_context_from(&file, "prompt");
        assert!(bounded.contains("…[truncated]"));
        assert!(bounded.len() < MAX_CONTEXT_BYTES + 200);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
