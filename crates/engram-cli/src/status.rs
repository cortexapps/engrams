//! Program status (OSC 7501) for the verbs that wait: the terminal shows
//! that `auth login` waits on the browser, that `session exec` runs, and
//! how each ended. Reports go to stderr, and only when stderr is a
//! terminal, so stdout stays clean for scripts. A terminal that does not
//! know the protocol ignores them. `ENGRAMS_PROGRAM_STATUS=0` turns them
//! off.
//!
//! The process ends right after a verb does, so the last report is a
//! result (`done` or `error`): the terminal keeps it for the user to see,
//! and drops a `working` or `blocked` one by itself at exit.

use std::io::{IsTerminal, Write};

use engram_tui::status::{encode_report, Kind, Report, State, APP};

use crate::output::{CliError, CliResult};

pub struct Status {
    on: bool,
    title: &'static str,
}

impl Status {
    pub fn new(title: &'static str) -> Self {
        let off = std::env::var("ENGRAMS_PROGRAM_STATUS").is_ok_and(|v| v == "0");
        Status {
            on: !off && std::io::stderr().is_terminal(),
            title,
        }
    }

    pub fn working(&self, msg: &str) {
        self.report(State::Working, msg);
    }

    pub fn needs_sign_in(&self, msg: &str) {
        self.report(State::Blocked(Kind::Auth), msg);
    }

    /// Report how the verb ended: `done` with `ok`, or `error` with the
    /// failure's own line (or `failed` when the verb already printed it).
    pub fn finish<T>(&self, result: &CliResult<T>, ok: &str, failed: &str) {
        let (state, msg) = outcome(result, ok, failed);
        self.report(state, msg);
    }

    fn report(&self, state: State, msg: &str) {
        if !self.on {
            return;
        }
        let mut out = Vec::with_capacity(256);
        encode_report(
            &mut out,
            &Report {
                id: None,
                state,
                app: Some(APP),
                progress: None,
                title: self.title,
                msg,
            },
        );
        let mut err = std::io::stderr().lock();
        let _ = err.write_all(&out).and_then(|()| err.flush());
    }
}

/// The result record of a verb that ended with `result`.
fn outcome<'a, T>(result: &'a CliResult<T>, ok: &'a str, failed: &'a str) -> (State, &'a str) {
    match result {
        Ok(_) => (State::Done, ok),
        Err(CliError::Message(m)) => (State::Error, m.lines().next().unwrap_or(m)),
        Err(CliError::Exit(_)) => (State::Error, failed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_verb_ends_as_done_or_error() {
        assert_eq!(outcome(&Ok(()), "ok", "failed"), (State::Done, "ok"));
        let e: CliResult = Err(CliError::Message("the code expired\nrun it again".into()));
        assert_eq!(
            outcome(&e, "ok", "failed"),
            (State::Error, "the code expired")
        );
        let e: CliResult = Err(CliError::Exit(2));
        assert_eq!(outcome(&e, "ok", "failed"), (State::Error, "failed"));
    }
}
