//! What the attached session needs from the user, told to the terminal
//! three ways from one value: the Program Status Protocol (OSC 7501) root
//! record, the window title, and an alert (bell, OSC 9, OSC 777) when the
//! terminal is unfocused.
//!
//! [`attention`] is a pure function of the app. [`Reporter`] writes only
//! what changed since the last frame, into reused buffers: a steady frame
//! writes nothing and allocates nothing (ADR 0124 D5).
//!
//! The protocol: <https://www.superlogical.com/rex/docs/build/program-status>.
//! A report replaces its record completely, so every report repeats `app`
//! and `title`. `msg` is never prompt or agent text: it is a fixed phrase,
//! a count, or the question the agent waits on.

use std::fmt::Write as _;
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::app::{is_failed, App, Unseen};
use crate::transcript::Cell;

/// The `app` key: a stable id for grouping, not a label.
pub const APP: &str = "engrams";
/// Decoded limits from the protocol.
const MAX_TITLE: usize = 192;
const MAX_MSG: usize = 2048;

/// A report is on the terminal: [`clear_on_exit`] removes it, and a
/// reporter whose report was cleared (a hand-off to `$EDITOR`) sends it
/// again.
static SHOWN: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Permission,
    Question,
    Auth,
    /// A program in the shell pane reported `blocked` without a kind it
    /// recognizes: no `kind` key is sent.
    Unstated,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Idle,
    Working,
    Blocked(Kind),
    Done,
    Error,
}

impl State {
    fn wire(self) -> &'static str {
        match self {
            State::Idle => "idle",
            State::Working => "working",
            State::Blocked(_) => "blocked",
            State::Done => "done",
            State::Error => "error",
        }
    }

    /// States the user is told about when the terminal is unfocused.
    fn alerts(self) -> bool {
        matches!(self, State::Blocked(_) | State::Done | State::Error)
    }
}

/// The record's one line, beside its title.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Note<'a> {
    None,
    Text(&'a str),
    /// Prompts the harness holds for after the run.
    Queued(usize),
    /// The agent's task list: (completed, total).
    Tasks(u16, u16),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Attention<'a> {
    pub state: State,
    /// The session title; empty on the session list.
    pub title: &'a str,
    pub note: Note<'a>,
    /// 0-100, only while working. `None` is indeterminate.
    pub progress: Option<u8>,
}

/// What the attached session needs from the user now. Precedence: a
/// refused API key, a failed session, a decision, a run (or a boot) in
/// progress, a result the user has not seen, then idle.
pub fn attention(app: &App) -> Attention<'_> {
    let title = app.attached.as_ref().map_or("", |a| a.title.as_str());
    let at = |state, note| Attention {
        state,
        title,
        note,
        progress: None,
    };
    if app.auth_failed {
        return at(
            State::Blocked(Kind::Auth),
            Note::Text("Run engrams auth login"),
        );
    }
    let Some(a) = &app.attached else {
        return at(State::Idle, Note::None);
    };
    let r = &a.reducer;
    // A failed session runs nothing and asks nothing, whatever its last
    // run or decision said (`host_lost` does not settle the run).
    if r.status.as_deref().is_some_and(is_failed) {
        return match app.unseen {
            Some(Unseen::SessionFailed) => at(State::Error, Note::Text("The session failed")),
            _ => at(State::Idle, Note::None),
        };
    }
    if let Some(i) = r.pending_decision_cell() {
        return match r.transcript.get(i).map(|e| &e.cell) {
            Some(Cell::Plan { .. }) => at(
                State::Blocked(Kind::Permission),
                Note::Text("Approve the plan?"),
            ),
            Some(Cell::Question { questions, .. }) => at(
                State::Blocked(Kind::Question),
                questions
                    .first()
                    .map_or(Note::None, |q| Note::Text(&q.question)),
            ),
            _ => at(State::Blocked(Kind::Question), Note::None),
        };
    }
    // An answered decision resumes the run: working, not a short idle
    // before `run_started`.
    if r.is_running() || r.resuming {
        let note = match r.todos {
            Some((done, total)) => Note::Tasks(done, total),
            None if !r.queued.is_empty() => Note::Queued(r.queued.len()),
            None => Note::None,
        };
        return Attention {
            progress: r
                .todos
                .map(|(done, total)| (u32::from(done.min(total)) * 100 / u32::from(total)) as u8),
            ..at(State::Working, note)
        };
    }
    if r.waking {
        return at(State::Working, Note::Text("Resuming the sandbox"));
    }
    match r.status.as_deref() {
        Some("pending" | "queued" | "created") => {
            return at(State::Working, Note::Text("Waiting for a sandbox"));
        }
        Some("evacuating") => return at(State::Working, Note::Text("Moving to another host")),
        _ => {}
    }
    if !r.queued.is_empty() {
        return at(State::Working, Note::Queued(r.queued.len()));
    }
    match app.unseen {
        Some(Unseen::Done) => at(State::Done, Note::Text("Finished")),
        Some(Unseen::RunFailed) => at(State::Error, Note::Text("The run failed")),
        Some(Unseen::SessionFailed) => at(State::Error, Note::Text("The session failed")),
        None => at(State::Idle, Note::None),
    }
}

