//! Slash commands (ADR 0124 PR8): a draft that starts with `/` names a
//! command instead of a prompt. One static table drives the popup, Tab
//! completion and dispatch, so the three never drift.

/// What a command does, dispatched by `App`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Slash {
    New,
    Sessions,
    Shell,
    Interrupt,
    Resume,
    /// Toggle plan mode for the next prompt.
    Plan,
    Transcript,
    Dump,
    Help,
    Quit,
}

pub struct Command {
    pub name: &'static str,
    pub slash: Slash,
    pub help: &'static str,
}

pub const COMMANDS: &[Command] = &[
    Command {
        name: "new",
        slash: Slash::New,
        help: "new session: pick a profile, type the first prompt",
    },
    Command {
        name: "sessions",
        slash: Slash::Sessions,
        help: "back to the session list",
    },
    Command {
        name: "shell",
        slash: Slash::Shell,
        help: "a shell in the sandbox",
    },
    Command {
        name: "interrupt",
        slash: Slash::Interrupt,
        help: "stop the current run",
    },
    Command {
        name: "resume",
        slash: Slash::Resume,
        help: "resume an idle session",
    },
    Command {
        name: "plan",
        slash: Slash::Plan,
        help: "plan mode on/off for the next prompt (the agent proposes, you approve)",
    },
    Command {
        name: "transcript",
        slash: Slash::Transcript,
        help: "open the loaded transcript in $EDITOR",
    },
    Command {
        name: "dump",
        slash: Slash::Dump,
        help: "write the loaded transcript to the terminal's scrollback",
    },
    Command {
        name: "help",
        slash: Slash::Help,
        help: "the key list",
    },
    Command {
        name: "quit",
        slash: Slash::Quit,
        help: "leave the TUI",
    },
];

/// The command prefix a draft names: `/` followed by letters only, on one
/// line. `None` for a prompt (anything else).
pub fn prefix(draft: &str) -> Option<&str> {
    let rest = draft.strip_prefix('/')?;
    if rest.chars().all(|c| c.is_ascii_alphabetic()) {
        Some(rest)
    } else {
        None
    }
}

/// Commands whose name starts with the draft's prefix, in table order.
pub fn matches(draft: &str) -> impl Iterator<Item = &'static Command> + '_ {
    let p = prefix(draft);
    COMMANDS
        .iter()
        .filter(move |c| p.is_some_and(|p| c.name.starts_with(p)))
}

/// Resolve a draft on Enter: an exact name, else the one command the
/// prefix selects. `None` when the draft is not a command or is ambiguous.
pub fn resolve(draft: &str) -> Option<Slash> {
    let p = prefix(draft)?;
    if let Some(c) = COMMANDS.iter().find(|c| c.name == p) {
        return Some(c.slash);
    }
    let mut it = matches(draft);
    match (it.next(), it.next()) {
        (Some(c), None) => Some(c.slash),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefixes_and_resolution() {
        assert_eq!(prefix("/sh"), Some("sh"));
        assert_eq!(prefix("/"), Some(""));
        assert_eq!(prefix("/sh x"), None);
        assert_eq!(prefix("hello"), None);
        assert_eq!(prefix("/a\nb"), None);
        assert_eq!(resolve("/shell"), Some(Slash::Shell));
        assert_eq!(resolve("/sh"), Some(Slash::Shell));
        // `/s` matches sessions and shell: ambiguous.
        assert_eq!(resolve("/s"), None);
        assert_eq!(resolve("/zzz"), None);
        assert_eq!(resolve("/"), None);
        assert_eq!(matches("/").count(), COMMANDS.len());
        assert_eq!(matches("hello").count(), 0);
    }
}
