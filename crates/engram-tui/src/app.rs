//! The application state and its pure update step. The runtime (`lib.rs`)
//! feeds it messages and executes the commands it emits; the renderer
//! (`render.rs`) draws it. Nothing here touches the terminal or the
//! network, so tests drive it directly.

use std::time::{Duration, Instant};

use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};

use crate::commands::{self, Slash};
use crate::composer::Composer;
use crate::event::{Envelope, Event};
use crate::inbox::Inbox;
use crate::keymap::{resolve, Action};
use crate::reducer::{is_terminal, Reducer, RunOutcome};
use crate::sessions::{fuzzy, ProfileRow, RunState, SessionRow};
use crate::shell::ShellPane;
use crate::transcript::Cell;

/// Events per history page (the web's window size).
pub const PAGE: i64 = 200;

/// Rows of a `body` (everything above the footer) the shell pane takes in
/// fullscreen mode, header included: two fifths, never so much that the
/// title, a transcript row and the composer lose their rows.
pub fn shell_pane_rows(body: u16) -> u16 {
    (body * 2 / 5).clamp(3, body.saturating_sub(3).max(3))
}
const DOUBLE_TAP: Duration = Duration::from_secs(1);

/// What the runtime must do after an update.
#[derive(Debug, PartialEq, Eq)]
pub enum Cmd {
    LoadSessions,
    Attach {
        session_id: String,
    },
    Detach,
    LoadOlder {
        session_id: String,
        before_idx: i64,
    },
    Send {
        session_id: String,
        prompt_id: String,
        text: String,
        /// Set only when the prompt changes the harness mode (`/plan`).
        harness_mode: Option<String>,
    },
    Interrupt {
        session_id: String,
    },
    Dequeue {
        session_id: String,
        prompt_id: String,
    },
    /// Open the draft in `$EDITOR`; the runtime answers with
    /// [`App::draft_edited`].
    EditDraft {
        text: String,
    },
    /// Answer a deferred tool call (question, plan decision).
    Complete {
        session_id: String,
        tool_call_id: String,
        result_json: String,
    },
    /// `SessionService.Resume` (`/resume`).
    Resume {
        session_id: String,
    },
    /// Open the transcript text in `$EDITOR`, read-only in effect: the
    /// runtime discards the file (`/transcript`).
    ViewTranscript {
        text: String,
    },
    /// Write the transcript text to the terminal's own scrollback
    /// (`/dump`, fullscreen mode).
    Dump {
        text: String,
    },
    /// The profiles for the new-session picker.
    LoadProfiles,
    /// `TaskService.CreateTask`; the runtime answers with `NetMsg::Created`.
    CreateTask {
        profile_id: String,
        prompt: String,
    },
    /// Open the session shell at this screen size.
    ShellOpen {
        session_id: String,
        cols: u16,
        rows: u16,
    },
    ShellInput(Vec<u8>),
    ShellResize {
        cols: u16,
        rows: u16,
    },
    ShellClose,
    /// Ask the terminal to set its clipboard (OSC 52): the runtime writes
    /// it between frames.
    CopyToTerminal(String),
    Quit,
}

/// A mouse selection in the transcript. The ends are (cell, layout row,
/// column) so the selection stays on its text while the transcript
/// scrolls.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Selection {
    pub anchor: (usize, u16, u16),
    pub head: (usize, u16, u16),
    /// The pointer moved since the press: a drag, not a click.
    pub moved: bool,
}

impl Selection {
    /// The ends in transcript order.
    pub fn range(&self) -> ((usize, u16, u16), (usize, u16, u16)) {
        if self.anchor <= self.head {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }
}

/// How a copy reached the clipboard.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Copied {
    /// The platform's clipboard command took it.
    Native,
    /// Sent to the terminal (OSC 52); terminals do not confirm it.
    Terminal,
    Failed(String),
}

/// Transcript search (Ctrl+T, fullscreen). `cursor` is the cell of the
/// current hit; Enter walks to older hits and wraps.
#[derive(Debug, Default)]
pub struct Search {
    pub query: String,
    /// `query` lowercased, what the matcher compares against.
    pub lower: String,
    pub cursor: Option<usize>,
}

/// Inputs to the update step.
#[derive(Debug)]
pub enum Msg {
    Key(KeyEvent),
    Paste(String),
    Mouse(MouseEvent),
    Resize(u16, u16),
    Focus(bool),
    Tick,
    Net(NetMsg),
    /// A copy of `chars` characters finished.
    Copied {
        chars: usize,
        how: Copied,
        /// The text, when the platform command failed and the terminal
        /// should be asked instead.
        fallback: Option<String>,
    },
}

/// A result of the attached session the user has not seen yet: the
/// program status reports it as `done` or `error` until deliberate input
/// (a key, a paste, a click, a scroll) acknowledges it. Only live events
/// make one; a history replay never does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unseen {
    Done,
    RunFailed,
    SessionFailed,
}

#[derive(Debug)]
pub enum NetMsg {
    Sessions(Result<Vec<SessionRow>, String>),
    /// Which listed sessions have a run in progress (active ones only):
    /// the end of a session-list load. `known` is false when the server
    /// could not say what state the sessions are in.
    SessionsRunning {
        running: Vec<(String, RunState)>,
        known: bool,
    },
    /// The orchestrator refused the API key: the user has to sign in.
    AuthFailed,
    Profiles(Result<Vec<ProfileRow>, String>),
    /// The task was created: its primary session id and title.
    Created(Result<(String, String), String>),
    /// A history page. `older` pages prepend; the first page replaces.
    History {
        session_id: String,
        events: Vec<Envelope>,
        older: bool,
    },
    Live {
        session_id: String,
        envelope: Envelope,
    },
    Status {
        session_id: String,
        status: String,
        title: Option<String>,
    },
    Reconnecting {
        session_id: String,
        attempt: u32,
    },
    Connected {
        session_id: String,
    },
    Failed {
        session_id: String,
        error: String,
    },
    /// An older history page could not be loaded: paging may retry.
    OlderFailed {
        session_id: String,
        error: String,
    },
    SendFailed {
        session_id: String,
        prompt_id: String,
        error: String,
    },
    CompleteFailed {
        session_id: String,
        tool_call_id: String,
        error: String,
    },
    ShellOpened {
        session_id: String,
    },
    ShellOutput {
        session_id: String,
        bytes: bytes::Bytes,
    },
    ShellTitle {
        session_id: String,
        title: String,
    },
    /// The shell ended: `None` for a clean close, else the reason.
    ShellEnded {
        session_id: String,
        error: Option<String>,
    },
}

#[derive(Debug)]
pub struct Attached {
    pub session_id: String,
    pub title: String,
    pub reducer: Reducer,
    pub composer: Composer,
    /// Rows scrolled up from the bottom; 0 follows new output.
    pub scroll_up: u32,
    /// Events that landed while scrolled up.
    pub unseen: u32,
    pub oldest_idx: Option<i64>,
    pub newest_idx: Option<i64>,
    pub exhausted: bool,
    pub loading_older: bool,
    pub loaded: bool,
    pub reconnecting: Option<u32>,
    pub error: Option<String>,
    pub run_started_at: Option<Instant>,
    /// Rows the transcript area had on the last draw (for paging).
    pub page_rows: u16,
    /// The last scroll-up walk hit the top of memory: load more.
    pub want_older: bool,
    /// The (cell, layout row) under each transcript row of the last
    /// frame, from `hits_y` down: a click or a drag resolves through it.
    pub hits: Vec<(usize, u16)>,
    /// The left edge of the transcript area of the last frame.
    pub hits_x: u16,
    pub selection: Option<Selection>,
    /// Copy the selection on the next frame: the text is read from what
    /// the frame shows.
    pub copy_request: bool,
    /// The text of the last copy request, for the runtime.
    pub copied: Option<String>,
    pub hits_y: u16,
    pub search: Option<Search>,
    /// The composer collects feedback for a plan rejection (Tab).
    pub feedback: bool,
    /// The shell pane (Ctrl+`), open until closed or the shell exits.
    pub shell: Option<ShellPane>,
    /// `/plan`: the mode the next prompt asks for, when it differs from
    /// the session's current mode (the web's not-yet-sent toggle).
    pub mode_override: Option<String>,
    /// The highlighted command in the slash popup.
    pub slash_sel: usize,
}