/// The sandbox's lifecycle, when it is doing something of its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sandbox {
    None,
    Waiting,
    Resuming,
    Moving,
    Saving,
}

impl Sandbox {
    fn of(app: &App) -> Sandbox {
        let Some(a) = &app.attached else {
            return Sandbox::None;
        };
        if a.reducer.waking {
            return Sandbox::Resuming;
        }
        match a.reducer.status.as_deref() {
            Some("pending" | "queued" | "created") => Sandbox::Waiting,
            Some("evacuating") => Sandbox::Moving,
            Some("evicting") => Sandbox::Saving,
            _ => Sandbox::None,
        }
    }

    fn msg(self) -> &'static str {
        match self {
            Sandbox::None => "",
            Sandbox::Waiting => "Waiting for a sandbox",
            Sandbox::Resuming => "Resuming the sandbox",
            Sandbox::Moving => "Moving to another host",
            Sandbox::Saving => "Saving the sandbox",
        }
    }
}

/// Child records past this many are left out (the protocol's floor is
/// 64 per terminal).
const MAX_CHILDREN: usize = 32;

/// What the child records are built from; they are built again only when
/// it changes.
type ChildKey = (u64, u64, Option<(u64, u64)>, Sandbox, u64);

fn child_key(app: &App) -> ChildKey {
    let a = app.attached.as_ref();
    (
        app.attach_seq,
        a.map_or(0, |a| a.reducer.children_rev),
        a.and_then(|a| a.shell.as_ref())
            .map(|s| (s.generation(), s.relay().rev())),
        Sandbox::of(app),
        app.inbox.rev(),
    )
}

/// The records beside the root: the attached session's own first (the
/// sandbox while it boots, resumes, moves or saves; each running
/// subagent; what programs in the shell pane report), then the other
/// sessions, so the cap drops those first. As (id, encoded report).
fn children(app: &App, out: &mut Vec<(String, Vec<u8>)>) {
    let attached = app.attached.as_ref();
    if let Some(a) = attached {
        let sandbox = Sandbox::of(app);
        if sandbox != Sandbox::None {
            let mut report = Vec::with_capacity(96);
            encode_report(
                &mut report,
                &Report {
                    id: Some("vm"),
                    state: State::Working,
                    app: None,
                    progress: None,
                    title: "Sandbox",
                    msg: sandbox.msg(),
                },
            );
            out.push(("vm".into(), report));
        }
        for (_, n, label) in &a.reducer.subagents {
            let id = format!("agent-{n}");
            let mut report = Vec::with_capacity(128);
            encode_report(
                &mut report,
                &Report {
                    id: Some(&id),
                    state: State::Working,
                    app: None,
                    progress: None,
                    title: label,
                    msg: "Subagent",
                },
            );
            out.push((id, report));
        }
        if let Some(sh) = &a.shell {
            for (id, report) in sh.relay().records() {
                out.push((id.to_string(), report.to_vec()));
            }
        }
    }
    let attached = attached.map(|a| a.session_id.as_str());
    for e in &app.inbox.entries {
        if out.len() >= MAX_CHILDREN {
            break;
        }
        if Some(e.session_id.as_str()) == attached {
            continue;
        }
        let mut report = Vec::with_capacity(160);
        encode_report(
            &mut report,
            &Report {
                id: Some(&e.id),
                state: e.state,
                app: None,
                progress: None,
                title: &e.title,
                msg: e.msg,
            },
        );
        out.push((e.id.clone(), report));
    }
    out.truncate(MAX_CHILDREN);
}

