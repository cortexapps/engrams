//! Folds session events into transcript cells (ADR 0124 D2). The rules are
//! the web transcript's (`web/src/components/session-thread/
//! buildMessages.ts`), applied incrementally: each event mutates the cell
//! list in place, and the inverted-order cases the web pre-scans for are
//! handled by remembering what is still awaited.

use std::collections::{HashMap, HashSet};

use crate::event::{
    parse_answers, parse_plan, parse_plan_decision, parse_questions, Answers, Event,
};
use crate::markdown::Markdown;
use crate::tools::{canonical as canonical_tool, summarize};
use crate::transcript::{Cell, Marker, ToolState, Transcript};

/// Session states in which no run can be in flight.
pub fn is_inactive(status: &str) -> bool {
    matches!(
        status,
        "parked" | "unreachable" | "idle" | "completed" | "failed" | "dead"
    )
}

/// Session states nothing can be sent to.
pub fn is_terminal(status: &str) -> bool {
    matches!(status, "completed" | "failed" | "dead" | "host_lost")
}

#[derive(Debug, Default)]
pub struct Reducer {
    pub transcript: Transcript,
    /// Echo text of a `prompt_id` user message, held until its consuming
    /// `run_started` or `prompt_steered`.
    held: HashMap<String, String>,
    /// A consumed prompt whose echo has not arrived (the inverted order):
    /// prompt_id → the pending user cell.
    awaiting_echo: HashMap<String, usize>,
    /// The harness-owned queue, oldest first: (prompt_id, summary).
    pub queued: Vec<(String, String)>,
    pub run_open: bool,
    harness_idle: bool,
    /// The assistant cell still accepting text.
    active: Option<usize>,
    /// Durable text length of the active cell; chunks append past it.
    stream_base: usize,
    /// The cell a chunk stream is landing in, by message id.
    stream: Option<(String, usize)>,
    open_tools: HashMap<String, usize>,
    open_execs: HashMap<String, usize>,
    /// Deferred decisions (plan, question) by tool_call_id → cell.
    decisions: HashMap<String, usize>,
    pending_decisions: HashSet<String>,
    requested: HashSet<String>,
    plan_attempts: HashSet<String>,
    /// Prompts this client sent, rendered optimistically at send time:
    /// prompt_id → cell. Consumed by `run_started` / `prompt_steered`.
    sent: HashMap<String, usize>,
    plan_handoff: bool,
    run_start_len: usize,
    pub mode: String,
    pub status: Option<String>,
    pub waking: bool,
    pub title: Option<String>,
}

impl Reducer {
    pub fn new() -> Self {
        Reducer {
            mode: "default".into(),
            ..Default::default()
        }
    }

    /// Is the agent working (the composer's "send vs queue" and the
    /// spinner)? Mirrors the web's `isRunning`.
    pub fn is_running(&self) -> bool {
        let inactive = self.status.as_deref().is_some_and(is_inactive);
        if inactive || !self.pending_decisions.is_empty() {
            return false;
        }
        self.run_open || (self.tail_awaiting() && !self.harness_idle)
    }

    /// A decision (plan approval, question) is waiting on the user.
    pub fn awaiting_decision(&self) -> bool {
        !self.pending_decisions.is_empty()
    }

    /// The oldest pending decision: its cell and tool_call_id.
    pub fn pending_decision(&self) -> Option<(usize, String)> {
        self.pending_decisions
            .iter()
            .filter_map(|id| self.decisions.get(id).map(|&i| (i, id.clone())))
            .min()
    }

    /// The user answered (optimistically, like the web): the cell locks
    /// into its receipt and the run counts as running again. The
    /// authoritative `tool_result_submitted` lands later.
    pub fn decide(&mut self, tool_call_id: &str, approved: Option<bool>) {
        if !self.pending_decisions.remove(tool_call_id) {
            return;
        }
        if let Some(e) = self
            .decisions
            .get(tool_call_id)
            .and_then(|&i| self.transcript.get_mut(i))
        {
            match &mut e.cell {
                Cell::Question { answered, .. } => *answered = true,
                Cell::Plan { approved: a, .. } => *a = approved,
                _ => {}
            }
            e.invalidate();
        }
    }

    /// The RPC failed: the decision is pending again.
    pub fn decision_failed(&mut self, tool_call_id: &str) {
        let Some(&i) = self.decisions.get(tool_call_id) else {
            return;
        };
        let Some(e) = self.transcript.get_mut(i) else {
            return;
        };
        match &mut e.cell {
            Cell::Question { answered, .. } if *answered => *answered = false,
            Cell::Plan { approved, .. } if approved.is_some() => *approved = None,
            _ => return,
        }
        e.invalidate();
        self.pending_decisions.insert(tool_call_id.to_string());
    }

