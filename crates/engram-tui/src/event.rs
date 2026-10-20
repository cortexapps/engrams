//! Session events as the orchestrator's SSE tail and `ListSessionEvents`
//! deliver them: an envelope `{idx, kind, payload_json}` whose payload shape
//! depends on `kind`. Only the fields the transcript needs are decoded; the
//! shapes mirror `web/src/events.ts`.

use serde::Deserialize;

/// The envelope on the wire (SSE `data:` and `SessionEvent` alike).
#[derive(Debug, Clone, Deserialize)]
pub struct Envelope {
    pub idx: Option<i64>,
    pub kind: String,
    pub payload_json: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Question {
    pub question: String,
    #[serde(default)]
    pub header: String,
    #[serde(default, rename = "multiSelect")]
    pub multi_select: bool,
    #[serde(default)]
    pub options: Vec<QuestionOption>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct QuestionOption {
    pub label: String,
    #[serde(default)]
    pub description: String,
}

/// Answers to a question card: question text → the labels picked.
pub type Answers = std::collections::HashMap<String, Vec<String>>;

/// A decoded event. `Other` carries every kind the transcript does not
/// render; the footer still sees the kind name.
#[derive(Debug, Clone)]
pub enum Event {
    RunStarted {
        prompt_id: Option<String>,
        prompt_summary: Option<String>,
        /// When the run started, in Unix seconds.
        at: Option<u64>,
    },
    AgentMessage {
        role: String,
        text: String,
        prompt_id: Option<String>,
        message_id: String,
    },
    Chunk {
        message_id: String,
        chunk: String,
    },
    ToolCallStarted {
        tool_call_id: String,
        tool_name: String,
        args_summary: String,
    },
    ToolCallCompleted {
        tool_call_id: String,
        tool_name: String,
        ok: bool,
        duration_ms: Option<u64>,
        result_summary: Option<String>,
    },
    ToolCallRequested {
        tool_call_id: String,
        name: String,
        args_json: String,
    },
    ToolResultSubmitted {
        tool_call_id: String,
        result_json: String,
    },
    ExecStarted {
        exec_id: String,
        command: Vec<String>,
    },
    ExecOutput {
        exec_id: String,
        chunk: String,
    },
    ExecCompleted {
        exec_id: String,
        exit_status: Option<i32>,
        wall_ms: Option<u64>,
    },
    RunEnded {
        ok: bool,
        interrupted: bool,
        /// When the run ended, in Unix seconds.
        at: Option<u64>,
    },
    HarnessIdle,
    HarnessModeChanged {
        mode: String,
    },
    PromptQueued {
        prompt_id: String,
        summary: String,
    },
    PromptEdited {
        prompt_id: String,
        summary: String,
    },
    PromptDequeued {
        prompt_id: String,
    },
    PromptSteered {
        prompt_id: String,
    },
    StatusChanged {
        to: String,
    },
    UserQuestion {
        tool_call_id: String,
        questions: Vec<Question>,
    },
    QuestionAnswered {
        tool_call_id: String,
        /// Question text → the labels picked.
        answers: Answers,
    },
    ResumeStarted,
    Resumed,
    RecoveredFromCheckpoint,
    DurabilityRollback,
    ResumedFromDisk,
    TitleSuggested {
        title: String,
    },
    /// A file the agent wrote or edited. `text` is the diff as `+`/`-`
    /// prefixed lines (hunks separated by a `⋯` line); a `patch` change
    /// carries its unified diff as is.
    FileChanged {
        tool_call_id: String,
        path: String,
        /// `Wrote`, `Edited` or `Patched`.
        verb: &'static str,
        text: String,
        added: u32,
        removed: u32,
    },
    Other,
}

#[derive(Deserialize)]
struct FileChangedP {
    tool_call_id: String,
    path: String,
    change: ChangeP,
}

#[derive(Deserialize, Default)]
struct ChangeP {
    write: Option<WriteP>,
    edit: Option<EditP>,
    patch: Option<PatchP>,
}

#[derive(Deserialize)]
struct WriteP {
    content: String,
}

#[derive(Deserialize)]
struct EditP {
    hunks: Vec<HunkP>,
}

#[derive(Deserialize)]
struct HunkP {
    #[serde(default)]
    old: String,
    #[serde(default)]
    new: String,
}

#[derive(Deserialize)]
struct PatchP {
    unified_diff: String,
}

fn push_lines(out: &mut String, prefix: char, text: &str) -> u32 {
    let mut n = 0;
    for line in text.trim_end_matches('\n').split('\n') {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push(prefix);
        out.push_str(line);
        n += 1;
    }
    n
}

/// Flatten a change into `+`/`-` lines; returns `(text, added, removed)`.
fn diff_text(change: ChangeP) -> (String, u32, u32) {
    let mut out = String::new();
    let (mut added, mut removed) = (0, 0);
    if let Some(w) = change.write {
        added += push_lines(&mut out, '+', &w.content);
    } else if let Some(e) = change.edit {
        for (i, h) in e.hunks.iter().enumerate() {
            if i > 0 {
                out.push_str("\n⋯");
            }
            if !h.old.is_empty() {
                removed += push_lines(&mut out, '-', &h.old);
            }
            if !h.new.is_empty() {
                added += push_lines(&mut out, '+', &h.new);
            }
        }
    } else if let Some(p) = change.patch {
        for line in p.unified_diff.trim_end_matches('\n').split('\n') {
            if line.starts_with("+++") || line.starts_with("---") {
                continue;
            }
            match line.as_bytes().first() {
                Some(b'+') => added += 1,
                Some(b'-') => removed += 1,
                _ => {}
            }
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(line);
        }
    }
    (out, added, removed)
}

#[derive(Deserialize)]
struct RunStartedP {
    prompt_id: Option<String>,
    prompt_summary: Option<String>,
    at: Option<String>,
}
#[derive(Deserialize)]
struct AtP {
    at: Option<String>,
}
#[derive(Deserialize)]
struct AgentMessageP {
    #[serde(default)]
    role: String,
    #[serde(default)]
    text: String,
    prompt_id: Option<String>,
    #[serde(default)]
    message_id: String,
}
#[derive(Deserialize)]
struct ChunkP {
    #[serde(default)]
    message_id: String,
    #[serde(default)]
    chunk: String,
}
#[derive(Deserialize)]
struct ToolStartedP {
    tool_call_id: String,
    #[serde(default)]
    tool_name: String,
    args_summary: Option<String>,
}
#[derive(Deserialize)]
struct ToolCompletedP {
    tool_call_id: String,
    #[serde(default)]
    tool_name: String,
    #[serde(default)]
    ok: bool,
    duration_ms: Option<u64>,
    result_summary: Option<String>,
}
#[derive(Deserialize)]
struct ToolRequestedP {
    tool_call_id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    args_json: String,
}
#[derive(Deserialize)]
struct ToolResultP {
    tool_call_id: String,
    #[serde(default)]
    result_json: String,
}
#[derive(Deserialize)]
struct ExecStartedP {
    exec_id: String,
    #[serde(default)]
    command: Vec<String>,
}
#[derive(Deserialize)]
struct ExecOutputP {
    exec_id: String,
    #[serde(default)]
    chunk: String,
}
#[derive(Deserialize)]
struct RusageP {
    wall_ms: Option<u64>,
}
#[derive(Deserialize)]
struct ExecCompletedP {
    exec_id: String,
    exit_status: Option<i32>,
    rusage: Option<RusageP>,
}
#[derive(Deserialize)]
struct RunCompletedP {
    #[serde(default)]
    ok: bool,
    at: Option<String>,
}
#[derive(Deserialize)]
struct ModeP {
    #[serde(default)]
    mode: String,
}
#[derive(Deserialize)]
struct PromptP {
    prompt_id: String,
    summary: Option<String>,
}
#[derive(Deserialize)]
struct PromptIdP {
    prompt_id: String,
}
#[derive(Deserialize)]
struct StatusP {
    #[serde(default)]
    to: String,
}
#[derive(Deserialize)]
struct UserQuestionP {
    tool_call_id: String,
    #[serde(default)]
    questions: Vec<Question>,
}
#[derive(Deserialize)]
struct QuestionAnsweredP {
    tool_call_id: String,
    #[serde(default)]
    answers: Answers,
}
#[derive(Deserialize)]
struct TitleP {
    #[serde(default)]
    title: String,
}

fn p<'a, T: Deserialize<'a>>(s: &'a str) -> Option<T> {
    serde_json::from_str(s).ok()
}

impl Event {
    /// Decode one envelope. A payload that does not parse decodes to
    /// `Other`: the transcript skips it, nothing panics, the raw event is
    /// still in the server log.
    pub fn decode(kind: &str, payload: &str) -> Event {
        let decoded = match kind {
            "run_started" => p::<RunStartedP>(payload).map(|v| Event::RunStarted {
                prompt_id: v.prompt_id.filter(|s| !s.is_empty()),
                prompt_summary: v.prompt_summary.filter(|s| !s.is_empty()),
                at: v.at.as_deref().and_then(unix_secs),
            }),
            "agent_message" => p::<AgentMessageP>(payload).map(|v| Event::AgentMessage {
                role: v.role,
                text: v.text,
                prompt_id: v.prompt_id.filter(|s| !s.is_empty()),
                message_id: v.message_id,
            }),
            "agent_message_chunk" => p::<ChunkP>(payload).map(|v| Event::Chunk {
                message_id: v.message_id,
                chunk: v.chunk,
            }),
            "tool_call_started" => p::<ToolStartedP>(payload).map(|v| Event::ToolCallStarted {
                tool_call_id: v.tool_call_id,
                tool_name: v.tool_name,
                args_summary: v.args_summary.unwrap_or_default(),
            }),
            "tool_call_completed" => {
                p::<ToolCompletedP>(payload).map(|v| Event::ToolCallCompleted {
                    tool_call_id: v.tool_call_id,
                    tool_name: v.tool_name,
                    ok: v.ok,
                    duration_ms: v.duration_ms,
                    result_summary: v.result_summary,
                })
            }
            "tool_call_requested" => {
                p::<ToolRequestedP>(payload).map(|v| Event::ToolCallRequested {
                    tool_call_id: v.tool_call_id,
                    name: v.name,
                    args_json: v.args_json,
                })
            }
            "tool_result_submitted" => {
                p::<ToolResultP>(payload).map(|v| Event::ToolResultSubmitted {
                    tool_call_id: v.tool_call_id,
                    result_json: v.result_json,
                })
            }
            "exec_started" => p::<ExecStartedP>(payload).map(|v| Event::ExecStarted {
                exec_id: v.exec_id,
                command: v.command,
            }),
            "stdout" | "stderr" => p::<ExecOutputP>(payload).map(|v| Event::ExecOutput {
                exec_id: v.exec_id,
                chunk: v.chunk,
            }),
            "exec_completed" => p::<ExecCompletedP>(payload).map(|v| Event::ExecCompleted {
                exec_id: v.exec_id,
                exit_status: v.exit_status,
                wall_ms: v.rusage.and_then(|r| r.wall_ms),
            }),
            "run_completed" => p::<RunCompletedP>(payload).map(|v| Event::RunEnded {
                ok: v.ok,
                interrupted: false,
                at: v.at.as_deref().and_then(unix_secs),
            }),
            "run_interrupted" => Some(Event::RunEnded {
                ok: false,
                interrupted: true,
                at: p::<AtP>(payload)
                    .and_then(|v| v.at)
                    .as_deref()
                    .and_then(unix_secs),
            }),
            "harness_idle" => Some(Event::HarnessIdle),
            "harness_mode_changed" => {
                p::<ModeP>(payload).map(|v| Event::HarnessModeChanged { mode: v.mode })
            }
            "prompt_queued" => p::<PromptP>(payload).map(|v| Event::PromptQueued {
                prompt_id: v.prompt_id,
                summary: v.summary.unwrap_or_default(),
            }),
            "prompt_edited" => p::<PromptP>(payload).map(|v| Event::PromptEdited {
                prompt_id: v.prompt_id,
                summary: v.summary.unwrap_or_default(),
            }),
            "prompt_dequeued" => p::<PromptIdP>(payload).map(|v| Event::PromptDequeued {
                prompt_id: v.prompt_id,
            }),
            "prompt_steered" => p::<PromptIdP>(payload).map(|v| Event::PromptSteered {
                prompt_id: v.prompt_id,
            }),
            "status_changed" => p::<StatusP>(payload).map(|v| Event::StatusChanged { to: v.to }),
            "user_question" => p::<UserQuestionP>(payload).map(|v| Event::UserQuestion {
                tool_call_id: v.tool_call_id,
                questions: v.questions,
            }),
            "question_answered" => {
                p::<QuestionAnsweredP>(payload).map(|v| Event::QuestionAnswered {
                    tool_call_id: v.tool_call_id,
                    answers: v.answers,
                })
            }
            "resume_started" => Some(Event::ResumeStarted),
            "resumed" => Some(Event::Resumed),
            "recovered_from_checkpoint" => Some(Event::RecoveredFromCheckpoint),
            "durability_rollback" => Some(Event::DurabilityRollback),
            "resumed_from_disk" => Some(Event::ResumedFromDisk),
            "title_suggested" => {
                p::<TitleP>(payload).map(|v| Event::TitleSuggested { title: v.title })
            }
            "file_changed" => p::<FileChangedP>(payload).map(|v| {
                let verb = if v.change.write.is_some() {
                    "Wrote"
                } else if v.change.patch.is_some() {
                    "Patched"
                } else {
                    "Edited"
                };
                let (text, added, removed) = diff_text(v.change);
                Event::FileChanged {
                    tool_call_id: v.tool_call_id,
                    path: v.path,
                    verb,
                    text,
                    added,
                    removed,
                }
            }),
            _ => None,
        };
        decoded.unwrap_or(Event::Other)
    }
}

/// Unix seconds of an RFC 3339 timestamp (`2026-01-02T03:04:05.678Z`).
/// Fractions are dropped; a zone offset is applied. Anything else is
/// `None`.
pub fn unix_secs(at: &str) -> Option<u64> {
    let b = at.as_bytes();
    let num = |from: usize, len: usize| -> Option<i64> {
        let s = b.get(from..from + len)?;
        if !s.iter().all(u8::is_ascii_digit) {
            return None;
        }
        std::str::from_utf8(s).ok()?.parse().ok()
    };
    if b.len() < 19
        || b[4] != b'-'
        || b[7] != b'-'
        || b[10] != b'T'
        || b[13] != b':'
        || b[16] != b':'
    {
        return None;
    }
    let (y, m, d) = (num(0, 4)?, num(5, 2)?, num(8, 2)?);
    let (hh, mm, ss) = (num(11, 2)?, num(14, 2)?, num(17, 2)?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || hh > 23 || mm > 59 || ss > 60 {
        return None;
    }
    // Days from civil (Howard Hinnant's algorithm).
    let (y, m) = if m <= 2 { (y - 1, m + 9) } else { (y, m - 3) };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * m + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let mut secs = days * 86_400 + hh * 3_600 + mm * 60 + ss;
    // The zone: `Z`, or `±HH:MM` after the seconds and any fraction.
    let mut i = 19;
    if b.get(i) == Some(&b'.') {
        i += 1;
        while b.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
    }
    match b.get(i) {
        Some(b'Z') | None => {}
        Some(sign @ (b'+' | b'-')) => {
            let (oh, om) = (num(i + 1, 2)?, num(i + 4, 2)?);
            let off = oh * 3_600 + om * 60;
            secs = if *sign == b'+' {
                secs - off
            } else {
                secs + off
            };
        }
        _ => return None,
    }
    u64::try_from(secs).ok()
}

/// The `questions` array of an `ask_user_question` request's `args_json`.
pub fn parse_questions(args_json: &str) -> Option<Vec<Question>> {
    #[derive(Deserialize)]
    struct Args {
        questions: Vec<Question>,
    }
    p::<Args>(args_json)
        .map(|a| a.questions)
        .filter(|q| !q.is_empty())
}

/// The answers in an `ask_user_question` result: `{question: [labels]}`.
pub fn parse_answers(result_json: &str) -> Option<Answers> {
    p::<Answers>(result_json)
}

/// The decision in an `exit_plan_mode` result: `{"decision":"approve"|"reject"}`.
pub fn parse_plan_decision(result_json: &str) -> Option<bool> {
    #[derive(Deserialize)]
    struct R {
        decision: String,
    }
    match p::<R>(result_json)?.decision.as_str() {
        "approve" => Some(true),
        "reject" => Some(false),
        _ => None,
    }
}

/// The `plan` text of an `exit_plan_mode` request's `args_json`.
pub fn parse_plan(args_json: &str) -> Option<String> {
    #[derive(Deserialize)]
    struct Args {
        plan: String,
    }
    p::<Args>(args_json).map(|a| a.plan)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_the_kinds_the_transcript_renders() {
        match Event::decode(
            "run_started",
            r#"{"run_id":"r1","prompt_summary":null,"prompt_id":"p1","at":"t"}"#,
        ) {
            Event::RunStarted {
                prompt_id,
                prompt_summary,
                ..
            } => {
                assert_eq!(prompt_id.as_deref(), Some("p1"));
                assert!(prompt_summary.is_none());
            }
            other => panic!("{other:?}"),
        }
        match Event::decode(
            "exec_completed",
            r#"{"exec_id":"e","exit_status":3,"rusage":{"wall_ms":12}}"#,
        ) {
            Event::ExecCompleted {
                exit_status,
                wall_ms,
                ..
            } => {
                assert_eq!(exit_status, Some(3));
                assert_eq!(wall_ms, Some(12));
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            Event::decode("status_changed", r#"{"from":"a","to":"idle"}"#),
            Event::StatusChanged { to } if to == "idle"
        ));
    }

    #[test]
    fn garbage_never_panics() {
        assert!(matches!(Event::decode("run_started", "nope"), Event::Other));
        match Event::decode(
            "file_changed",
            r#"{"run_id":"r","tool_call_id":"t","path":"src/a.rs","change":{"edit":{"hunks":[{"old":"a\nb","new":"c"},{"old":"","new":"d\n"}]}},"at":"2026-01-01T00:00:00Z"}"#,
        ) {
            Event::FileChanged {
                path,
                text,
                added,
                removed,
                ..
            } => {
                assert_eq!(path, "src/a.rs");
                assert_eq!(text, "-a\n-b\n+c\n⋯\n+d");
                assert_eq!((added, removed), (2, 2));
            }
            other => panic!("{other:?}"),
        }
        match Event::decode(
            "file_changed",
            r#"{"tool_call_id":"t","path":"n.txt","change":{"write":{"content":"x\ny\n"}}}"#,
        ) {
            Event::FileChanged { text, added, .. } => {
                assert_eq!(text, "+x\n+y");
                assert_eq!(added, 2);
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(Event::decode("unknown_kind", "{}"), Event::Other));
        assert!(matches!(
            Event::decode("agent_message", "{}"),
            Event::AgentMessage { .. }
        ));
    }

    #[test]
    fn parses_question_and_plan_args() {
        let q = parse_questions(
            r#"{"questions":[{"question":"Which?","header":"Lib","multiSelect":false,"options":[{"label":"a","description":"A"}]}]}"#,
        )
        .unwrap();
        assert_eq!(q[0].options[0].label, "a");
        assert!(parse_questions(r#"{"questions":[]}"#).is_none());
        assert_eq!(parse_plan(r#"{"plan":"do x"}"#).as_deref(), Some("do x"));
        assert_eq!(parse_plan_decision(r#"{"decision":"approve"}"#), Some(true));
        assert_eq!(parse_plan_decision(r#"{"ok":true}"#), None);
    }

    #[test]
    fn timestamps_parse_to_unix_seconds() {
        assert_eq!(unix_secs("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(unix_secs("2026-01-02T03:04:05.678Z"), Some(1_767_323_045));
        assert_eq!(unix_secs("2026-01-02T03:04:05+01:00"), Some(1_767_319_445));
        assert_eq!(unix_secs("2026-01-02 03:04:05"), None);
        assert_eq!(unix_secs("garbage"), None);
    }
}