/// `id` is `parent` or a record beneath it.
pub(crate) fn is_at_or_under(id: &str, parent: &str) -> bool {
    id == parent
        || id
            .strip_prefix(parent)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// Writes the record, the title and the alerts. One per terminal.
#[derive(Debug)]
pub struct Reporter {
    /// The terminal takes OSC 7501 reports (detected or forced). Without
    /// it no report is sent and the title carries the state as a glyph.
    pub enabled: bool,
    last: Vec<u8>,
    next: Vec<u8>,
    title: String,
    next_title: String,
    prev: Option<State>,
    prev_seq: u64,
    child_key: Option<ChildKey>,
    last_children: Vec<(String, Vec<u8>)>,
    alert: Alert,
    /// The inbox revision and alert number already handled.
    inbox_rev: u64,
    inbox_alerted: u64,
}

impl Reporter {
    /// A reporter for this terminal: the alert follows from the
    /// environment (see [`Alert::detect`]).
    pub fn new(enabled: bool) -> Self {
        Self::with_alert(enabled, Alert::detect())
    }

    pub fn with_alert(enabled: bool, alert: Alert) -> Self {
        Reporter {
            enabled,
            alert,
            last: Vec::with_capacity(512),
            next: Vec::with_capacity(512),
            title: String::with_capacity(128),
            next_title: String::with_capacity(128),
            prev: None,
            prev_seq: 0,
            child_key: None,
            last_children: Vec::new(),
            inbox_rev: 0,
            inbox_alerted: 0,
        }
    }

    /// Alert once for each change of another session into a state the
    /// user must see, while the terminal is unfocused.
    fn inbox_alerts(&mut self, app: &App, out: &mut impl Write) -> io::Result<()> {
        if app.inbox.rev() == self.inbox_rev {
            return Ok(());
        }
        self.inbox_rev = app.inbox.rev();
        let attached = app.attached.as_ref().map(|a| a.session_id.as_str());
        let mut top = self.inbox_alerted;
        for e in &app.inbox.entries {
            if e.seq <= self.inbox_alerted {
                continue;
            }
            top = top.max(e.seq);
            if !app.focused && e.state.alerts() && Some(e.session_id.as_str()) != attached {
                alert(
                    self.alert,
                    &Attention {
                        state: e.state,
                        title: &e.title,
                        note: Note::None,
                        progress: None,
                    },
                    out,
                )?;
            }
        }
        self.inbox_alerted = top;
        Ok(())
    }

    /// Send the child records that changed, and clear the ones that went
    /// away. Clearing a record clears what is beneath it, so a child under
    /// a cleared id is sent again.
    fn children(&mut self, app: &App, out: &mut impl Write) -> io::Result<()> {
        let key = child_key(app);
        if self.child_key == Some(key) {
            return Ok(());
        }
        self.child_key = Some(key);
        let mut next = Vec::new();
        children(app, &mut next);
        let mut buf = Vec::new();
        let mut cleared: Vec<&str> = Vec::new();
        for (id, _) in &self.last_children {
            if !next.iter().any(|(n, _)| n == id) {
                encode_clear(&mut buf, id);
                cleared.push(id);
            }
        }
        for (id, report) in &next {
            let same = self
                .last_children
                .iter()
                .any(|(l, r)| l == id && r == report);
            if !same || cleared.iter().any(|c| is_at_or_under(id, c)) {
                buf.extend_from_slice(report);
            }
        }
        out.write_all(&buf)?;
        self.last_children = next;
        Ok(())
    }

    /// Write whatever changed since the last frame to `out`: the report,
    /// the window title, an alert. Writes nothing on a steady frame.
    pub fn frame(&mut self, app: &App, out: &mut impl Write) -> io::Result<()> {
        let at = attention(app);

        // An alert on a change into a state the user must see, while the
        // terminal is unfocused. A new attach is not a change, and nor is
        // its history arriving: the baseline is the first frame after it.
        let loaded = app.attached.as_ref().is_none_or(|a| a.loaded);
        let changed_session = app.attach_seq != self.prev_seq || !loaded;
        if !changed_session
            && self.prev.is_some_and(|p| p != at.state)
            && at.state.alerts()
            && !app.focused
        {
            alert(self.alert, &at, out)?;
        }
        self.prev = Some(at.state);
        // Before its history the session has no baseline yet: the first
        // frame after it counts as a new session.
        self.prev_seq = if loaded { app.attach_seq } else { u64::MAX };
        self.inbox_alerts(app, out)?;

        if self.enabled {
            // A hand-off (`$EDITOR`) cleared every record: send them again.
            if !SHOWN.load(Ordering::SeqCst) {
                self.last.clear();
                self.last_children.clear();
                self.child_key = None;
            }
            self.next.clear();
            encode(&mut self.next, None, &at);
            if self.next != self.last {
                out.write_all(&self.next)?;
                std::mem::swap(&mut self.next, &mut self.last);
                SHOWN.store(true, Ordering::SeqCst);
            }
            self.children(app, out)?;
        }

        self.next_title.clear();
        if !self.enabled {
            self.next_title.push_str(match at.state {
                State::Blocked(_) => "⚠ needs you · ",
                State::Working => "⟳ ",
                State::Done => "✓ ",
                State::Error => "✕ ",
                State::Idle => "",
            });
        }
        if !at.title.is_empty() {
            push_clean(&mut self.next_title, at.title, MAX_TITLE);
            self.next_title.push_str(" · ");
        }
        self.next_title.push_str(APP);
        if self.next_title != self.title {
            // OSC 2, terminated with ST like the report.
            write!(out, "\x1b]2;{}\x1b\\", self.next_title)?;
            std::mem::swap(&mut self.next_title, &mut self.title);
        }
        Ok(())
    }
}

/// Remove the record on the way out (exit, panic, a hand-off to another
/// program). Safe to call when nothing was reported.
pub fn clear_on_exit(out: &mut impl Write) {
    if SHOWN.swap(false, Ordering::SeqCst) {
        let _ = write!(out, "\x1b]7501;state=clear\x1b\\");
    }
}

/// How an unfocused terminal is told a session needs the user, beside
/// the OSC 7501 record. One sequence per alert: a terminal that turns
/// several of them into sounds plays them all. Chosen from the terminal
/// alone, as Codex does: a terminal that takes reports still shows the
/// bell's notification (the record's change makes its own sound).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Alert {
    /// A desktop notification, on terminals known to show one.
    Osc9,
    /// The bell, everywhere else.
    Bell,
}

