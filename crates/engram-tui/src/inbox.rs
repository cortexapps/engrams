//! Your other sessions, for the terminal: each one that needs you, works,
//! or finished while you were elsewhere is a child record
//! `sessions/<id>`, so the terminal shows them beside the attached one.
//!
//! The list comes from polling `ListTasks` (and, for active sessions,
//! their newest run event). A result (`done`, `error`) is made only by a
//! change this TUI saw between two polls, never by the first poll, and it
//! stays until you open that session. What a poll could not learn (a
//! failed or skipped run read) is unknown, never "stopped".

use std::collections::HashMap;

use crate::app::is_failed;
use crate::sessions::{RunState, SessionRow};
use crate::status::{Kind, State};

/// Records past this many are left out.
const MAX_ENTRIES: usize = 24;

/// What a session is doing, from one poll.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Live {
    /// No run, or none this TUI saw end.
    Quiet,
    /// Booting or moving.
    Lifecycle(&'static str),
    Running,
    /// The newest run ended: ok or failed.
    Ended(bool),
    Blocked,
    Failed,
}

/// `None` when the poll could not say (an active session whose run read
/// failed or was skipped).
fn live(row: &SessionRow) -> Option<Live> {
    if is_failed(&row.status) {
        return Some(Live::Failed);
    }
    if row.needs_you {
        return Some(Live::Blocked);
    }
    match row.status.as_str() {
        "pending" | "queued" | "created" => return Some(Live::Lifecycle("Waiting for a sandbox")),
        "evacuating" => return Some(Live::Lifecycle("Moving to another host")),
        _ => {}
    }
    match row.running {
        Some(RunState::Running) => Some(Live::Running),
        Some(RunState::Ended { ok }) => Some(Live::Ended(ok)),
        Some(RunState::Interrupted | RunState::NoRun) => Some(Live::Quiet),
        None if row.status == "active" => None,
        None => Some(Live::Quiet),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub session_id: String,
    /// The record id: `sessions/<id>`.
    pub id: String,
    pub title: String,
    pub state: State,
    pub msg: &'static str,
    /// Non-zero when a change this TUI saw made the entry: the reporter
    /// alerts once per new value.
    pub seq: u64,
}

#[derive(Debug, Default)]
pub struct Inbox {
    pub entries: Vec<Entry>,
    seen: HashMap<String, Live>,
    primed: bool,
    rev: u64,
    seq: u64,
}

impl Inbox {
    /// Bumped on every change to `entries`.
    pub fn rev(&self) -> u64 {
        self.rev
    }

    /// Fold one poll. `attached` is left out: it is the root record.
    pub fn update(&mut self, rows: &[SessionRow], attached: Option<&str>) {
        let mut next: Vec<Entry> = Vec::new();
        for row in rows {
            let before = self.seen.get(&row.session_id).copied();
            // Unknown: as last known, and no change. Unknown at first sight
            // is quiet: a result needs a session seen running or blocked.
            let now = live(row).or(before).unwrap_or(Live::Quiet);
            self.seen.insert(row.session_id.clone(), now);
            if Some(row.session_id.as_str()) == attached {
                continue;
            }
            // A change this TUI saw (not the first sight of the session).
            let observed = self.primed && before.is_some_and(|b| b != now);
            let was_live = matches!(before, Some(Live::Running | Live::Blocked));
            let kept = self.entries.iter().find(|e| e.session_id == row.session_id);
            let entry = match now {
                Live::Blocked => {
                    Some((State::Blocked(Kind::Question), "Waiting for you", observed))
                }
                Live::Running => Some((State::Working, "Working", false)),
                Live::Lifecycle(msg) => Some((State::Working, msg, false)),
                Live::Failed if observed => Some((State::Error, "The session failed", true)),
                Live::Ended(true) if observed && was_live => Some((State::Done, "Finished", true)),
                Live::Ended(false) if observed && was_live => {
                    Some((State::Error, "The run failed", true))
                }
                // A result stays until the session is opened.
                Live::Quiet | Live::Ended(_) | Live::Failed => kept
                    .filter(|e| matches!(e.state, State::Done | State::Error))
                    .map(|e| (e.state, e.msg, false)),
            };
            let Some((state, msg, alert)) = entry else {
                continue;
            };
            let seq = if alert {
                self.seq += 1;
                self.seq
            } else {
                kept.filter(|e| e.state == state).map_or(0, |e| e.seq)
            };
            next.push(Entry {
                session_id: row.session_id.clone(),
                id: record_id(&row.session_id),
                title: row.title.clone(),
                state,
                msg,
                seq,
            });
        }
        // Sessions no longer listed are forgotten.
        self.seen
            .retain(|id, _| rows.iter().any(|r| r.session_id == *id));
        next.truncate(MAX_ENTRIES);
        self.primed = true;
        if next != self.entries {
            self.entries = next;
            self.rev += 1;
        }
    }

    /// The user opened `session_id`: its result has been seen.
    pub fn opened(&mut self, session_id: &str) {
        let before = self.entries.len();
        self.entries.retain(|e| e.session_id != session_id);
        if self.entries.len() != before {
            self.rev += 1;
        }
    }
}

/// `sessions/<id>`, the id cut to the protocol's segment: characters
/// outside it dropped, a UUID's hyphens dropped (36 → 32), at most 32.
fn record_id(session_id: &str) -> String {
    let keep = |c: &char| c.is_ascii_alphanumeric() || "_.+-".contains(*c);
    let mut seg: String = session_id.chars().filter(keep).collect();
    if seg.len() > 32 {
        seg.retain(|c| c != '-');
    }
    seg.truncate(32);
    if seg.is_empty() {
        seg.push('_');
    }
    format!("sessions/{seg}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, status: &str) -> SessionRow {
        SessionRow {
            session_id: id.into(),
            title: format!("t-{id}"),
            status: status.into(),
            age_secs: None,
            needs_you: false,
            running: None,
        }
    }

    fn active(id: &str, run: Option<RunState>) -> SessionRow {
        SessionRow {
            running: run,
            ..row(id, "active")
        }
    }

    fn states(i: &Inbox) -> Vec<(&str, State)> {
        i.entries.iter().map(|e| (e.id.as_str(), e.state)).collect()
    }

    #[test]
    fn a_uuid_fits_one_segment() {
        assert_eq!(
            record_id("ce1d9809-0c20-41a9-8b72-43cfffaa5baa"),
            "sessions/ce1d98090c2041a98b7243cfffaa5baa"
        );
        assert_eq!(record_id("s-1"), "sessions/s-1");
        assert_eq!(record_id("a b/c"), "sessions/abc");
    }

    #[test]
    fn the_first_poll_reports_what_is_true_now_and_no_results() {
        let mut i = Inbox::default();
        let mut waiting = row("a", "active");
        waiting.needs_you = true;
        i.update(
            &[
                waiting,
                active("b", Some(RunState::Running)),
                active("e", Some(RunState::Ended { ok: true })),
                row("c", "failed"),
                row("d", "idle"),
            ],
            None,
        );
        assert_eq!(
            states(&i),
            [
                ("sessions/a", State::Blocked(Kind::Question)),
                ("sessions/b", State::Working)
            ]
        );
        assert!(
            i.entries.iter().all(|e| e.seq == 0),
            "no alert on the first poll"
        );
    }

    #[test]
    fn a_run_that_ends_between_polls_is_done_until_opened() {
        let mut i = Inbox::default();
        i.update(&[active("a", Some(RunState::Running))], None);
        let ended = active("a", Some(RunState::Ended { ok: true }));
        i.update(std::slice::from_ref(&ended), None);
        assert_eq!(states(&i), [("sessions/a", State::Done)]);
        let seq = i.entries[0].seq;
        assert!(seq > 0);
        // It stays across polls, with the same alert number, and when the
        // session is later parked.
        i.update(std::slice::from_ref(&ended), None);
        assert_eq!(i.entries[0].seq, seq);
        i.update(&[row("a", "parked")], None);
        assert_eq!(states(&i), [("sessions/a", State::Done)]);
        i.opened("a");
        assert!(i.entries.is_empty());
    }

    #[test]
    fn a_failed_run_is_an_error_and_an_interrupted_one_is_nothing() {
        let mut i = Inbox::default();
        i.update(
            &[
                active("a", Some(RunState::Running)),
                active("b", Some(RunState::Running)),
            ],
            None,
        );
        i.update(
            &[
                active("a", Some(RunState::Ended { ok: false })),
                active("b", Some(RunState::Interrupted)),
            ],
            None,
        );
        assert_eq!(states(&i), [("sessions/a", State::Error)]);
        assert_eq!(i.entries[0].msg, "The run failed");
    }

    #[test]
    fn an_unknown_run_state_is_not_a_finish() {
        let mut i = Inbox::default();
        i.update(&[active("a", Some(RunState::Running))], None);
        // The run read failed, or the session was past the checked ones.
        i.update(&[active("a", None)], None);
        assert_eq!(states(&i), [("sessions/a", State::Working)]);
        i.update(&[active("a", Some(RunState::Running))], None);
        assert_eq!(states(&i), [("sessions/a", State::Working)]);
        assert!(i.entries.iter().all(|e| e.seq == 0), "no alert");
    }

    #[test]
    fn a_session_that_fails_between_polls_is_an_error() {
        let mut i = Inbox::default();
        i.update(&[row("a", "active")], None);
        i.update(&[row("a", "host_lost")], None);
        assert_eq!(states(&i), [("sessions/a", State::Error)]);
    }

    #[test]
    fn the_attached_session_is_left_out() {
        let mut i = Inbox::default();
        let mut r = row("a", "active");
        r.needs_you = true;
        i.update(&[r], Some("a"));
        assert!(i.entries.is_empty());
    }

    #[test]
    fn an_unchanged_poll_is_not_a_change() {
        let mut i = Inbox::default();
        let r = active("a", Some(RunState::Running));
        i.update(std::slice::from_ref(&r), None);
        let rev = i.rev();
        i.update(std::slice::from_ref(&r), None);
        assert_eq!(i.rev(), rev);
    }
}
