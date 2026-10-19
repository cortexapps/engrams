//! The application state and its pure update step. The runtime (`lib.rs`)
//! feeds it messages and executes the commands it emits; the renderer
//! (`render.rs`) draws it. Nothing here touches the terminal or the
//! network, so tests drive it directly.

use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind};

use crate::composer::Composer;
use crate::event::{Envelope, Event};
use crate::keymap::{resolve, Action};
use crate::reducer::{is_terminal, Reducer};
use crate::sessions::{fuzzy, SessionRow};
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
    Quit,
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
}

#[derive(Debug)]
pub enum NetMsg {
    Sessions(Result<Vec<SessionRow>, String>),
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
    pub search: Option<Search>,
    /// The composer collects feedback for a plan rejection (Tab).
    pub feedback: bool,
    /// The shell pane (Ctrl+`), open until closed or the shell exits.
    pub shell: Option<ShellPane>,
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
            search: None,
            feedback: false,
            shell: None,
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

    pub fn can_send(&self) -> bool {
        !self.reducer.status.as_deref().is_some_and(is_terminal)
    }
}

#[derive(Debug, Default)]
pub struct SessionList {
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
    pub inline: bool,
    /// Screen-reader mode (`--ax`): word gutters, no glyph-only state.
    pub ax: bool,
    pub focused: bool,
    pub width: u16,
    pub height: u16,
    pub note: Option<(String, Instant)>,
    /// Desktop notifications the runtime emits (bell + OSC) when the
    /// terminal is unfocused.
    pub alerts: Vec<String>,
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
            inline,
            ax: false,
            focused: true,
            width,
            height,
            note: None,
            alerts: Vec::new(),
            quit: false,
            ctrl_c_at: None,
            esc_at: None,
            detach_at: None,
            prompt_seq: 0,
            cmds: vec![Cmd::LoadSessions],
        }
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
        self.attached = Some(Attached::new(session_id.to_string(), title));
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
            Msg::Tick => {
                if let Some((_, at)) = &self.note {
                    if at.elapsed() > Duration::from_secs(4) {
                        self.note = None;
                    }
                }
            }
            Msg::Paste(text) => {
                if self.screen == Screen::Attached && !self.palette {
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
        match m.kind {
            MouseEventKind::ScrollUp => Self::scroll(a, 3),
            MouseEventKind::ScrollDown => Self::scroll(a, -3),
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
        let draft_empty = self
            .attached
            .as_ref()
            .is_none_or(|a| a.composer.is_empty() && a.search.is_none());
        let Some(action) = resolve(&key, draft_empty) else {
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
        if self.screen == Screen::Sessions || self.palette {
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
                a.shell = Some(ShellPane::new(cols, rows));
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

    fn list_key(&mut self, action: Action) {
        match action {
            Action::Up => self.sessions.step(false),
            Action::Down => self.sessions.step(true),
            Action::Send => {
                if let Some(r) = self.sessions.selected_row() {
                    let id = r.session_id.clone();
                    self.attach(&id);
                }
            }
            Action::Escape => {
                if self.palette {
                    self.palette = false;
                    self.sessions.filter.clear();
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
            Action::Palette => {
                if self.palette {
                    self.palette = false;
                } else {
                    self.sessions.loading = true;
                    self.cmds.push(Cmd::LoadSessions);
                }
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
        if a.can_send() {
            let mut cmds = Vec::new();
            let consumed = a.decision_key(action, &mut cmds);
            self.cmds.extend(cmds);
            if consumed {
                return;
            }
        }
        match action {
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
                    self.cmds.push(Cmd::Send {
                        session_id: a.session_id.clone(),
                        prompt_id,
                        text,
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
                let t = &mut a.reducer.transcript;
                for e in t.iter_mut().rev() {
                    match &mut e.cell {
                        Cell::Tool { expanded, .. }
                        | Cell::Exec { expanded, .. }
                        | Cell::Diff { expanded, .. } => {
                            *expanded = !*expanded;
                            e.invalidate();
                            break;
                        }
                        _ => {}
                    }
                }
            }
            Action::ExpandAll => {
                let t = &mut a.reducer.transcript;
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
            Action::Yank => a.composer.yank(),
            Action::Up => {
                if !a.composer.move_row(width, false) {
                    // On the first row: pull the newest queued prompt back,
                    // else browse history.
                    if let Some((prompt_id, _)) = a.reducer.queued.last().cloned() {
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
            NetMsg::Sessions(res) => {
                self.sessions.loading = false;
                match res {
                    Ok(rows) => {
                        self.sessions.rows = rows;
                        self.sessions.error = None;
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
                    Err(e) => self.sessions.error = Some(e),
                }
            }
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
                let was_awaiting = a.reducer.awaiting_decision();
                let was_running = a.reducer.is_running();
                a.apply_envelope(&envelope);
                if let Some(t) = a.reducer.title.take() {
                    a.title = t;
                }
                let awaiting = a.reducer.awaiting_decision();
                let running = a.reducer.is_running();
                if !self.focused {
                    if awaiting && !was_awaiting {
                        self.alerts.push(format!("{}: needs you", a.title));
                    } else if was_running && !running && !awaiting {
                        self.alerts.push(format!("{}: done", a.title));
                    }
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
                if let Some(sh) = self.shell_of(&session_id) {
                    sh.feed(&bytes);
                }
            }
            NetMsg::ShellTitle { session_id, title } => {
                if let Some(sh) = self.shell_of(&session_id) {
                    sh.title = title;
                }
            }
            NetMsg::ShellEnded { session_id, error } => {
                if let Some(sh) = self.shell_of(&session_id) {
                    sh.connected = false;
                    sh.ended = Some(error.unwrap_or_else(|| "the shell exited".to_string()));
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