impl Alert {
    /// The alert for this terminal: OSC 9 where it is known to post a
    /// notification (iTerm2, Ghostty, Kitty, Warp, WezTerm), else the
    /// bell. Inside tmux the outer terminal is unknown: the bell, which
    /// tmux passes on.
    pub fn detect() -> Alert {
        let var = |k: &str| std::env::var(k).unwrap_or_default();
        if !var("TMUX").is_empty() {
            return Alert::Bell;
        }
        let osc9 = matches!(
            var("TERM_PROGRAM").as_str(),
            "iTerm.app" | "ghostty" | "WarpTerminal" | "WezTerm"
        ) || var("TERM") == "xterm-kitty"
            || !var("KITTY_WINDOW_ID").is_empty();
        if osc9 {
            Alert::Osc9
        } else {
            Alert::Bell
        }
    }
}

/// Tell an unfocused terminal that a session needs the user.
fn alert(how: Alert, at: &Attention, out: &mut impl Write) -> io::Result<()> {
    match how {
        Alert::Bell => return out.write_all(b"\x07"),
        Alert::Osc9 => {}
    }
    let mut text = String::with_capacity(at.title.len() + 16);
    push_clean(
        &mut text,
        if at.title.is_empty() { APP } else { at.title },
        MAX_TITLE,
    );
    text.push_str(match at.state {
        State::Blocked(Kind::Auth) => ": sign in needed",
        State::Blocked(_) => ": needs you",
        State::Error => ": failed",
        _ => ": done",
    });
    write!(out, "\x1b]9;{text}\x07")
}

/// One record as a report carries it.
#[derive(Clone, Copy, Debug)]
pub struct Report<'a> {
    /// `None` is the root record.
    pub id: Option<&'a str>,
    pub state: State,
    /// `None` inherits the nearest ancestor's.
    pub app: Option<&'a str>,
    /// Sent with `working` and `blocked` only.
    pub progress: Option<u8>,
    pub title: &'a str,
    pub msg: &'a str,
}