    fn tail_awaiting(&self) -> bool {
        let Some(last) = self.transcript.last_index() else {
            return false;
        };
        let mut i = last;
        loop {
            let Some(e) = self.transcript.get(i) else {
                return false;
            };
            match &e.cell {
                Cell::Marker { kind, .. } => {
                    if !matches!(kind, Marker::Resumed | Marker::Recovered | Marker::Rollback) {
                        return false;
                    }
                }
                Cell::Assistant {
                    open, streaming, ..
                } => return *open || *streaming,
                Cell::User { .. } => return true,
                _ => return false,
            }
            if i == 0 {
                return false;
            }
            i -= 1;
        }
    }

    fn close_active(&mut self) {
        if let Some(i) = self.active.take() {
            if let Some(e) = self.transcript.get_mut(i) {
                if let Cell::Assistant {
                    md,
                    open,
                    streaming,
                    ..
                } = &mut e.cell
                {
                    *open = false;
                    if !*streaming {
                        md.finalize();
                        e.invalidate();
                    }
                }
            }
        }
        self.stream_base = 0;
    }

    fn ensure_assistant(&mut self) -> usize {
        if let Some(i) = self.active {
            if self.transcript.get(i).is_some() {
                return i;
            }
        }
        let i = self.transcript.push(Cell::Assistant {
            md: Markdown::default(),
            streaming: false,
            open: true,
            failed: false,
        });
        self.active = Some(i);
        self.stream_base = 0;
        i
    }

    /// A question card. A request the harness re-fires (the same
    /// tool_call_id after a resume) replaces its earlier card, keeping
    /// the user's picks, so one card sits by the composer.
    fn push_question(&mut self, tool_call_id: String, questions: Vec<crate::event::Question>) {
        let mut picks = vec![Vec::new(); questions.len()];
        let mut current = 0;
        let mut answered = false;
        if let Some(old) = self.decisions.get(&tool_call_id).copied() {
            if let Some(e) = self.transcript.get_mut(old) {
                if let Cell::Question {
                    picks: p,
                    current: c,
                    answered: a,
                    ..
                } = &mut e.cell
                {
                    if p.len() == picks.len() {
                        picks = std::mem::take(p);
                    }
                    current = *c;
                    answered = *a;
                }
                e.hide();
            }
        }
        self.close_active();
        let i = self.transcript.push(Cell::Question {
            tool_call_id: tool_call_id.as_str().into(),
            questions,
            answered,
            picks,
            current,
        });
        self.decisions.insert(tool_call_id.clone(), i);
        if !answered {
            self.pending_decisions.insert(tool_call_id);
        }
    }

    /// Lock a question card with the answers the harness recorded.
    fn answer_question(&mut self, tool_call_id: &str, answers: Option<Answers>) {
        self.pending_decisions.remove(tool_call_id);
        let Some(&i) = self.decisions.get(tool_call_id) else {
            return;
        };
        let Some(e) = self.transcript.get_mut(i) else {
            return;
        };
        if let Cell::Question {
            answered,
            questions,
            picks,
            ..
        } = &mut e.cell
        {
            *answered = true;
            if let Some(answers) = answers {
                for (q, p) in questions.iter().zip(picks.iter_mut()) {
                    let Some(labels) = answers.get(&q.question) else {
                        continue;
                    };
                    p.clear();
                    for (j, o) in q.options.iter().enumerate() {
                        if labels.iter().any(|l| *l == o.label) {
                            p.push(j as u16);
                        }
                    }
                }
            }
        }
        e.invalidate();
    }

    fn push_marker(&mut self, kind: Marker, text: &str) {
        self.close_active();
        self.transcript.push(Cell::Marker {
            kind,
            text: text.into(),
        });
    }

    fn push_user(&mut self, prompt_id: Option<&str>, text: String, steered: bool) {
        self.close_active();
        let pending = text.is_empty();
        let i = self.transcript.push(Cell::User {
            text,
            pending,
            steered,
        });
        if let (true, Some(pid)) = (pending, prompt_id) {
            self.awaiting_echo.insert(pid.to_string(), i);
        }
    }

    fn take_queued(&mut self, prompt_id: &str) -> Option<String> {
        let pos = self.queued.iter().position(|(id, _)| id == prompt_id)?;
        Some(self.queued.remove(pos).1)
    }

    fn settle_run(&mut self, ok: bool) {
        let start = self.run_start_len.max(self.transcript.first_index());
        let end = self.transcript.len();
        for i in start..end {
            if let Some(e) = self.transcript.get_mut(i) {
                match &mut e.cell {
                    Cell::Assistant {
                        md,
                        open,
                        streaming,
                        failed,
                    } => {
                        if *open || *streaming {
                            *open = false;
                            *streaming = false;
                            *failed = !ok;
                            md.finalize();
                            e.invalidate();
                        }
                    }
                    Cell::Tool { state, .. } if *state == ToolState::Running => {
                        *state = if ok { ToolState::Ok } else { ToolState::Err };
                        e.invalidate();
                    }
                    Cell::Exec { done, .. } if !*done => {
                        *done = true;
                        e.invalidate();
                    }
                    _ => {}
                }
            }
        }
        self.open_tools.clear();
        self.open_execs.clear();
        self.stream = None;
        self.active = None;
        self.stream_base = 0;
        self.run_open = false;
        self.plan_handoff = false;
    }