impl Attached {
    pub fn new(session_id: String, title: String) -> Self {
        Attached {
            session_id,
            title,
            reducer: Reducer::new(),
            composer: Composer::default(),
            scroll_up: 0,
            unseen: 0,
            oldest_idx: None,
            newest_idx: None,
            exhausted: false,
            loading_older: false,
            loaded: false,
            reconnecting: None,
            error: None,
            run_started_at: None,
            page_rows: 20,
            want_older: false,
            hits: Vec::new(),
            hits_x: 0,
            selection: None,
            copy_request: false,
            copied: None,
            hits_y: 0,
            search: None,
            feedback: false,
            shell: None,
            mode_override: None,
            slash_sel: 0,
        }
    }

    /// Submit the pending decision at cell `i` and lock the cell.
    fn complete(&mut self, i: usize, tool_call_id: String, cmds: &mut Vec<Cmd>) -> bool {
        let Some(e) = self.reducer.transcript.get(i) else {
            return false;
        };
        let (result_json, approved) = match &e.cell {
            Cell::Question {
                questions, picks, ..
            } => {
                if picks.iter().any(Vec::is_empty) {
                    return false;
                }
                let mut map = serde_json::Map::new();
                for (q, p) in questions.iter().zip(picks) {
                    let labels: Vec<serde_json::Value> = p
                        .iter()
                        .filter_map(|&j| q.options.get(j as usize))
                        .map(|o| serde_json::Value::String(o.label.clone()))
                        .collect();
                    map.insert(q.question.clone(), serde_json::Value::Array(labels));
                }
                (serde_json::Value::Object(map).to_string(), None)
            }
            Cell::Plan { choice, .. } => {
                let Some(approve) = *choice else {
                    return false;
                };
                let mut map = serde_json::Map::new();
                map.insert(
                    "decision".into(),
                    serde_json::Value::String(if approve { "approve" } else { "reject" }.into()),
                );
                if !approve && self.feedback {
                    let text = self.composer.text().trim().to_string();
                    if !text.is_empty() {
                        map.insert("feedback".into(), serde_json::Value::String(text));
                    }
                }
                (serde_json::Value::Object(map).to_string(), Some(approve))
            }
            _ => return false,
        };
        if self.feedback {
            self.feedback = false;
            self.composer.clear();
        }
        self.reducer.decide(&tool_call_id, approved);
        cmds.push(Cmd::Complete {
            session_id: self.session_id.clone(),
            tool_call_id,
            result_json,
        });
        true
    }

    /// A key for the pending decision. Returns true when consumed.
    fn decision_key(&mut self, action: Action, cmds: &mut Vec<Cmd>) -> bool {
        let Some((i, tool_call_id)) = self.reducer.pending_decision() else {
            self.feedback = false;
            return false;
        };
        if self.feedback {
            return match action {
                Action::Send => self.complete(i, tool_call_id, cmds),
                Action::Escape => {
                    self.feedback = false;
                    true
                }
                _ => false,
            };
        }
        if !self.composer.is_empty() {
            return false;
        }
        let Some(e) = self.reducer.transcript.get_mut(i) else {
            return false;
        };
        let consumed = match (&mut e.cell, action) {
            (
                Cell::Question {
                    questions,
                    picks,
                    current,
                    ..
                },
                Action::Char(c @ '1'..='9'),
            ) => {
                let qi = *current as usize;
                let n = (c as u8 - b'1') as u16;
                if let (Some(q), Some(p)) = (questions.get(qi), picks.get_mut(qi)) {
                    if (n as usize) < q.options.len() {
                        if q.multi_select {
                            match p.iter().position(|&x| x == n) {
                                Some(pos) => {
                                    p.remove(pos);
                                }
                                None => p.push(n),
                            }
                        } else {
                            *p = vec![n];
                        }
                    }
                }
                true
            }
            (
                Cell::Question {
                    questions,
                    picks,
                    current,
                    ..
                },
                Action::Send,
            ) => {
                let qi = *current as usize;
                if picks.get(qi).is_some_and(|p| p.is_empty()) {
                    true
                } else if qi + 1 < questions.len() {
                    *current += 1;
                    true
                } else {
                    e.invalidate();
                    return self.complete(i, tool_call_id, cmds);
                }
            }
            (Cell::Plan { choice, .. }, Action::Char('1')) => {
                *choice = Some(true);
                true
            }
            (Cell::Plan { choice, .. }, Action::Char('2')) => {
                *choice = Some(false);
                true
            }
            (Cell::Plan { choice, .. }, Action::Char('\t')) => {
                *choice = Some(false);
                e.invalidate();
                self.feedback = true;
                return true;
            }
            (Cell::Plan { choice, .. }, Action::Send) => {
                if choice.is_none() {
                    true
                } else {
                    e.invalidate();
                    return self.complete(i, tool_call_id, cmds);
                }
            }
            _ => false,
        };
        if consumed {
            e.invalidate();
        }
        consumed
    }

    /// Walk to the next older cell containing the search query (wrapping
    /// at the top) and scroll so it heads the viewport. False when
    /// nothing matches.
    fn search_next(&mut self, width: u16) -> bool {
        let Some(s) = &mut self.search else {
            return false;
        };
        s.lower = s.query.to_lowercase();
        if s.lower.is_empty() {
            s.cursor = None;
            return false;
        }
        let t = &mut self.reducer.transcript;
        let first = t.first_index();
        let len = t.len();
        if len == 0 {
            return false;
        }
        let from = s.cursor.unwrap_or(len);
        let hit = (first..from)
            .rev()
            .chain((from..len).rev())
            .find(|&i| t.get(i).is_some_and(|e| e.matches(&s.lower)));
        let Some(i) = hit else {
            s.cursor = None;
            return false;
        };
        s.cursor = Some(i);
        t.reveal(i);
        let rows = t.rows_from(i, width);
        self.scroll_up = rows.saturating_sub(self.page_rows as u32);
        if self.scroll_up == 0 {
            self.unseen = 0;
        }
        true
    }

    fn apply_envelope(&mut self, env: &Envelope) {
        if let Some(idx) = env.idx {
            if self.newest_idx.is_some_and(|n| idx <= n) {
                return; // replayed
            }
            self.newest_idx = Some(idx);
            if self.oldest_idx.is_none() {
                self.oldest_idx = Some(idx);
            }
        }
        let was_running = self.reducer.is_running();
        self.reducer
            .apply(Event::decode(&env.kind, &env.payload_json));
        let running = self.reducer.is_running();
        if running && !was_running {
            self.run_started_at = Some(Instant::now());
        } else if !running {
            self.run_started_at = None;
        }
        if self.scroll_up > 0 {
            self.unseen += 1;
        }
    }

    /// The mode the next prompt runs in: the override, else the session's.
    pub fn composer_mode(&self) -> &str {
        self.mode_override.as_deref().unwrap_or(&self.reducer.mode)
    }