/// Encode one report. Free text is cleaned and cut to the limits.
pub fn encode_report(out: &mut Vec<u8>, r: &Report) {
    out.extend_from_slice(b"\x1b]7501;state=");
    out.extend_from_slice(r.state.wire().as_bytes());
    if let Some(id) = r.id {
        out.extend_from_slice(b":id=");
        out.extend_from_slice(id.as_bytes());
    }
    if let Some(app) = r.app {
        out.extend_from_slice(b":app=");
        out.extend_from_slice(app.as_bytes());
    }
    if let State::Blocked(kind) = r.state {
        out.extend_from_slice(match kind {
            Kind::Permission => b":kind=permission",
            Kind::Question => b":kind=question",
            Kind::Auth => b":kind=auth",
            Kind::Unstated => b"",
        });
    }
    if let (State::Working | State::Blocked(_), Some(p)) = (r.state, r.progress) {
        let mut n = Small::default();
        let _ = write!(n, ":progress={}", p.min(100));
        out.extend_from_slice(n.as_bytes());
    }
    text_pair(out, b":title=", r.title, MAX_TITLE);
    text_pair(out, b":msg=", r.msg, MAX_MSG);
    out.extend_from_slice(b"\x1b\\");
}

/// Remove record `id` and every record beneath it.
pub fn encode_clear(out: &mut Vec<u8>, id: &str) {
    out.extend_from_slice(b"\x1b]7501;state=clear:id=");
    out.extend_from_slice(id.as_bytes());
    out.extend_from_slice(b"\x1b\\");
}

/// Encode the TUI's own record `id` (`None` is the root record).
pub fn encode(out: &mut Vec<u8>, id: Option<&str>, at: &Attention) {
    let mut n = Small::default();
    let msg = match at.note {
        Note::None => "",
        Note::Text(t) => t,
        Note::Queued(k) => {
            let _ = write!(
                n,
                "{k} {} queued",
                if k == 1 { "prompt" } else { "prompts" }
            );
            n.as_str()
        }
        Note::Tasks(done, total) => {
            let _ = write!(n, "{done} of {total} tasks done");
            n.as_str()
        }
    };
    encode_report(
        out,
        &Report {
            id,
            state: at.state,
            app: Some(APP),
            progress: at.progress,
            title: at.title,
            msg,
        },
    );
}

/// Characters a free-text value never carries: control characters
/// (which make the terminal discard the whole report) and the invisible
/// formatting ones (bidi overrides, zero-width marks).
fn dropped(c: char) -> bool {
    c.is_control()
        || matches!(c,
            '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2069}' | '\u{FEFF}')
}

/// The text a free-text value carries: trimmed, without dropped
/// characters, cut at a character boundary to `max` bytes.
fn clean(text: &str, max: usize) -> impl Iterator<Item = char> + '_ {
    let mut bytes = 0;
    text.trim()
        .chars()
        .filter(|&c| !dropped(c))
        .take_while(move |c| {
            bytes += c.len_utf8();
            bytes <= max
        })
}

fn push_clean(out: &mut String, text: &str, max: usize) {
    out.extend(clean(text, max));
}

/// `key` and the base64 of the cleaned `text`; nothing when it is empty.
fn text_pair(out: &mut Vec<u8>, key: &[u8], text: &str, max: usize) {
    let mut chars = clean(text, max).peekable();
    if chars.peek().is_none() {
        return;
    }
    out.extend_from_slice(key);
    let mut b64 = Base64::new(out);
    let mut utf8 = [0u8; 4];
    for c in chars {
        b64.push(c.encode_utf8(&mut utf8).as_bytes());
    }
    b64.finish();
}

/// Standard base64, streamed: bytes go in, encoded text goes out with no
/// intermediate buffer.
struct Base64<'a> {
    out: &'a mut Vec<u8>,
    carry: [u8; 3],
    n: usize,
}

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

impl<'a> Base64<'a> {
    fn new(out: &'a mut Vec<u8>) -> Self {
        Base64 {
            out,
            carry: [0; 3],
            n: 0,
        }
    }