    /// Record a prompt this client just sent (ADR 0108 optimistic echo).
    /// The cell stays pending until the harness consumes it, so native
    /// scrollback mode never commits it early.
    pub fn sent(&mut self, prompt_id: &str, text: String) {
        self.close_active();
        self.harness_idle = false;
        let i = self.transcript.push(Cell::User {
            text,
            pending: true,
            steered: false,
        });
        self.sent.insert(prompt_id.to_string(), i);
    }

    /// Mark an optimistic cell consumed at its send position.
    fn consume_sent(&mut self, prompt_id: &str, steered: bool) -> bool {
        let Some(i) = self.sent.remove(prompt_id) else {
            return false;
        };
        if let Some(e) = self.transcript.get_mut(i) {
            if let Cell::User {
                pending,
                steered: s,
                ..
            } = &mut e.cell
            {
                *pending = false;
                *s = steered;
            }
            e.invalidate();
        }
        true
    }

    /// A send failed: the optimistic cell leaves the thread.
    pub fn send_failed(&mut self, prompt_id: &str) {
        if let Some(i) = self.sent.remove(prompt_id) {
            if let Some(e) = self.transcript.get_mut(i) {
                e.hide();
            }
        }
    }

    /// Prepend an older page folded on its own (fullscreen history
    /// paging). This reducer's bookkeeping shifts by the prepended count;
    /// the older page's open state is dropped: a run that straddles the
    /// page edge was already settled by the newer fold or the session
    /// status, and an unloaded decision is unknown, not pending.
    pub fn prepend(&mut self, older: Reducer) {
        let k = self.transcript.prepend(older.transcript);
        if k == 0 {
            return;
        }
        for v in self
            .awaiting_echo
            .values_mut()
            .chain(self.open_tools.values_mut())
            .chain(self.open_execs.values_mut())
            .chain(self.decisions.values_mut())
            .chain(self.sent.values_mut())
        {
            *v += k;
        }
        if let Some(a) = &mut self.active {
            *a += k;
        }
        if let Some((_, i)) = &mut self.stream {
            *i += k;
        }
        self.run_start_len += k;
        for (id, i) in older.decisions {
            self.decisions.entry(id).or_insert(i);
        }
        self.requested.extend(older.requested);
        self.plan_attempts.extend(older.plan_attempts);
    }

    /// Apply the session's authoritative status (from `GetSession` or a
    /// `status_changed` event). An inactive session has no live run.
    pub fn set_status(&mut self, status: &str) {
        self.status = Some(status.to_string());
        if is_inactive(status) && self.run_open {
            self.settle_run(false);
        }
    }