    pub fn can_send(&self) -> bool {
        !self.reducer.status.as_deref().is_some_and(is_terminal)
    }
}

/// Which step of the new-session flow has the keys.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NewStage {
    /// Pick a profile from the list (type to filter).
    Pick,
    /// Type the first prompt.
    Prompt,
}

/// The new-session overlay (Ctrl+N, `/new`): a profile picker, then the
/// first prompt, then `CreateTask` and an attach to the primary session.
#[derive(Debug)]
pub struct NewSession {
    pub profiles: Vec<ProfileRow>,
    pub loading: bool,
    pub error: Option<String>,
    pub selected: usize,
    pub filter: String,
    pub stage: NewStage,
    pub composer: Composer,
    /// `CreateTask` is in flight; Enter does nothing until it answers.
    pub creating: bool,
}

impl NewSession {
    fn new() -> Self {
        NewSession {
            profiles: Vec::new(),
            loading: true,
            error: None,
            selected: 0,
            filter: String::new(),
            stage: NewStage::Pick,
            composer: Composer::default(),
            creating: false,
        }
    }

    /// Indices of profiles matching the filter.
    pub fn visible(&self) -> impl Iterator<Item = usize> + '_ {
        self.profiles
            .iter()
            .enumerate()
            .filter(|(_, p)| self.filter.is_empty() || fuzzy(&self.filter, &p.name))
            .map(|(i, _)| i)
    }

    pub fn selected_profile(&self) -> Option<&ProfileRow> {
        self.visible().nth(self.selected).map(|i| &self.profiles[i])
    }

    fn step(&mut self, down: bool) {
        let n = self.visible().count();
        if n == 0 {
            self.selected = 0;
        } else if down {
            self.selected = (self.selected + 1).min(n - 1);
        } else {
            self.selected = self.selected.saturating_sub(1);
        }
    }
}

#[derive(Debug, Default)]
pub struct SessionList {
    /// Letters type into the filter (after `/`); otherwise they are Vim
    /// keys. The palette always filters.
    pub filtering: bool,
    pub rows: Vec<SessionRow>,
    pub selected: usize,
    pub filter: String,
    pub loading: bool,
    pub error: Option<String>,
}

impl SessionList {
    /// Indices of rows matching the filter.
    pub fn visible(&self) -> impl Iterator<Item = usize> + '_ {
        self.rows
            .iter()
            .enumerate()
            .filter(|(_, r)| fuzzy(&self.filter, &r.title) || fuzzy(&self.filter, &r.session_id))
            .map(|(i, _)| i)
    }

    fn selected_row(&self) -> Option<&SessionRow> {
        self.visible()
            .nth(self.selected)
            .and_then(|i| self.rows.get(i))
    }

    fn step(&mut self, down: bool) {
        let n = self.visible().count();
        if n == 0 {
            self.selected = 0;
            return;
        }
        self.selected = if down {
            (self.selected + 1).min(n - 1)
        } else {
            self.selected.saturating_sub(1)
        };
    }

    fn jump(&mut self, bottom: bool) {
        self.selected = if bottom {
            self.visible().count().saturating_sub(1)
        } else {
            0
        };
    }
}