    fn push(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.carry[self.n] = b;
            self.n += 1;
            if self.n == 3 {
                self.group(3);
                self.n = 0;
            }
        }
    }

    fn group(&mut self, len: usize) {
        let [a, b, c] = self.carry;
        let v = (u32::from(a) << 16) | (u32::from(b) << 8) | u32::from(c);
        for i in 0..4 {
            if i <= len {
                self.out.push(ALPHABET[((v >> (18 - 6 * i)) & 63) as usize]);
            } else {
                self.out.push(b'=');
            }
        }
    }

    fn finish(mut self) {
        if self.n > 0 {
            self.carry[self.n..].fill(0);
            let n = self.n;
            self.group(n);
        }
    }
}

/// A short formatted value on the stack.
#[derive(Default)]
struct Small {
    buf: [u8; 32],
    len: usize,
}

impl Small {
    fn as_bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }

    fn as_str(&self) -> &str {
        // Only `fmt::Write` fills it, with whole `str`s.
        std::str::from_utf8(self.as_bytes()).unwrap_or("")
    }
}

impl std::fmt::Write for Small {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        let end = self.len + s.len();
        let dst = self.buf.get_mut(self.len..end).ok_or(std::fmt::Error)?;
        dst.copy_from_slice(s.as_bytes());
        self.len = end;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b64(s: &str) -> String {
        let mut out = Vec::new();
        let mut e = Base64::new(&mut out);
        e.push(s.as_bytes());
        e.finish();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn base64_matches_the_rfc_4648_vectors() {
        for (plain, enc) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(b64(plain), enc, "{plain:?}");
        }
    }

    fn report(at: &Attention) -> String {
        let mut out = Vec::new();
        encode(&mut out, None, at);
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn a_blocked_report_carries_kind_title_and_msg() {
        let at = Attention {
            state: State::Blocked(Kind::Question),
            title: "fix login",
            note: Note::Text("Which branch?"),
            progress: Some(40),
        };
        assert_eq!(
            report(&at),
            "\x1b]7501;state=blocked:app=engrams:kind=question:progress=40\
             :title=Zml4IGxvZ2lu:msg=V2hpY2ggYnJhbmNoPw==\x1b\\"
        );
    }

    #[test]
    fn progress_and_counts_ride_only_on_working() {
        let at = Attention {
            state: State::Working,
            title: "",
            note: Note::Tasks(2, 5),
            progress: Some(40),
        };
        assert_eq!(
            report(&at),
            format!(
                "\x1b]7501;state=working:app=engrams:progress=40:msg={}\x1b\\",
                b64("2 of 5 tasks done")
            )
        );
        let done = Attention {
            state: State::Done,
            ..at
        };
        assert!(!report(&done).contains("progress"));
    }

    #[test]
    fn free_text_drops_control_and_bidi_characters_and_keeps_the_limit() {
        let at = Attention {
            state: State::Idle,
            title: "  a\u{7}b\u{1b}[31mc\u{202E}d\n  ",
            note: Note::None,
            progress: None,
        };
        assert!(report(&at).contains(&format!(":title={}", b64("ab[31mcd"))));

        // 300 two-byte characters: cut to 96 of them (192 bytes), never
        // inside one.
        let long = "é".repeat(300);
        let at = Attention { title: &long, ..at };
        assert!(report(&at).contains(&format!(":title={}\x1b", b64(&"é".repeat(96)))));
    }

    #[test]
    fn an_empty_or_all_control_value_is_left_out() {
        let at = Attention {
            state: State::Idle,
            title: "\n\t",
            note: Note::Text(""),
            progress: None,
        };
        assert_eq!(report(&at), "\x1b]7501;state=idle:app=engrams\x1b\\");
    }

    #[test]
    fn a_child_record_names_its_id() {
        let at = Attention {
            state: State::Done,
            title: "",
            note: Note::None,
            progress: None,
        };
        let mut out = Vec::new();
        encode(&mut out, Some("s1/agent-2"), &at);
        assert_eq!(out, b"\x1b]7501;state=done:id=s1/agent-2:app=engrams\x1b\\");
    }

    #[test]
    fn the_largest_report_stays_inside_the_sequence_limit() {
        let title = "x".repeat(10_000);
        let msg = "y".repeat(10_000);
        let at = Attention {
            state: State::Blocked(Kind::Permission),
            title: &title,
            note: Note::Text(&msg),
            progress: None,
        };
        let mut out = Vec::new();
        encode(&mut out, Some(&"a".repeat(128)), &at);
        assert!(out.len() <= 4096, "{}", out.len());
    }
}