    pub fn apply(&mut self, ev: Event) {
        match ev {
            Event::RunStarted {
                prompt_id,
                prompt_summary,
            } => {
                self.waking = false;
                self.run_open = true;
                self.harness_idle = false;
                self.plan_handoff = false;
                self.close_active();
                self.run_start_len = self.transcript.len();
                if let Some(pid) = prompt_id {
                    let queued = self.take_queued(&pid);
                    if self.consume_sent(&pid, false) {
                        return;
                    }
                    let text = self
                        .held
                        .remove(&pid)
                        .or(queued)
                        .or(prompt_summary)
                        .unwrap_or_default();
                    self.push_user(Some(&pid), text, false);
                } else if let Some(summary) = prompt_summary {
                    self.push_user(None, summary, false);
                }
            }
            Event::AgentMessage {
                role,
                text,
                prompt_id,
                message_id,
            } => match role.as_str() {
                "user" => {
                    self.close_active();
                    self.harness_idle = false;
                    match prompt_id {
                        Some(pid) if self.sent.contains_key(&pid) => {}
                        Some(pid) => {
                            if let Some(i) = self.awaiting_echo.remove(&pid) {
                                if let Some(e) = self.transcript.get_mut(i) {
                                    if let Cell::User {
                                        text: t, pending, ..
                                    } = &mut e.cell
                                    {
                                        *t = text;
                                        *pending = false;
                                        e.invalidate();
                                    }
                                }
                            } else {
                                self.held.insert(pid, text);
                            }
                        }
                        None => self.push_user(None, text, false),
                    }
                }
                "system" => self.push_marker(Marker::Note, &text),
                _ => {
                    // The durable message supersedes its chunk stream.
                    let streamed = match &self.stream {
                        Some((mid, i)) if *mid == message_id => Some(*i),
                        _ => None,
                    };
                    let i = match streamed {
                        Some(i) if self.active == Some(i) => i,
                        Some(i) => {
                            // The stream's cell was closed by a boundary;
                            // the final text still belongs there.
                            if let Some(e) = self.transcript.get_mut(i) {
                                if let Cell::Assistant { md, streaming, .. } = &mut e.cell {
                                    md.replace_from(self.stream_base, &text);
                                    *streaming = false;
                                    // Closed and no longer streaming: final.
                                    md.finalize();
                                    e.invalidate();
                                }
                            }
                            self.stream = None;
                            return;
                        }
                        None => self.ensure_assistant(),
                    };
                    let mut base = self.stream_base;
                    if let Some(e) = self.transcript.get_mut(i) {
                        if let Cell::Assistant { md, streaming, .. } = &mut e.cell {
                            md.replace_from(base, &text);
                            // Durable text is complete: render its tail now.
                            md.commit_all();
                            *streaming = false;
                            base = md.raw().len();
                        }
                        e.invalidate();
                    }
                    self.stream_base = base;
                    self.stream = None;
                }
            },
            Event::Chunk { message_id, chunk } => {
                if !self.run_open {
                    return;
                }
                let i = match &self.stream {
                    Some((mid, i)) if *mid == message_id && self.active == Some(*i) => *i,
                    _ => {
                        let i = self.ensure_assistant();
                        if let Some(e) = self.transcript.get_mut(i) {
                            if let Cell::Assistant { md, .. } = &mut e.cell {
                                md.replace_from(self.stream_base, "");
                            }
                            e.invalidate();
                        }
                        self.stream = Some((message_id, i));
                        i
                    }
                };
                if let Some(e) = self.transcript.get_mut(i) {
                    let mut from = None;
                    if let Cell::Assistant { md, streaming, .. } = &mut e.cell {
                        from = Some(md.push(&chunk));
                        *streaming = true;
                    }
                    if let Some(from) = from {
                        e.relayout_from(from);
                    }
                }
            }
            Event::ToolCallStarted {
                tool_call_id,
                tool_name,
                args_summary,
            } => {
                if canonical_tool(&tool_name) == "exit_plan_mode"
                    && !self.requested.contains(&tool_call_id)
                {
                    self.plan_attempts.insert(tool_call_id);
                    if self.mode != "plan" {
                        self.push_marker(
                            Marker::PlanAttempt,
                            "the agent drafted a plan outside plan mode",
                        );
                    }
                    return;
                }
                if self.decisions.contains_key(&tool_call_id) {
                    return;
                }
                self.close_active();
                let i = self.transcript.push(Cell::Tool {
                    args: summarize(&tool_name, &args_summary).into(),
                    name: tool_name.into(),
                    result: None,
                    state: ToolState::Running,
                    duration_ms: None,
                    expanded: false,
                });
                self.open_tools.insert(tool_call_id, i);
            }
            Event::ToolCallRequested {
                tool_call_id,
                name,
                args_json,
            } => {
                self.requested.insert(tool_call_id.clone());
                // A deferred question or plan is represented by its card.
                // The harness emits a `tool_call_started` for the same id
                // first (and a completion once it is answered): the tool
                // row would sit beside the card, so it is withdrawn.
                if name == "exit_plan_mode" || name == "ask_user_question" {
                    if let Some(i) = self.open_tools.remove(&tool_call_id) {
                        if let Some(e) = self.transcript.get_mut(i) {
                            e.hide();
                        }
                    }
                }
                if name == "exit_plan_mode" {
                    if let Some(plan) = parse_plan(&args_json) {
                        self.close_active();
                        let i = self.transcript.push(Cell::Plan {
                            tool_call_id: tool_call_id.as_str().into(),
                            md: Markdown::finalized(&plan),
                            approved: None,
                            choice: None,
                        });
                        self.decisions.insert(tool_call_id.clone(), i);
                        self.pending_decisions.insert(tool_call_id);
                    }
                    return;
                }
                if name == "ask_user_question" {
                    if let Some(questions) = parse_questions(&args_json) {
                        self.push_question(tool_call_id, questions);
                    }
                    return;
                }
                if self.open_tools.contains_key(&tool_call_id) {
                    return;
                }
                self.close_active();
                let i = self.transcript.push(Cell::Tool {
                    args: summarize(&name, &args_json).into(),
                    name: name.into(),
                    result: None,
                    state: ToolState::Running,
                    duration_ms: None,
                    expanded: false,
                });
                self.open_tools.insert(tool_call_id, i);
            }
            Event::ToolResultSubmitted {
                tool_call_id,
                result_json,
            } => {
                self.pending_decisions.remove(&tool_call_id);
                let Some(&i) = self.decisions.get(&tool_call_id) else {
                    return;
                };
                let Some(e) = self.transcript.get_mut(i) else {
                    return;
                };
                match &mut e.cell {
                    Cell::Plan { approved, .. } => {
                        *approved = parse_plan_decision(&result_json);
                        if *approved == Some(true) {
                            self.mode = "default".into();
                        }
                        self.plan_handoff = true;
                        e.invalidate();
                    }
                    Cell::Question { .. } => {
                        self.answer_question(&tool_call_id, parse_answers(&result_json));
                    }
                    _ => {}
                }
            }
            Event::ToolCallCompleted {
                tool_call_id,
                tool_name,
                ok,
                duration_ms,
                result_summary,
            } => {
                if self.decisions.contains_key(&tool_call_id)
                    || self.plan_attempts.contains(&tool_call_id)
                {
                    return;
                }
                let state = if ok { ToolState::Ok } else { ToolState::Err };
                match self.open_tools.remove(&tool_call_id) {
                    Some(i) => {
                        if let Some(e) = self.transcript.get_mut(i) {
                            if let Cell::Tool {
                                result,
                                state: s,
                                duration_ms: d,
                                ..
                            } = &mut e.cell
                            {
                                *result = result_summary.map(Into::into);
                                *s = state;
                                *d = duration_ms;
                            }
                            e.invalidate();
                        }
                    }
                    None => {
                        self.close_active();
                        self.transcript.push(Cell::Tool {
                            name: tool_name.into(),
                            args: "".into(),
                            result: result_summary.map(Into::into),
                            state,
                            duration_ms,
                            expanded: false,
                        });
                    }
                }
            }
            Event::FileChanged {
                tool_call_id,
                path,
                text,
                added,
                removed,
            } => {
                // ADR 0054 Flavor A: the diff replaces the generic tool
                // card of the write/edit call that produced it. A failed
                // edit emits no `file_changed` and keeps its error card.
                let cell = Cell::Diff {
                    path: path.into(),
                    text: text.into(),
                    added,
                    removed,
                    expanded: false,
                };
                // The entry stays in `open_tools` so the completion that
                // follows finds the card (and leaves a diff cell alone).
                match self.open_tools.get(&tool_call_id).copied() {
                    Some(i) if self.transcript.get(i).is_some() => {
                        let e = self.transcript.get_mut(i).expect("checked");
                        e.cell = cell;
                        e.invalidate();
                    }
                    _ => {
                        self.close_active();
                        self.transcript.push(cell);
                    }
                }
            }
            Event::ExecStarted { exec_id, command } => {
                self.close_active();
                let i = self.transcript.push(Cell::Exec {
                    command: command.join(" ").into(),
                    output: String::new(),
                    exit: None,
                    done: false,
                    expanded: false,
                });
                self.open_execs.insert(exec_id, i);
            }
            Event::ExecOutput { exec_id, chunk } => {
                let i = self.open_exec(&exec_id);
                if let Some(e) = self.transcript.get_mut(i) {
                    if let Cell::Exec { output, .. } = &mut e.cell {
                        output.push_str(&chunk);
                    }
                    e.invalidate();
                }
            }
            Event::ExecCompleted {
                exec_id,
                exit_status,
                ..
            } => {
                let i = self.open_exec(&exec_id);
                self.open_execs.remove(&exec_id);
                if let Some(e) = self.transcript.get_mut(i) {
                    if let Cell::Exec { exit, done, .. } = &mut e.cell {
                        *exit = exit_status;
                        *done = true;
                    }
                    e.invalidate();
                }
            }
            Event::RunEnded { ok, interrupted } => {
                // A plan decision ends the read-only turn by design: that
                // interrupt is a handoff, not a failure.
                let was_interrupt = interrupted;
                let interrupted = interrupted && !self.plan_handoff;
                let ok = if interrupted {
                    false
                } else {
                    was_interrupt || ok
                };
                self.settle_run(ok);
            }
            Event::HarnessIdle => {
                self.close_active();
                self.harness_idle = true;
            }
            Event::HarnessModeChanged { mode } => {
                self.mode = mode;
                let text = format!("mode: {}", self.mode);
                self.push_marker(Marker::Mode, &text);
            }
            Event::PromptQueued { prompt_id, summary } => {
                // An optimistic cell for a prompt the harness queued leaves
                // the thread: the queue rail owns it until its run starts.
                if let Some(i) = self.sent.remove(&prompt_id) {
                    if let Some(e) = self.transcript.get_mut(i) {
                        if let Cell::User { text, .. } = &mut e.cell {
                            self.held.insert(prompt_id.clone(), std::mem::take(text));
                        }
                        e.hide();
                    }
                }
                self.queued.push((prompt_id, summary));
            }
            Event::PromptEdited { prompt_id, summary } => {
                if let Some(q) = self.queued.iter_mut().find(|(id, _)| *id == prompt_id) {
                    q.1 = summary;
                }
            }
            Event::PromptDequeued { prompt_id } => {
                self.take_queued(&prompt_id);
                self.held.remove(&prompt_id);
            }
            Event::PromptSteered { prompt_id } => {
                let queued = self.take_queued(&prompt_id);
                if self.consume_sent(&prompt_id, true) {
                    return;
                }
                let text = self.held.remove(&prompt_id).or(queued).unwrap_or_default();
                self.push_user(Some(&prompt_id), text, true);
            }
            Event::StatusChanged { to } => self.set_status(&to),
            Event::UserQuestion {
                tool_call_id,
                questions,
            } => self.push_question(tool_call_id, questions),
            Event::QuestionAnswered {
                tool_call_id,
                answers,
            } => self.answer_question(&tool_call_id, Some(answers)),
            Event::ResumeStarted => self.waking = true,
            Event::Resumed => {
                self.waking = false;
                self.push_marker(Marker::Resumed, "resumed");
            }
            Event::RecoveredFromCheckpoint => {
                self.push_marker(Marker::Recovered, "recovered from a checkpoint");
            }
            Event::DurabilityRollback => {
                self.push_marker(Marker::Rollback, "guest disk was rolled back");
            }
            Event::ResumedFromDisk => self.push_marker(
                Marker::ResumedFromDisk,
                "resumed from disk; running processes were stopped",
            ),
            Event::TitleSuggested { title } => {
                if !title.is_empty() {
                    self.title = Some(title);
                }
            }
            Event::Other => {}
        }
    }

