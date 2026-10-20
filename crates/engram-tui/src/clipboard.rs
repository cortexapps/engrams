//! Copy text to the user's clipboard, without a clipboard crate: the
//! platform's own command where the TUI runs on the user's machine
//! (`pbcopy`, `wl-copy`, `xclip`, `xsel`, `clip`), and OSC 52, which asks
//! the terminal to set its clipboard, over SSH or when no command works.
//!
//! A command blocks while it runs, so the runtime calls [`copy_native`]
//! off the event loop. OSC 52 is written by the event loop itself, between
//! frames, so it never lands inside one.

use std::io::Write;
use std::process::{Command, Stdio};

/// OSC 52 payloads past this many bytes are refused: some terminals drop
/// or truncate large ones (Codex uses the same cap).
pub const OSC52_MAX: usize = 100_000;

/// The session runs on another machine than the user's screen: the local
/// clipboard is not theirs.
pub fn remote() -> bool {
    ["SSH_CONNECTION", "SSH_TTY"]
        .iter()
        .any(|k| std::env::var_os(k).is_some_and(|v| !v.is_empty()))
}

/// Copy with the platform's clipboard command. Blocking.
pub fn copy_native(text: &str) -> Result<(), String> {
    let candidates: &[(&str, &[&str])] = if cfg!(target_os = "macos") {
        &[("pbcopy", &[])]
    } else if cfg!(windows) {
        &[("clip", &[])]
    } else if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        &[("wl-copy", &[]), ("xclip", &["-selection", "clipboard"])]
    } else if std::env::var_os("DISPLAY").is_some() {
        &[
            ("xclip", &["-selection", "clipboard"]),
            ("xsel", &["--clipboard", "--input"]),
        ]
    } else {
        &[]
    };
    let mut last = "no clipboard command for this system".to_string();
    for (program, args) in candidates {
        match run(program, args, text, DEADLINE) {
            Ok(()) => return Ok(()),
            Err(e) => last = format!("{program}: {e}"),
        }
    }
    Err(last)
}

/// How long a clipboard command may take before it is killed.
const DEADLINE: std::time::Duration = std::time::Duration::from_secs(2);

fn run(program: &str, args: &[&str], text: &str, limit: std::time::Duration) -> Result<(), String> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| e.to_string())?;
    // Write on a thread: a command that stops reading must not hold this
    // one past the deadline. Closing stdin (the thread's end) is EOF.
    let writer = child.stdin.take().map(|mut stdin| {
        let text = text.to_string();
        std::thread::spawn(move || stdin.write_all(text.as_bytes()))
    });
    let deadline = std::time::Instant::now() + limit;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            Ok(None) => {
                // Stuck: kill and reap it, never leave it behind.
                let _ = child.kill();
                let _ = child.wait();
                return Err("timed out".into());
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(e.to_string());
            }
        }
    };
    if let Some(Ok(Err(e))) = writer.map(std::thread::JoinHandle::join) {
        return Err(e.to_string());
    }
    if status.success() {
        Ok(())
    } else {
        Err(format!("exited with {status}"))
    }
}

/// The OSC 52 sequence that asks the terminal to set its clipboard to
/// `text`, wrapped for tmux to pass through. `None` past [`OSC52_MAX`].
pub fn osc52(text: &str) -> Option<Vec<u8>> {
    if text.len() > OSC52_MAX {
        return None;
    }
    let tmux = std::env::var_os("TMUX").is_some_and(|v| !v.is_empty());
    let mut out = Vec::with_capacity(text.len() * 4 / 3 + 32);
    if tmux {
        out.extend_from_slice(b"\x1bPtmux;\x1b");
    }
    out.extend_from_slice(b"\x1b]52;c;");
    crate::status::base64_into(&mut out, text.as_bytes());
    out.push(0x07);
    if tmux {
        out.extend_from_slice(b"\x1b\\");
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A command that never finishes is killed at the deadline, not
    /// waited on (exit would hang behind it).
    #[cfg(unix)]
    #[test]
    fn a_stuck_command_is_killed_at_the_deadline() {
        let at = std::time::Instant::now();
        let res = run("sleep", &["30"], "x", std::time::Duration::from_millis(100));
        assert_eq!(res, Err("timed out".to_string()));
        assert!(at.elapsed() < std::time::Duration::from_secs(5));
    }

    #[test]
    fn osc52_carries_the_text_as_base64() {
        // Outside tmux in the test environment.
        if std::env::var_os("TMUX").is_some() {
            return;
        }
        assert_eq!(osc52("hi").unwrap(), b"\x1b]52;c;aGk=\x07");
        assert!(osc52(&"x".repeat(OSC52_MAX + 1)).is_none());
    }
}