/// A key that moves a list selection, as arrows, fzf and Vim completion
/// do: Down / Ctrl+N / Ctrl+J is the next row, Up / Ctrl+P / Ctrl+K the
/// previous one. `Some(true)` is down.
fn list_step(action: Action) -> Option<bool> {
    match action {
        Action::Down | Action::NewSession | Action::Newline => Some(true),
        Action::Up | Action::Palette | Action::KillToEnd => Some(false),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Sessions,
    Attached,
}

#[derive(Debug)]
pub struct App {
    pub screen: Screen,
    pub sessions: SessionList,
    pub attached: Option<Attached>,
    /// The palette overlay (Ctrl+P) reuses the session list with this
    /// flag; `sessions.filter` is its query.
    pub palette: bool,
    pub help: bool,
    /// The new-session overlay, while open.
    pub new_session: Option<NewSession>,
    pub inline: bool,
    /// Screen-reader mode (`--ax`): word gutters, no glyph-only state.
    pub ax: bool,
    pub focused: bool,
    pub width: u16,
    pub height: u16,
    pub note: Option<(String, Instant)>,
    /// The attached session's result the user has not seen yet.
    pub unseen: Option<Unseen>,
    /// The orchestrator refused the API key; cleared by the next success.
    pub auth_failed: bool,
    /// Counts attaches, so the program status can tell a new session
    /// from a state change of the same one.
    pub attach_seq: u64,
    /// The terminal takes OSC 7501 reports (set by the runtime).
    pub program_status: bool,
    /// The other sessions, for the terminal.
    pub inbox: Inbox,
    /// A background session-list poll is in flight.
    refreshing: bool,
    /// Session-list loads that failed in a row: the poll backs off.
    poll_failures: u32,
    pub quit: bool,
    ctrl_c_at: Option<Instant>,
    esc_at: Option<Instant>,
    detach_at: Option<Instant>,
    prompt_seq: u64,
    pub cmds: Vec<Cmd>,
}

impl App {
    pub fn new(inline: bool, width: u16, height: u16) -> Self {
        App {
            screen: Screen::Sessions,
            sessions: SessionList {
                loading: true,
                ..Default::default()
            },
            attached: None,
            palette: false,
            help: false,
            new_session: None,
            inline,
            ax: false,
            focused: true,
            width,
            height,
            note: None,
            unseen: None,
            auth_failed: false,
            attach_seq: 0,
            program_status: false,
            inbox: Inbox::default(),
            refreshing: false,
            poll_failures: 0,
            quit: false,
            ctrl_c_at: None,
            esc_at: None,
            detach_at: None,
            prompt_seq: 0,
            cmds: vec![Cmd::LoadSessions],
        }
    }

    /// Poll the session list in the background: it feeds the terminal's
    /// view of the other sessions and keeps the list live. Skipped while a
    /// poll is in flight, and when nothing would show the result.
    pub fn poll_sessions(&mut self) {
        if self.refreshing || !self.wants_poll() {
            return;
        }
        self.refreshing = true;
        self.cmds.push(Cmd::LoadSessions);
    }

    /// How long until the next poll: 5 s while the list or the palette
    /// is on screen, 15 s for the terminal's records alone, doubled per
    /// failed load in a row up to a minute.
    pub fn poll_interval(&self) -> Duration {
        let base = if self.screen == Screen::Sessions || self.palette {
            5
        } else {
            15
        };
        Duration::from_secs((base << self.poll_failures.min(4)).min(60))
    }

    /// Is anything showing the session list: the terminal's records, the
    /// list, or the palette?
    pub fn wants_poll(&self) -> bool {
        self.program_status || self.screen == Screen::Sessions || self.palette
    }

    pub fn note(&mut self, text: impl Into<String>) {
        self.note = Some((text.into(), Instant::now()));
    }

    /// Does the footer need a timer (an elapsed counter or a fading note)?
    pub fn wants_tick(&self) -> bool {
        self.note.is_some()
            || self
                .attached
                .as_ref()
                .is_some_and(|a| a.run_started_at.is_some() || a.reducer.waking)
    }

    fn mint_prompt_id(&mut self) -> String {
        self.prompt_seq += 1;
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        format!("tui-{nanos:x}-{}", self.prompt_seq)
    }

    pub fn attach(&mut self, session_id: &str) {
        if self
            .attached
            .as_ref()
            .is_some_and(|a| a.session_id == session_id)
        {
            self.screen = Screen::Attached;
            self.palette = false;
            return;
        }
        let title = self
            .sessions
            .rows
            .iter()
            .find(|r| r.session_id == session_id)
            .map(|r| r.title.clone())
            .unwrap_or_else(|| session_id.to_string());
        if self.attached.is_some() {
            self.cmds.push(Cmd::Detach);
        }
        let mut a = Attached::new(session_id.to_string(), title);
        a.reducer.fold = !self.inline;
        self.attached = Some(a);
        self.attach_seq += 1;
        self.unseen = None;
        self.inbox.opened(session_id);
        self.screen = Screen::Attached;
        self.palette = false;
        self.cmds.push(Cmd::Attach {
            session_id: session_id.to_string(),
        });
    }

    fn detach(&mut self) {
        if self.attached.take().is_some() {
            self.cmds.push(Cmd::Detach);
        }
        self.attach_seq += 1;
        self.unseen = None;
        self.screen = Screen::Sessions;
        self.sessions.filter.clear();
        self.sessions.loading = true;
        self.cmds.push(Cmd::LoadSessions);
    }

    /// Step to the previous/next session in list order while attached.
    fn neighbour(&mut self, next: bool) {
        let Some(cur) = self.attached.as_ref().map(|a| a.session_id.clone()) else {
            return;
        };
        let rows = &self.sessions.rows;
        let Some(pos) = rows.iter().position(|r| r.session_id == cur) else {
            return;
        };
        let target = if next {
            pos + 1
        } else if pos == 0 {
            return;
        } else {
            pos - 1
        };
        if let Some(r) = rows.get(target) {
            let id = r.session_id.clone();
            self.attach(&id);
        }
    }

    pub fn update(&mut self, msg: Msg) {
        if self.unseen.is_some() && acknowledges(&msg) {
            self.unseen = None;
        }
        match msg {
            Msg::Resize(w, h) => {
                self.width = w;
                self.height = h;
                let (cols, rows) = self.shell_geometry();
                if let Some(a) = &mut self.attached {
                    a.reducer.transcript.invalidate_all();
                    if a.shell.as_mut().is_some_and(|s| s.resize(cols, rows)) {
                        self.cmds.push(Cmd::ShellResize { cols, rows });
                    }
                }
            }
            Msg::Focus(f) => self.focused = f,
            Msg::Copied {
                chars,
                how,
                fallback,
            } => {
                let unit = if chars == 1 { "char" } else { "chars" };
                match (how, fallback) {
                    (Copied::Native, _) => self.note(format!("Copied {chars} {unit} to clipboard")),
                    (Copied::Terminal, _) => {
                        self.note(format!("Sent {chars} {unit} to the terminal's clipboard"))
                    }
                    // The platform command failed: ask the terminal.
                    (Copied::Failed(_), Some(text)) => {
                        self.cmds.push(Cmd::CopyToTerminal(text));
                    }
                    (Copied::Failed(e), None) => self.note(format!("copy failed: {e}")),
                }
            }
            Msg::Tick => {
                if let Some((_, at)) = &self.note {
                    if at.elapsed() > Duration::from_secs(4) {
                        self.note = None;
                    }
                }
            }
            Msg::Paste(text) => {
                if let Some(ns) = &mut self.new_session {
                    match ns.stage {
                        NewStage::Prompt => ns.composer.paste(&text),
                        NewStage::Pick => ns.filter.push_str(text.trim()),
                    }
                } else if self.screen == Screen::Attached && !self.palette {
                    if let Some(a) = &mut self.attached {
                        if let Some(sh) = a.shell.as_ref().filter(|s| s.focused && s.connected) {
                            let mut out = Vec::with_capacity(text.len() + 12);
                            sh.encode_paste(&text, &mut out);
                            self.cmds.push(Cmd::ShellInput(out));
                        } else {
                            a.composer.paste(&text);
                        }
                    }
                } else {
                    self.sessions.filter.push_str(text.trim());
                    self.sessions.selected = 0;
                }
            }
            Msg::Mouse(m) => self.mouse(m),
            Msg::Key(key) => self.key(key),
            Msg::Net(n) => self.net(n),
        }
    }

    fn mouse(&mut self, m: MouseEvent) {
        let Some(a) = &mut self.attached else {
            return;
        };
        if self.screen != Screen::Attached {
            return;
        }
        // The transcript row under the pointer, clamped into the rows
        // the last frame showed (a drag past the edge keeps selecting).
        let hit = |a: &Attached, clamp: bool| -> Option<(usize, u16, u16)> {
            let last = a.hits.len().checked_sub(1)?;
            let row = m.row.checked_sub(a.hits_y).map(usize::from);
            let row = match row {
                Some(r) if r <= last => r,
                Some(_) if clamp => last,
                None if clamp => 0,
                _ => return None,
            };
            let (cell, r) = a.hits[row];
            Some((cell, r, m.column.saturating_sub(a.hits_x)))
        };
        match m.kind {
            MouseEventKind::ScrollUp => Self::scroll(a, 3),
            MouseEventKind::ScrollDown => Self::scroll(a, -3),
            MouseEventKind::Down(MouseButton::Left) => {
                a.selection = hit(a, false).map(|at| Selection {
                    anchor: at,
                    head: at,
                    moved: false,
                });
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if let Some(at) = hit(a, true) {
                    if let Some(sel) = &mut a.selection {
                        sel.moved |= at != sel.head;
                        sel.head = at;
                    }
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                // A drag copies what it selected, and the selection stays
                // on screen until Esc or a click.
                if a.selection.is_some_and(|s| s.moved) {
                    a.copy_request = true;
                    return;
                }
                let Some(sel) = a.selection.take() else {
                    return;
                };
                // A click on a folded turn opens it; on a tool call,
                // toggles its output.
                let i = sel.anchor.0;
                let t = &mut a.reducer.transcript;
                if t.toggle_fold(i) {
                    return;
                }
                if let Some(e) = t.get_mut(i) {
                    if let Cell::Tool { expanded, .. }
                    | Cell::Exec { expanded, .. }
                    | Cell::Diff { expanded, .. } = &mut e.cell
                    {
                        *expanded = !*expanded;
                        e.invalidate();
                    }
                }
            }
            _ => {}
        }
    }

    fn scroll(a: &mut Attached, delta: i32) {
        let next = (a.scroll_up as i64 + delta as i64).max(0) as u32;
        a.scroll_up = next;
        if next == 0 {
            a.unseen = 0;
        }
    }

    fn key(&mut self, key: KeyEvent) {
        if self.help {
            if key.kind != KeyEventKind::Release {
                self.help = false;
            }
            return;
        }
        if self.screen == Screen::Attached
            && !self.palette
            && self
                .attached
                .as_ref()
                .is_some_and(|a| a.shell.as_ref().is_some_and(|s| s.focused))
        {
            self.shell_key(&key);
            return;
        }
        // `?` is help only on an empty draft and outside a search query.
        let draft_empty = self.new_session.is_none()
            && self
                .attached
                .as_ref()
                .is_none_or(|a| a.composer.is_empty() && a.search.is_none());
        // Does the focused composer hold text? Then Ctrl+D/E/N/P edit it,
        // as in Codex; on an empty one they keep the TUI's own meaning.
        let has_text = match &self.new_session {
            Some(ns) => ns.stage == NewStage::Prompt && !ns.composer.is_empty(),
            None => self
                .attached
                .as_ref()
                .is_some_and(|a| !a.composer.is_empty() && a.search.is_none()),
        };
        let Some(action) = resolve(&key, draft_empty, has_text) else {
            return;
        };
        if action != Action::CtrlC {
            self.ctrl_c_at = None;
        }
        if action != Action::Escape {
            self.esc_at = None;
        }
        if action != Action::Detach {
            self.detach_at = None;
        }
        if self.new_session.is_some() {
            self.new_session_key(action);
        } else if self.screen == Screen::Sessions || self.palette {
            self.list_key(action);
        } else if self.attached.as_ref().is_some_and(|a| a.search.is_some()) {
            self.search_key(action);
        } else {
            self.attached_key(action);
        }
    }

    fn search_key(&mut self, action: Action) {
        let width = self.width.saturating_sub(self.gutter_width()).max(1);
        let Some(a) = &mut self.attached else {
            return;
        };
        let Some(s) = &mut a.search else {
            return;
        };
        match action {
            Action::Escape | Action::Search | Action::CtrlC => a.search = None,
            Action::Send | Action::Down => {
                if !a.search_next(width) {
                    self.note("no match");
                }
            }
            Action::Backspace => {
                s.query.pop();
                s.cursor = None;
            }
            Action::KillToStart | Action::KillWord => {
                s.query.clear();
                s.cursor = None;
            }
            Action::ScrollUp | Action::ScrollDown | Action::ScrollTop | Action::ScrollBottom => {
                self.attached_key(action)
            }
            Action::Char(c) if !c.is_control() => {
                s.query.push(c);
                s.cursor = None;
            }
            _ => {}
        }
    }

    fn gutter_width(&self) -> u16 {
        if self.ax {
            crate::render::GUTTER_AX
        } else {
            crate::render::GUTTER
        }
    }

    /// Screen size of the shell pane: the split below the transcript in
    /// fullscreen mode, the whole terminal less a header and the footer
    /// in native scrollback mode (where it is an alternate-screen
    /// overlay). Pure in the terminal size, so the renderer and the
    /// resize path agree.
    pub fn shell_geometry(&self) -> (u16, u16) {
        let cols = self.width.max(1);
        let rows = if self.inline {
            self.height.saturating_sub(2)
        } else {
            shell_pane_rows(self.height.saturating_sub(1)).saturating_sub(1)
        };
        (cols, rows.max(1))
    }

    /// Keys while the shell has focus: Ctrl+` (or Ctrl+Space) hands focus
    /// back; after the shell ended, Enter reopens and Esc closes the pane;
    /// everything else is encoded for the shell.
    fn shell_key(&mut self, key: &KeyEvent) {
        if key.kind == KeyEventKind::Release {
            return;
        }
        let (cols, rows) = self.shell_geometry();
        let Some(a) = &mut self.attached else {
            return;
        };
        let Some(sh) = &mut a.shell else {
            return;
        };
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && matches!(key.code, KeyCode::Char('`' | ' ' | '@')) {
            sh.focused = false;
            return;
        }
        if sh.ended.is_some() {
            match key.code {
                KeyCode::Enter => {
                    sh.reset();
                    self.cmds.push(Cmd::ShellOpen {
                        session_id: a.session_id.clone(),
                        cols,
                        rows,
                    });
                }
                KeyCode::Esc => a.shell = None,
                _ => {}
            }
            return;
        }
        if !sh.connected {
            return;
        }
        let mut out = Vec::with_capacity(8);
        if sh.encode_key(key, &mut out) {
            self.cmds.push(Cmd::ShellInput(out));
        }
    }

    /// Ctrl+` from the composer: open the shell, or focus the open pane.
    fn toggle_shell(&mut self) {
        let (cols, rows) = self.shell_geometry();
        let Some(a) = &mut self.attached else {
            return;
        };
        match &mut a.shell {
            Some(sh) => sh.focused = true,
            None => {
                a.shell = Some(ShellPane::new(cols, rows, self.program_status));
                self.cmds.push(Cmd::ShellOpen {
                    session_id: a.session_id.clone(),
                    cols,
                    rows,
                });
            }
        }
    }

    /// The runtime brings the draft back from `$EDITOR` (None when the
    /// editor failed or was abandoned).
    pub fn draft_edited(&mut self, text: Option<String>) {
        let Some(t) = text else {
            self.note("editor exited without saving; the draft is unchanged");
            return;
        };
        if let Some(a) = &mut self.attached {
            a.composer.set_text(&t);
        }
    }

    /// Run a slash command from the composer.
    fn slash(&mut self, cmd: Slash) {
        match cmd {
            Slash::New => self.open_new_session(),
            Slash::Sessions => self.detach(),
            Slash::Shell => self.toggle_shell(),
            Slash::Help => self.help = true,
            Slash::Quit => {
                self.cmds.push(Cmd::Quit);
                self.quit = true;
            }
            Slash::Interrupt => {
                let Some(a) = &self.attached else {
                    return;
                };
                if a.reducer.is_running() {
                    self.cmds.push(Cmd::Interrupt {
                        session_id: a.session_id.clone(),
                    });
                    self.note("interrupting…");
                } else {
                    self.note("no run to interrupt");
                }
            }
            Slash::Resume => {
                let Some(a) = &self.attached else {
                    return;
                };
                if a.reducer.status.as_deref() == Some("idle") {
                    self.cmds.push(Cmd::Resume {
                        session_id: a.session_id.clone(),
                    });
                    self.note("resuming…");
                } else {
                    self.note("only an idle session resumes; a prompt wakes it too");
                }
            }
            Slash::Plan => {
                let Some(a) = &mut self.attached else {
                    return;
                };
                let next = if a.composer_mode() == "plan" {
                    "default"
                } else {
                    "plan"
                };
                a.mode_override = Some(next.to_string()).filter(|m| *m != a.reducer.mode);
                self.note(if next == "plan" {
                    "plan mode: the next prompt asks for a plan to approve"
                } else {
                    "plan mode off"
                });
            }
            Slash::Transcript => {
                let Some(a) = &self.attached else {
                    return;
                };
                let mut text = String::new();
                a.reducer.transcript.to_text(&mut text);
                self.cmds.push(Cmd::ViewTranscript { text });
            }
            Slash::Dump => {
                if self.inline {
                    self.note("native scrollback mode: the transcript is already in the terminal");
                    return;
                }
                let Some(a) = &self.attached else {
                    return;
                };
                let mut text = String::new();
                a.reducer.transcript.to_text(&mut text);
                self.cmds.push(Cmd::Dump { text });
                self.note("transcript written to the scrollback; leave the TUI to read it");
            }
        }
    }

    /// Ctrl+N or `/new`: open the picker and fetch the profiles.
    pub fn open_new_session(&mut self) {
        if self.new_session.is_some() {
            return;
        }
        self.palette = false;
        self.new_session = Some(NewSession::new());
        self.cmds.push(Cmd::LoadProfiles);
    }

    fn new_session_key(&mut self, action: Action) {
        let width = self.width.saturating_sub(6).max(1);
        let Some(ns) = &mut self.new_session else {
            return;
        };
        match ns.stage {
            NewStage::Pick => match action {
                a if list_step(a).is_some() => ns.step(list_step(a) == Some(true)),
                Action::Send => {
                    if ns.selected_profile().is_some() {
                        ns.stage = NewStage::Prompt;
                    }
                }
                Action::Escape => {
                    if ns.filter.is_empty() {
                        self.new_session = None;
                    } else {
                        ns.filter.clear();
                        ns.selected = 0;
                    }
                }
                Action::CtrlC | Action::Detach => self.new_session = None,
                Action::Backspace => {
                    ns.filter.pop();
                    ns.selected = 0;
                }
                Action::KillToStart | Action::KillWord => {
                    ns.filter.clear();
                    ns.selected = 0;
                }
                Action::Help => self.help = true,
                Action::Char(c) if !c.is_control() => {
                    ns.filter.push(c);
                    ns.selected = 0;
                }
                _ => {}
            },
            NewStage::Prompt => match action {
                Action::Send => {
                    if ns.creating {
                        return;
                    }
                    let Some(profile_id) = ns.selected_profile().map(|p| p.id.clone()) else {
                        ns.stage = NewStage::Pick;
                        return;
                    };
                    if let Some(prompt) = ns.composer.take() {
                        ns.creating = true;
                        ns.error = None;
                        self.cmds.push(Cmd::CreateTask { profile_id, prompt });
                    }
                }
                Action::Escape => {
                    if ns.creating {
                        return;
                    }
                    ns.stage = NewStage::Pick;
                }
                Action::CtrlC => {
                    if !ns.composer.is_empty() {
                        ns.composer.clear();
                    } else {
                        self.new_session = None;
                    }
                }
                Action::Newline => ns.composer.insert_char('\n'),
                Action::Backspace => ns.composer.backspace(),
                Action::Delete => ns.composer.delete(),
                Action::Left => ns.composer.left(),
                Action::Right => ns.composer.right(),
                Action::WordLeft => ns.composer.word_left(),
                Action::WordRight => ns.composer.word_right(),
                Action::Home => ns.composer.home(),
                Action::End => ns.composer.end(),
                Action::KillToEnd => ns.composer.kill_to_end(),
                Action::KillToStart => ns.composer.kill_to_start(),
                Action::KillWord => ns.composer.kill_word(),
                Action::KillWordForward => ns.composer.kill_word_forward(),
                Action::Yank => ns.composer.yank(),
                Action::Up => {
                    ns.composer.move_row(width, false);
                }
                Action::Down => {
                    ns.composer.move_row(width, true);
                }
                Action::Char(c) => ns.composer.insert_char(c),
                _ => {}
            },
        }
    }

    fn list_key(&mut self, action: Action) {
        if let Some(down) = list_step(action) {
            self.sessions.step(down);
            return;
        }
        // The session list outside a filter: Vim keys.
        if !self.palette && !self.sessions.filtering {
            if let Action::Char(c) = action {
                match c {
                    'j' => self.sessions.step(true),
                    'k' => self.sessions.step(false),
                    'g' => self.sessions.jump(false),
                    'G' => self.sessions.jump(true),
                    '/' => self.sessions.filtering = true,
                    'n' => self.open_new_session(),
                    'r' => {
                        self.sessions.loading = true;
                        self.cmds.push(Cmd::LoadSessions);
                    }
                    _ => {}
                }
                return;
            }
        }
        match action {
            Action::Send => {
                self.sessions.filtering = false;
                if let Some(r) = self.sessions.selected_row() {
                    let id = r.session_id.clone();
                    self.attach(&id);
                }
            }
            Action::Escape => {
                if self.palette {
                    self.palette = false;
                    self.sessions.filter.clear();
                } else if self.sessions.filtering {
                    // Back to the Vim keys; the filter stays.
                    self.sessions.filtering = false;
                } else if !self.sessions.filter.is_empty() {
                    self.sessions.filter.clear();
                    self.sessions.selected = 0;
                } else {
                    self.cmds.push(Cmd::Quit);
                    self.quit = true;
                }
            }
            Action::CtrlC | Action::Detach => {
                if self.palette {
                    self.palette = false;
                } else {
                    self.cmds.push(Cmd::Quit);
                    self.quit = true;
                }
            }
            // Backspace on an empty filter leaves it, as from Vim's `/`.
            Action::Backspace
                if !self.palette && self.sessions.filtering && self.sessions.filter.is_empty() =>
            {
                self.sessions.filtering = false;
            }
            Action::Backspace => {
                self.sessions.filter.pop();
                self.sessions.selected = 0;
            }
            Action::KillToStart | Action::KillWord => {
                self.sessions.filter.clear();
                self.sessions.selected = 0;
            }
            Action::Help => self.help = true,
            Action::Char(c) if !c.is_control() => {
                self.sessions.filter.push(c);
                self.sessions.selected = 0;
            }
            Action::Redraw => {}
            _ => {}
        }
    }

    fn attached_key(&mut self, action: Action) {
        let width = self.width.saturating_sub(self.gutter_width()).max(1);
        let Some(a) = &mut self.attached else {
            return;
        };
        // A selection takes Esc (clear) and Ctrl+C (copy again) first.
        if a.selection.is_some() {
            match action {
                Action::Escape => {
                    a.selection = None;
                    return;
                }
                Action::CtrlC => {
                    a.copy_request = true;
                    return;
                }
                _ => {}
            }
        }
        if a.can_send() {
            let mut cmds = Vec::new();
            let consumed = a.decision_key(action, &mut cmds);
            self.cmds.extend(cmds);
            if consumed {
                return;
            }
        }
        // The slash popup is open: list keys move its selection.
        let popup = commands::matches(a.composer.text()).count();
        if popup > 0 {
            if let Some(down) = list_step(action) {
                a.slash_sel = if down {
                    (a.slash_sel + 1) % popup
                } else {
                    (a.slash_sel + popup - 1) % popup
                };
                return;
            }
        }
        if !matches!(action, Action::Send | Action::Char('\t')) {
            a.slash_sel = 0;
        }
        match action {
            Action::Send if commands::prefix(a.composer.text()).is_some() => {
                let draft = a.composer.text().to_string();
                // An exact or unique name, else the highlighted command.
                let cmd = commands::resolve(&draft)
                    .or_else(|| commands::matches(&draft).nth(a.slash_sel).map(|c| c.slash));
                a.slash_sel = 0;
                match cmd {
                    Some(cmd) => {
                        a.composer.clear();
                        self.slash(cmd);
                    }
                    None => self.note("unknown command; type / to list them"),
                }
            }
            Action::Char('\t') if commands::prefix(a.composer.text()).is_some() => {
                let name = commands::matches(a.composer.text())
                    .nth(a.slash_sel)
                    .map(|c| c.name);
                a.slash_sel = 0;
                if let Some(name) = name {
                    let mut t = String::with_capacity(name.len() + 1);
                    t.push('/');
                    t.push_str(name);
                    a.composer.set_text(&t);
                }
            }
            Action::Send => {
                if !a.can_send() {
                    self.note("this session is finished; nothing can be sent to it");
                    return;
                }
                if a.composer.is_empty() {
                    return;
                }
                if let Some(text) = a.composer.take() {
                    let prompt_id = self.mint_prompt_id();
                    let a = self.attached.as_mut().expect("attached");
                    a.reducer.sent(&prompt_id, text.clone());
                    a.scroll_up = 0;
                    a.unseen = 0;
                    let harness_mode = a.mode_override.take().filter(|m| *m != a.reducer.mode);
                    self.cmds.push(Cmd::Send {
                        session_id: a.session_id.clone(),
                        prompt_id,
                        text,
                        harness_mode,
                    });
                }
            }
            Action::Newline => a.composer.insert_char('\n'),
            Action::Escape => {
                let now = Instant::now();
                if a.reducer.is_running() && !a.composer.is_empty() {
                    // A draft in progress: Esc clears it (twice), the run
                    // keeps going; the hint says how to stop it.
                    if self.esc_at.is_some_and(|t| now - t < DOUBLE_TAP) {
                        a.composer.clear();
                        self.esc_at = None;
                    } else {
                        self.esc_at = Some(now);
                        self.note("Esc again clears the draft · Ctrl+C clears · empty draft + Esc interrupts");
                    }
                } else if a.reducer.is_running() {
                    self.cmds.push(Cmd::Interrupt {
                        session_id: a.session_id.clone(),
                    });
                    self.note("interrupting…");
                } else if !a.composer.is_empty() {
                    if self.esc_at.is_some_and(|t| now - t < DOUBLE_TAP) {
                        a.composer.clear();
                        self.esc_at = None;
                    } else {
                        self.esc_at = Some(now);
                        self.note("Esc again clears the draft");
                    }
                } else if a.scroll_up > 0 {
                    a.scroll_up = 0;
                    a.unseen = 0;
                } else if a.shell.is_some() {
                    a.shell = None;
                    self.cmds.push(Cmd::ShellClose);
                }
            }
            Action::Shell => self.toggle_shell(),
            Action::NewSession => self.open_new_session(),
            Action::CtrlC => {
                let now = Instant::now();
                if !a.composer.is_empty() {
                    a.composer.clear();
                    self.ctrl_c_at = Some(now);
                    self.note("draft cleared · Ctrl+C again quits");
                } else if self.ctrl_c_at.is_some_and(|t| now - t < DOUBLE_TAP) {
                    self.cmds.push(Cmd::Quit);
                    self.quit = true;
                } else {
                    self.ctrl_c_at = Some(now);
                    self.note("Ctrl+C again quits");
                }
            }
            Action::Detach => {
                let now = Instant::now();
                if self.detach_at.is_some_and(|t| now - t < DOUBLE_TAP) {
                    self.cmds.push(Cmd::Quit);
                    self.quit = true;
                } else {
                    self.detach_at = Some(now);
                    self.detach();
                }
            }
            Action::Palette => {
                self.palette = true;
                self.sessions.filter.clear();
                self.sessions.selected = 0;
                self.sessions.loading = true;
                self.cmds.push(Cmd::LoadSessions);
            }
            Action::PrevSession => self.neighbour(false),
            Action::NextSession => self.neighbour(true),
            Action::Search => {
                if self.inline {
                    self.note("native scrollback mode: use your terminal's search");
                } else {
                    a.search = Some(Search::default());
                }
            }
            Action::EditDraft => {
                self.cmds.push(Cmd::EditDraft {
                    text: a.composer.text().to_string(),
                });
            }
            Action::ExpandLast => {
                // The newest tool-like cell: a folded turn opens first;
                // an open one toggles the call's output.
                let t = &mut a.reducer.transcript;
                let newest = (t.first_index()..t.len()).rev().find(|&i| {
                    t.get(i).is_some_and(|e| {
                        e.is_fold_head()
                            || matches!(
                                e.cell,
                                Cell::Tool { .. } | Cell::Exec { .. } | Cell::Diff { .. }
                            )
                    })
                });
                if let Some(i) = newest {
                    let closed = t
                        .get(i)
                        .is_some_and(|e| e.hidden_by_fold() || e.is_fold_head());
                    if closed && t.toggle_fold(i) {
                        return;
                    }
                    if let Some(e) = t.get_mut(i) {
                        if let Cell::Tool { expanded, .. }
                        | Cell::Exec { expanded, .. }
                        | Cell::Diff { expanded, .. } = &mut e.cell
                        {
                            *expanded = !*expanded;
                            e.invalidate();
                        }
                    }
                }
            }
            Action::ExpandAll => {
                let t = &mut a.reducer.transcript;
                for i in t.first_index()..t.len() {
                    t.reveal(i);
                }
                let mut any_collapsed = false;
                for e in t.iter_mut() {
                    if let Cell::Tool { expanded, .. }
                    | Cell::Exec { expanded, .. }
                    | Cell::Diff { expanded, .. } = &e.cell
                    {
                        any_collapsed |= !*expanded;
                    }
                }
                for e in t.iter_mut() {
                    if let Cell::Tool { expanded, .. }
                    | Cell::Exec { expanded, .. }
                    | Cell::Diff { expanded, .. } = &mut e.cell
                    {
                        *expanded = any_collapsed;
                        e.invalidate();
                    }
                }
            }
            Action::ScrollUp => Self::scroll(a, a.page_rows.saturating_sub(1) as i32),
            Action::ScrollDown => Self::scroll(a, -(a.page_rows.saturating_sub(1) as i32)),
            Action::ScrollTop => {
                a.scroll_up = u32::MAX / 2;
            }
            Action::ScrollBottom => {
                a.scroll_up = 0;
                a.unseen = 0;
            }
            Action::Help => self.help = true,
            Action::Redraw => {
                a.reducer.transcript.invalidate_all();
            }
            Action::Backspace => a.composer.backspace(),
            Action::Delete => a.composer.delete(),
            Action::Left => a.composer.left(),
            Action::Right => a.composer.right(),
            Action::WordLeft => a.composer.word_left(),
            Action::WordRight => a.composer.word_right(),
            Action::Home => a.composer.home(),
            Action::End => a.composer.end(),
            Action::KillToEnd => a.composer.kill_to_end(),
            Action::KillToStart => a.composer.kill_to_start(),
            Action::KillWord => a.composer.kill_word(),
            Action::KillWordForward => a.composer.kill_word_forward(),
            Action::Yank => a.composer.yank(),
            Action::Up => {
                if !a.composer.move_row(width, false) {
                    // On the first row: pull the newest queued prompt back,
                    // else browse history.
                    if let Some((prompt_id, summary)) = a.reducer.queued.last().cloned() {
                        // The text comes back into the composer now; the
                        // harness drops the queued copy.
                        let text = a.reducer.held_text(&prompt_id).unwrap_or(summary);
                        a.composer.set_text(&text);
                        self.cmds.push(Cmd::Dequeue {
                            session_id: a.session_id.clone(),
                            prompt_id,
                        });
                    } else {
                        a.composer.history_prev();
                    }
                }
            }
            Action::Down => {
                if !a.composer.move_row(width, true) {
                    a.composer.history_next();
                }
            }
            Action::Char(c) => a.composer.insert_char(c),
        }
    }

    fn net(&mut self, n: NetMsg) {
        match n {
            NetMsg::AuthFailed => self.auth_failed = true,
            NetMsg::SessionsRunning { running, known } => {
                self.refreshing = false;
                // Without session state nothing is known to have changed.
                if !known {
                    return;
                }
                for r in &mut self.sessions.rows {
                    r.running = running
                        .iter()
                        .find(|(id, _)| *id == r.session_id)
                        .map(|&(_, run)| run);
                }
                let attached = self.attached.as_ref().map(|a| a.session_id.as_str());
                self.inbox.update(&self.sessions.rows, attached);
            }
            NetMsg::Sessions(res) => {
                self.sessions.loading = false;
                match res {
                    Ok(rows) => {
                        self.auth_failed = false;
                        self.poll_failures = 0;
                        // A refresh reorders the list (newest activity
                        // first): the selection follows its session.
                        let keep = self.sessions.selected_row().map(|r| r.session_id.clone());
                        self.sessions.rows = rows;
                        self.sessions.error = None;
                        let pos = keep.and_then(|id| {
                            self.sessions
                                .visible()
                                .position(|i| self.sessions.rows[i].session_id == id)
                        });
                        if let Some(pos) = pos {
                            self.sessions.selected = pos;
                        }
                        let n = self.sessions.visible().count();
                        if self.sessions.selected >= n {
                            self.sessions.selected = n.saturating_sub(1);
                        }
                        if let Some(a) = &mut self.attached {
                            if let Some(r) = self
                                .sessions
                                .rows
                                .iter()
                                .find(|r| r.session_id == a.session_id)
                            {
                                a.title.clone_from(&r.title);
                            }
                        }
                    }
                    Err(e) => {
                        self.refreshing = false;
                        self.poll_failures = self.poll_failures.saturating_add(1);
                        self.sessions.error = Some(e);
                    }
                }
            }
            NetMsg::Profiles(res) => {
                if let Some(ns) = &mut self.new_session {
                    ns.loading = false;
                    match res {
                        Ok(rows) => {
                            ns.profiles = rows;
                            ns.error = None;
                        }
                        Err(e) => ns.error = Some(e),
                    }
                }
            }
            NetMsg::Created(res) => match res {
                Ok((session_id, title)) => {
                    self.new_session = None;
                    self.attach(&session_id);
                    if let Some(a) = &mut self.attached {
                        a.title = title;
                    }
                    self.sessions.loading = true;
                    self.cmds.push(Cmd::LoadSessions);
                }
                Err(e) => {
                    if let Some(ns) = &mut self.new_session {
                        ns.creating = false;
                        ns.error = Some(e);
                    }
                }
            },
            NetMsg::History {
                session_id,
                events,
                older,
            } => {
                let Some(a) = self
                    .attached
                    .as_mut()
                    .filter(|a| a.session_id == session_id)
                else {
                    return;
                };
                if older {
                    a.loading_older = false;
                    a.want_older = false;
                    if (events.len() as i64) < PAGE {
                        a.exhausted = true;
                    }
                    if events.is_empty() {
                        return;
                    }
                    // Fold the older page on its own and prepend its cells:
                    // the live reducer's bookkeeping shifts, nothing is
                    // re-folded, no raw events are retained.
                    let mut older_r = Reducer::new();
                    older_r.fold = a.reducer.fold;
                    for env in &events {
                        older_r.apply(Event::decode(&env.kind, &env.payload_json));
                    }
                    a.oldest_idx = events.iter().filter_map(|e| e.idx).min().or(a.oldest_idx);
                    a.reducer.prepend(older_r);
                } else {
                    a.loaded = true;
                    a.exhausted = (events.len() as i64) < PAGE;
                    a.oldest_idx = events.iter().filter_map(|e| e.idx).min();
                    for env in &events {
                        a.apply_envelope(env);
                    }
                    if let Some(status) = a.reducer.status.clone() {
                        a.reducer.set_status(&status);
                    }
                }
            }
            NetMsg::Live {
                session_id,
                envelope,
            } => {
                let Some(a) = self
                    .attached
                    .as_mut()
                    .filter(|a| a.session_id == session_id)
                else {
                    return;
                };
                let was_running = a.reducer.is_running();
                let was_failed = a.reducer.status.as_deref().is_some_and(is_failed);
                a.apply_envelope(&envelope);
                if let Some(t) = a.reducer.title.take() {
                    a.title = t;
                }
                let running = a.reducer.is_running();
                if running {
                    // A new run supersedes the last result.
                    self.unseen = None;
                } else if was_running && !a.reducer.awaiting_decision() && !a.reducer.resuming {
                    self.unseen = match a.reducer.last_run {
                        Some(RunOutcome::Failed) => Some(Unseen::RunFailed),
                        Some(RunOutcome::Interrupted) => None,
                        _ => Some(Unseen::Done),
                    };
                }
                if !was_failed && a.reducer.status.as_deref().is_some_and(is_failed) {
                    self.unseen = Some(Unseen::SessionFailed);
                }
            }
            NetMsg::Status {
                session_id,
                status,
                title,
            } => {
                let Some(a) = self
                    .attached
                    .as_mut()
                    .filter(|a| a.session_id == session_id)
                else {
                    return;
                };
                a.reducer.set_status(&status);
                if let Some(t) = title.filter(|t| !t.is_empty()) {
                    a.title = t;
                }
                if let Some(r) = self
                    .sessions
                    .rows
                    .iter_mut()
                    .find(|r| r.session_id == session_id)
                {
                    r.status = status;
                }
            }
            NetMsg::Reconnecting {
                session_id,
                attempt,
            } => {
                if let Some(a) = self
                    .attached
                    .as_mut()
                    .filter(|a| a.session_id == session_id)
                {
                    a.reconnecting = Some(attempt);
                }
            }
            NetMsg::Connected { session_id } => {
                if let Some(a) = self
                    .attached
                    .as_mut()
                    .filter(|a| a.session_id == session_id)
                {
                    a.reconnecting = None;
                    a.error = None;
                    self.auth_failed = false;
                }
            }
            NetMsg::Failed { session_id, error } => {
                if let Some(a) = self
                    .attached
                    .as_mut()
                    .filter(|a| a.session_id == session_id)
                {
                    a.error = Some(error);
                }
            }
            NetMsg::OlderFailed { session_id, error } => {
                if let Some(a) = self
                    .attached
                    .as_mut()
                    .filter(|a| a.session_id == session_id)
                {
                    a.loading_older = false;
                    a.error = Some(error);
                }
            }
            NetMsg::SendFailed {
                session_id,
                prompt_id,
                error,
            } => {
                if let Some(a) = self
                    .attached
                    .as_mut()
                    .filter(|a| a.session_id == session_id)
                {
                    a.reducer.send_failed(&prompt_id);
                }
                self.note(format!("send failed: {error}"));
            }
            NetMsg::CompleteFailed {
                session_id,
                tool_call_id,
                error,
            } => {
                if let Some(a) = self
                    .attached
                    .as_mut()
                    .filter(|a| a.session_id == session_id)
                {
                    a.reducer.decision_failed(&tool_call_id);
                }
                self.note(format!("answer failed: {error}"));
            }
            NetMsg::ShellOpened { session_id } => {
                if let Some(sh) = self.shell_of(&session_id) {
                    sh.connected = true;
                    sh.ended = None;
                }
            }
            NetMsg::ShellOutput { session_id, bytes } => {
                let reply = self.shell_of(&session_id).and_then(|sh| sh.feed(&bytes));
                if let Some(reply) = reply {
                    self.cmds.push(Cmd::ShellInput(reply));
                }
            }
            NetMsg::ShellTitle { session_id, title } => {
                if let Some(sh) = self.shell_of(&session_id) {
                    sh.title = title;
                }
            }
            NetMsg::ShellEnded { session_id, error } => {
                if let Some(sh) = self.shell_of(&session_id) {
                    sh.end(error.unwrap_or_else(|| "the shell exited".to_string()));
                }
            }
        }
    }

    fn shell_of(&mut self, session_id: &str) -> Option<&mut ShellPane> {
        self.attached
            .as_mut()
            .filter(|a| a.session_id == session_id)
            .and_then(|a| a.shell.as_mut())
    }

    /// The renderer reports that the scroll-up walk ran out of cells:
    /// page older history in once per gap.
    pub fn request_older(&mut self) {
        let Some(a) = &mut self.attached else {
            return;
        };
        if a.exhausted || a.loading_older || !a.loaded || self.inline {
            return;
        }
        let Some(before) = a.oldest_idx else {
            return;
        };
        a.loading_older = true;
        self.cmds.push(Cmd::LoadOlder {
            session_id: a.session_id.clone(),
            before_idx: before,
        });
    }
}

/// A session status that means the session stopped on a failure.
pub fn is_failed(status: &str) -> bool {
    matches!(status, "failed" | "dead" | "host_lost")
}

/// Input that shows the user is at the terminal: a key press, a paste, a
/// click or a scroll. Mouse movement and focus are not enough.
fn acknowledges(msg: &Msg) -> bool {
    match msg {
        Msg::Key(k) => k.kind != KeyEventKind::Release,
        Msg::Paste(_) => true,
        Msg::Mouse(m) => matches!(
            m.kind,
            MouseEventKind::Down(_) | MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
        ),
        _ => false,
    }
}