    /// The exec cell for `exec_id`, or a standalone one when its
    /// `exec_started` is outside the loaded window.
    fn open_exec(&mut self, exec_id: &str) -> usize {
        if let Some(&i) = self.open_execs.get(exec_id) {
            return i;
        }
        self.close_active();
        let i = self.transcript.push(Cell::Exec {
            command: "".into(),
            output: String::new(),
            exit: None,
            done: false,
            expanded: false,
        });
        self.open_execs.insert(exec_id.to_string(), i);
        i
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(kind: &str, payload: &str) -> Event {
        Event::decode(kind, payload)
    }

    fn texts(r: &Reducer) -> Vec<String> {
        (r.transcript.first_index()..r.transcript.len())
            .filter_map(|i| r.transcript.get(i))
            .map(|e| match &e.cell {
                Cell::User { text, pending, .. } => {
                    format!("user:{text}{}", if *pending { "?" } else { "" })
                }
                Cell::Assistant { md, open, .. } => {
                    format!("agent:{}{}", md.text(), if *open { "…" } else { "" })
                }
                Cell::Tool { name, state, .. } => format!("tool:{name}:{state:?}"),
                Cell::Exec { command, exit, .. } => format!("exec:{command}:{exit:?}"),
                Cell::Question { answered, .. } => format!("question:{answered}"),
                Cell::Plan { approved, .. } => format!("plan:{approved:?}"),
                Cell::Diff { path, .. } => format!("diff:{path}"),
                Cell::Marker { text, .. } => format!("marker:{text}"),
                Cell::Hidden => "hidden".to_string(),
            })
            .collect()
    }

    #[test]
    fn chunks_render_markdown_as_paragraphs_commit() {
        let mut r = Reducer::new();
        r.apply(ev("run_started", r#"{"run_id":"r1","prompt_id":"p1"}"#));
        r.apply(ev(
            "agent_message_chunk",
            r#"{"run_id":"r1","message_id":"m1","chunk":"**bold** start\n\n- it"}"#,
        ));
        assert_eq!(texts(&r), vec!["user:?", "agent:bold start\n\n- it…"]);
        r.apply(ev(
            "agent_message_chunk",
            r#"{"run_id":"r1","message_id":"m1","chunk":"em\n- two"}"#,
        ));
        assert_eq!(
            texts(&r),
            vec!["user:?", "agent:bold start\n\n- item\n- two…"]
        );
        r.apply(ev(
            "agent_message",
            r#"{"run_id":"r1","message_id":"m1","role":"assistant","text":"**bold** start\n\n- item\n- two"}"#,
        ));
        r.apply(ev("run_completed", r#"{"run_id":"r1","ok":true}"#));
        assert_eq!(
            texts(&r),
            vec!["user:?", "agent:bold start\n\n• item\n• two"]
        );
        let e = r.transcript.get(1).unwrap();
        assert!(e.is_final());
        if let Cell::Assistant { md, .. } = &e.cell {
            assert!(md.raw().is_empty(), "the source is dropped once final");
            assert_eq!(md.runs().len(), 3, "bold + two bullet markers");
        } else {
            panic!("assistant cell");
        }
    }

    #[test]
    fn file_changed_replaces_its_tool_card() {
        let mut r = Reducer::new();
        r.apply(ev("run_started", r#"{"run_id":"r1","prompt_id":"p1"}"#));
        r.apply(ev(
            "tool_call_started",
            r#"{"run_id":"r1","tool_call_id":"t1","tool_name":"Write","args_summary":"a.txt"}"#,
        ));
        r.apply(ev(
            "file_changed",
            r#"{"run_id":"r1","tool_call_id":"t1","path":"a.txt","change":{"write":{"content":"hi\n"}}}"#,
        ));
        r.apply(ev(
            "tool_call_completed",
            r#"{"run_id":"r1","tool_call_id":"t1","tool_name":"Write","ok":true}"#,
        ));
        assert_eq!(texts(&r), vec!["user:?", "diff:a.txt"]);
        // Out of window: a diff with no card of its own stands alone.
        r.apply(ev(
            "file_changed",
            r#"{"run_id":"r1","tool_call_id":"t9","path":"b.txt","change":{"write":{"content":"x"}}}"#,
        ));
        assert_eq!(texts(&r), vec!["user:?", "diff:a.txt", "diff:b.txt"]);
    }

    #[test]
    fn a_prompt_id_echo_is_held_until_its_run_starts() {
        let mut r = Reducer::new();
        r.apply(ev(
            "agent_message",
            r#"{"run_id":"","message_id":"u1","role":"user","text":"hi","prompt_id":"p1"}"#,
        ));
        assert!(texts(&r).is_empty(), "held, not rendered");
        r.apply(ev("run_started", r#"{"run_id":"r1","prompt_id":"p1"}"#));
        assert_eq!(texts(&r), vec!["user:hi"]);
        assert!(r.is_running());
    }

    #[test]
    fn an_echo_after_its_run_started_fills_the_pending_cell() {
        let mut r = Reducer::new();
        r.apply(ev("run_started", r#"{"run_id":"r1","prompt_id":"p1"}"#));
        assert_eq!(texts(&r), vec!["user:?"]);
        r.apply(ev(
            "agent_message",
            r#"{"run_id":"r1","message_id":"u1","role":"user","text":"late","prompt_id":"p1"}"#,
        ));
        assert_eq!(texts(&r), vec!["user:late"]);
    }

    #[test]
    fn chunks_stream_and_the_final_message_replaces_them() {
        let mut r = Reducer::new();
        r.apply(ev("run_started", r#"{"run_id":"r1"}"#));
        r.apply(ev(
            "agent_message_chunk",
            r#"{"run_id":"r1","message_id":"m1","chunk":"Hel"}"#,
        ));
        r.apply(ev(
            "agent_message_chunk",
            r#"{"run_id":"r1","message_id":"m1","chunk":"lo"}"#,
        ));
        assert_eq!(texts(&r), vec!["agent:Hello…"]);
        r.apply(ev(
            "agent_message",
            r#"{"run_id":"r1","message_id":"m1","role":"assistant","text":"Hello!"}"#,
        ));
        assert_eq!(texts(&r), vec!["agent:Hello!…"]);
        r.apply(ev(
            "agent_message",
            r#"{"run_id":"r1","message_id":"m2","role":"assistant","text":"More."}"#,
        ));
        assert_eq!(texts(&r), vec!["agent:Hello!\n\nMore.…"]);
        r.apply(ev("run_completed", r#"{"run_id":"r1","ok":true}"#));
        assert_eq!(texts(&r), vec!["agent:Hello!\n\nMore."]);
        assert!(!r.is_running());
    }

    #[test]
    fn tools_close_the_text_cell_and_complete_by_id() {
        let mut r = Reducer::new();
        r.apply(ev("run_started", r#"{"run_id":"r1"}"#));
        r.apply(ev(
            "agent_message",
            r#"{"run_id":"r1","message_id":"m1","role":"assistant","text":"Looking."}"#,
        ));
        r.apply(ev(
            "tool_call_started",
            r#"{"run_id":"r1","tool_call_id":"t1","tool_name":"Read","args_summary":"a.rs"}"#,
        ));
        r.apply(ev(
            "tool_call_completed",
            r#"{"run_id":"r1","tool_call_id":"t1","tool_name":"","ok":true,"duration_ms":5}"#,
        ));
        r.apply(ev(
            "agent_message",
            r#"{"run_id":"r1","message_id":"m2","role":"assistant","text":"Done."}"#,
        ));
        r.apply(ev("run_completed", r#"{"run_id":"r1","ok":true}"#));
        assert_eq!(
            texts(&r),
            vec!["agent:Looking.", "tool:Read:Ok", "agent:Done."]
        );
    }

    #[test]
    fn a_question_blocks_running_until_answered() {
        let mut r = Reducer::new();
        r.apply(ev("run_started", r#"{"run_id":"r1"}"#));
        r.apply(ev(
            "tool_call_requested",
            r#"{"tool_call_id":"q1","name":"ask_user_question","args_json":"{\"questions\":[{\"question\":\"A?\",\"header\":\"h\",\"multiSelect\":false,\"options\":[{\"label\":\"x\",\"description\":\"\"}]}]}"}"#,
        ));
        assert!(!r.is_running());
        assert!(r.awaiting_decision());
        r.apply(ev(
            "tool_result_submitted",
            r#"{"tool_call_id":"q1","result_json":"{\"A?\":[\"x\"]}"}"#,
        ));
        assert_eq!(texts(&r), vec!["question:true"]);
        assert!(r.is_running());
    }

    #[test]
    fn an_inactive_status_settles_an_open_run() {
        let mut r = Reducer::new();
        r.apply(ev("run_started", r#"{"run_id":"r1"}"#));
        r.apply(ev(
            "exec_started",
            r#"{"exec_id":"e1","command":["ls","-l"]}"#,
        ));
        assert!(r.is_running());
        r.apply(ev("status_changed", r#"{"from":"active","to":"parked"}"#));
        assert!(!r.is_running());
        assert_eq!(texts(&r), vec!["exec:ls -l:None"]);
        assert!(r.transcript.get(0).unwrap().is_final());
    }

    #[test]
    fn the_queue_follows_queued_edited_dequeued_and_steered() {
        let mut r = Reducer::new();
        r.apply(ev("run_started", r#"{"run_id":"r1"}"#));
        r.apply(ev("prompt_queued", r#"{"prompt_id":"p2","summary":"two"}"#));
        r.apply(ev(
            "prompt_queued",
            r#"{"prompt_id":"p3","summary":"three"}"#,
        ));
        r.apply(ev(
            "prompt_edited",
            r#"{"prompt_id":"p2","summary":"two!"}"#,
        ));
        assert_eq!(
            r.queued,
            vec![
                ("p2".to_string(), "two!".to_string()),
                ("p3".to_string(), "three".to_string())
            ]
        );
        r.apply(ev("prompt_dequeued", r#"{"prompt_id":"p3"}"#));
        r.apply(ev("prompt_steered", r#"{"prompt_id":"p2"}"#));
        assert!(r.queued.is_empty());
        assert_eq!(texts(&r), vec!["user:two!"]);
    }

    #[test]
    fn an_optimistic_prompt_is_consumed_in_place_or_moved_to_the_queue() {
        let mut r = Reducer::new();
        r.sent("p1", "first".into());
        assert_eq!(texts(&r), vec!["user:first?"]);
        r.apply(ev(
            "agent_message",
            r#"{"run_id":"","message_id":"u1","role":"user","text":"first","prompt_id":"p1"}"#,
        ));
        r.apply(ev("run_started", r#"{"run_id":"r1","prompt_id":"p1"}"#));
        assert_eq!(texts(&r), vec!["user:first"]);
        r.sent("p2", "second".into());
        r.apply(ev(
            "prompt_queued",
            r#"{"prompt_id":"p2","summary":"second"}"#,
        ));
        assert_eq!(texts(&r), vec!["user:first", "hidden"]);
        assert_eq!(r.queued.len(), 1);
        r.apply(ev("run_completed", r#"{"run_id":"r1","ok":true}"#));
        r.apply(ev("run_started", r#"{"run_id":"r2","prompt_id":"p2"}"#));
        assert_eq!(texts(&r), vec!["user:first", "hidden", "user:second"]);
        assert!(r.queued.is_empty());
    }

    #[test]
    fn canonical_tool_names() {
        assert_eq!(canonical_tool("ExitPlanMode"), "exit_plan_mode");
        assert_eq!(canonical_tool("mcp__x__exit_plan_mode"), "exit_plan_mode");
        assert_eq!(canonical_tool("Read"), "read");
    }

    #[test]
    fn a_refired_question_replaces_its_card_and_keeps_the_picks() {
        let mut r = Reducer::new();
        let req = r#"{"tool_call_id":"q1","name":"ask_user_question","args_json":"{\"questions\":[{\"question\":\"A?\",\"header\":\"h\",\"multiSelect\":false,\"options\":[{\"label\":\"x\",\"description\":\"\"},{\"label\":\"y\",\"description\":\"\"}]}]}"}"#;
        r.apply(ev("tool_call_requested", req));
        if let Some(e) = r.transcript.get_mut(0) {
            if let Cell::Question { picks, .. } = &mut e.cell {
                picks[0].push(1);
            }
        }
        r.apply(ev("resumed", "{}"));
        r.apply(ev("tool_call_requested", req));
        assert_eq!(
            texts(&r),
            vec!["hidden", "marker:resumed", "question:false"]
        );
        assert_eq!(r.pending_decision(), Some((2, "q1".to_string())));
        match &r.transcript.get(2).unwrap().cell {
            Cell::Question { picks, .. } => assert_eq!(picks, &vec![vec![1u16]]),
            other => panic!("{other:?}"),
        }
        r.apply(ev(
            "tool_result_submitted",
            r#"{"tool_call_id":"q1","result_json":"{\"A?\":[\"x\"]}"}"#,
        ));
        assert!(!r.awaiting_decision());
        match &r.transcript.get(2).unwrap().cell {
            Cell::Question {
                picks, answered, ..
            } => {
                assert!(*answered);
                assert_eq!(picks, &vec![vec![0u16]]);
            }
            other => panic!("{other:?}"),
        }
    }
}
