//! Issue #539: the capture-time `[warm]`-hook progress protocol + the
//! host-side watchdog that replaces "burn the whole global timeout before
//! anyone notices" with stall detection, per-stage deadlines, and a
//! carried output tail.
//!
//! Everything in this module is pure and clock-injected (`Instant`/
//! `DateTime<Utc>` passed in, never read internally) so the watchdog state
//! machine is unit-testable without sleeping. The I/O side — driving
//! `exec_stream`, running the `tokio::select!` loop against
//! [`WarmWatchdog::next_deadline`], and forwarding [`CaptureProgress`]
//! events — lives in `pooled_backend::run_warm_hook`.
//!
//! ## Progress line grammar
//!
//! ```text
//! ::engram-warm:: event=start stage=<name> [deadline_secs=<u64>] [msg=<free text>]
//! ::engram-warm:: event=heartbeat [stage=<name>] [msg=<free text>]
//! ::engram-warm:: event=done stage=<name>
//! ```
//!
//! Space-separated `key=value` tokens; `msg=` consumes to end-of-line;
//! unknown keys are ignored (forward-compatible); a line that doesn't
//! parse (missing prefix, missing required key, non-numeric
//! `deadline_secs`, an unrecognized `event=`, or a token without `=`) is
//! `None` — treated as ordinary hook output, not an error.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use engram_core::types::{CaptureFailureKind, WarmStageOutcome, WarmStageRecord};

/// Sentinel prefix a `[warm]` hook's stdout line must start with (after
/// trimming leading whitespace) to be treated as a progress line.
pub const SENTINEL_PREFIX: &str = "::engram-warm::";

/// Default stall budget: how long a *conforming* hook (one that has
/// already emitted >=1 progress line) may go with no stdout/stderr bytes
/// and no progress line before the watchdog kills the capture. Overridden
/// by `ENGRAM_WARM_STALL_SECS`.
pub const DEFAULT_STALL_SECS: u64 = 120;

/// Read `ENGRAM_WARM_STALL_SECS` — falls through to
/// [`DEFAULT_STALL_SECS`].
pub fn warm_stall_secs_from_env() -> Duration {
    std::env::var("ENGRAM_WARM_STALL_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or_else(|| Duration::from_secs(DEFAULT_STALL_SECS))
}

// ─── progress-line parser ──────────────────────────────────────────

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WarmProgressLine {
    pub event: WarmEvent,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WarmEvent {
    Start {
        stage: String,
        deadline_secs: Option<u64>,
        msg: Option<String>,
    },
    Heartbeat {
        stage: Option<String>,
        msg: Option<String>,
    },
    Done {
        stage: String,
    },
}

/// Parse one `[warm]`-hook stdout line against the sentinel grammar.
/// `None` for anything that doesn't conform — the caller treats it as
/// ordinary output (tail-captured, counted as a stall-resetting byte, but
/// not a stage transition).
pub fn parse_progress_line(line: &str) -> Option<WarmProgressLine> {
    let rest = line.trim_start().strip_prefix(SENTINEL_PREFIX)?;
    let mut rest = rest.trim_start();

    let mut event: Option<&str> = None;
    let mut stage: Option<String> = None;
    let mut deadline_secs: Option<u64> = None;
    let mut msg: Option<String> = None;

    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix("msg=") {
            msg = Some(after.trim_end().to_string());
            break;
        }
        let (token, remainder) = match rest.find(char::is_whitespace) {
            Some(idx) => (&rest[..idx], rest[idx..].trim_start()),
            None => (rest, ""),
        };
        let (key, value) = token.split_once('=')?;
        match key {
            "event" => event = Some(value),
            "stage" => stage = Some(value.to_string()),
            "deadline_secs" => deadline_secs = Some(value.parse().ok()?),
            // Forward-compatible: a future field a newer hook emits that
            // this host doesn't know about yet.
            _ => {}
        }
        rest = remainder;
    }

    let ev = match event? {
        "start" => WarmEvent::Start {
            stage: stage?,
            deadline_secs,
            msg,
        },
        "heartbeat" => WarmEvent::Heartbeat { stage, msg },
        "done" => WarmEvent::Done { stage: stage? },
        _ => return None,
    };
    Some(WarmProgressLine { event: ev })
}

// ─── output tail ────────────────────────────────────────────────────

/// Rolling ring buffer over combined stdout+stderr, capped at
/// [`Self::DEFAULT_CAP_BYTES`]. Kept on success *and* failure so a
/// `[warm]` hook's last words survive the capture VM's teardown.
#[derive(Clone, Debug)]
pub struct OutputTail {
    buf: VecDeque<u8>,
    cap: usize,
}

impl OutputTail {
    pub const DEFAULT_CAP_BYTES: usize = 16 * 1024;

    pub fn new(cap: usize) -> Self {
        Self {
            buf: VecDeque::with_capacity(cap.min(1 << 20)),
            cap,
        }
    }

    pub fn push(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        if bytes.len() >= self.cap {
            self.buf.clear();
            self.buf.extend(&bytes[bytes.len() - self.cap..]);
            return;
        }
        self.buf.extend(bytes.iter().copied());
        while self.buf.len() > self.cap {
            self.buf.pop_front();
        }
    }

    /// UTF-8-lossy render of the buffered bytes, oldest first.
    pub fn render(&self) -> String {
        let (a, b) = self.buf.as_slices();
        if b.is_empty() {
            return String::from_utf8_lossy(a).into_owned();
        }
        let mut v = Vec::with_capacity(a.len() + b.len());
        v.extend_from_slice(a);
        v.extend_from_slice(b);
        String::from_utf8_lossy(&v).into_owned()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }
}

impl Default for OutputTail {
    fn default() -> Self {
        Self::new(Self::DEFAULT_CAP_BYTES)
    }
}

// ─── watchdog ───────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug)]
pub struct WarmWatchdogConfig {
    /// Stall budget, armed only once the hook has emitted its first
    /// progress line ("conforming"). A hook that never emits one keeps
    /// today's single-global-timeout behavior — no regression.
    pub stall: Duration,
    /// `WarmConfig::timeout()` — the in-guest agentd SIGKILL backstop,
    /// mirrored host-side so the watchdog can fail the capture at the
    /// same instant rather than waiting on the in-guest kill + an
    /// `Exit(None)` round-trip.
    pub global_timeout: Duration,
}

/// Inputs the driving I/O loop feeds the watchdog as they occur.
#[derive(Clone, Debug)]
pub enum WatchdogInput {
    /// Stdout or stderr bytes arrived (protocol or not) — resets the
    /// stall clock but is not itself a stage transition.
    OutputBytes,
    /// A stdout line matched the `::engram-warm::` grammar.
    Progress(WarmProgressLine),
    /// The driving loop's deadline timer fired; re-check for a violation
    /// without any new input (used when the hook goes fully silent).
    Tick,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WarmViolation {
    Stall,
    StageDeadline,
    GlobalTimeout,
}

impl WarmViolation {
    pub fn kind(&self) -> CaptureFailureKind {
        match self {
            Self::Stall => CaptureFailureKind::WarmStall,
            Self::StageDeadline => CaptureFailureKind::WarmStageDeadline,
            Self::GlobalTimeout => CaptureFailureKind::WarmGlobalTimeout,
        }
    }
}

#[derive(Clone, Debug)]
struct OpenStage {
    name: String,
    started_wall: DateTime<Utc>,
    deadline: Option<Instant>,
}

/// Pure state machine driving the stall/stage-deadline/global-timeout
/// decision for a live `[warm]`-hook exec. Fed by [`WatchdogInput`]s and
/// an externally-owned clock (`now`/`wall_now`) — never reads the clock
/// itself, so tests drive it with fixed, hand-advanced timestamps.
#[derive(Clone, Debug)]
pub struct WarmWatchdog {
    cfg: WarmWatchdogConfig,
    start: Instant,
    /// Armed once the hook has emitted >=1 valid progress line.
    conforming: bool,
    last_output: Instant,
    current_stage: Option<OpenStage>,
    /// Closed stage history (a `start` implicitly closes the prior stage;
    /// `done` closes it explicitly).
    closed_stages: Vec<WarmStageRecord>,
}

impl WarmWatchdog {
    pub fn new(cfg: WarmWatchdogConfig, now: Instant) -> Self {
        Self {
            cfg,
            start: now,
            conforming: false,
            last_output: now,
            current_stage: None,
            closed_stages: Vec::new(),
        }
    }

    /// Feed one input at `(now, wall_now)`. Returns `Some(violation)` the
    /// instant a stall/stage-deadline/global-timeout condition is met —
    /// callers must check this on every event, not just on `Tick`, since a
    /// `Progress`/`OutputBytes` input can itself arrive after a deadline
    /// technically elapsed (e.g. a burst of buffered output flushed late).
    pub fn on_event(
        &mut self,
        ev: WatchdogInput,
        now: Instant,
        wall_now: DateTime<Utc>,
    ) -> Option<WarmViolation> {
        match ev {
            WatchdogInput::OutputBytes => {
                self.last_output = now;
            }
            WatchdogInput::Progress(line) => {
                self.conforming = true;
                self.last_output = now;
                match line.event {
                    WarmEvent::Start {
                        stage,
                        deadline_secs,
                        ..
                    } => {
                        self.close_current_stage(wall_now, WarmStageOutcome::Done);
                        let deadline = deadline_secs.map(|secs| now + Duration::from_secs(secs));
                        self.current_stage = Some(OpenStage {
                            name: stage,
                            started_wall: wall_now,
                            deadline,
                        });
                    }
                    WarmEvent::Heartbeat { .. } => {
                        // Heartbeats only reset the stall clock (done
                        // above); they never touch a stage's own deadline
                        // — a chatty-but-stuck stage still dies on budget.
                    }
                    WarmEvent::Done { .. } => {
                        self.close_current_stage(wall_now, WarmStageOutcome::Done);
                    }
                }
            }
            WatchdogInput::Tick => {}
        }
        self.check(now)
    }

    fn check(&self, now: Instant) -> Option<WarmViolation> {
        // Global timeout supremacy: checked first regardless of stage
        // deadlines or stall state.
        if now.duration_since(self.start) >= self.cfg.global_timeout {
            return Some(WarmViolation::GlobalTimeout);
        }
        if let Some(stage) = &self.current_stage {
            if let Some(deadline) = stage.deadline {
                if now >= deadline {
                    return Some(WarmViolation::StageDeadline);
                }
            }
        }
        if self.conforming && now.duration_since(self.last_output) >= self.cfg.stall {
            return Some(WarmViolation::Stall);
        }
        None
    }

    fn close_current_stage(&mut self, wall_now: DateTime<Utc>, outcome: WarmStageOutcome) {
        if let Some(stage) = self.current_stage.take() {
            self.closed_stages.push(WarmStageRecord {
                name: stage.name,
                started_at: stage.started_wall,
                ended_at: Some(wall_now),
                outcome,
            });
        }
    }

    /// The next instant the driving loop must wake up at (absent any new
    /// input) to re-check for a violation — the earliest of the global
    /// timeout, the open stage's deadline (if any), and the stall deadline
    /// (once conforming).
    pub fn next_deadline(&self) -> Instant {
        let mut deadline = self.start + self.cfg.global_timeout;
        if let Some(stage) = &self.current_stage {
            if let Some(sd) = stage.deadline {
                deadline = deadline.min(sd);
            }
        }
        if self.conforming {
            deadline = deadline.min(self.last_output + self.cfg.stall);
        }
        deadline
    }

    pub fn current_stage_name(&self) -> Option<&str> {
        self.current_stage.as_ref().map(|s| s.name.as_str())
    }

    pub fn is_conforming(&self) -> bool {
        self.conforming
    }

    /// Stage history so far, including the still-open current stage
    /// (`ended_at: None`, `outcome: Running`) — the shape a live
    /// `CaptureProgress` event carries.
    pub fn stage_history(&self) -> Vec<WarmStageRecord> {
        let mut out = self.closed_stages.clone();
        if let Some(stage) = &self.current_stage {
            out.push(WarmStageRecord {
                name: stage.name.clone(),
                started_at: stage.started_wall,
                ended_at: None,
                outcome: WarmStageOutcome::Running,
            });
        }
        out
    }

    /// Consume the watchdog on a terminal failure: close the open stage
    /// (if any) as `Failed` and return the final stage-history snapshot.
    pub fn finish_failed(mut self, wall_now: DateTime<Utc>) -> Vec<WarmStageRecord> {
        self.close_current_stage(wall_now, WarmStageOutcome::Failed);
        self.closed_stages
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── parser ──────────────────────────────────────────────────

    #[test]
    fn parses_start_with_deadline_and_msg() {
        let line = parse_progress_line(
            "::engram-warm:: event=start stage=deps-up deadline_secs=60 msg=installing deps now",
        )
        .expect("should parse");
        assert_eq!(
            line.event,
            WarmEvent::Start {
                stage: "deps-up".into(),
                deadline_secs: Some(60),
                msg: Some("installing deps now".into()),
            }
        );
    }

    #[test]
    fn parses_heartbeat_without_stage() {
        let line = parse_progress_line("::engram-warm:: event=heartbeat msg=still waiting")
            .expect("should parse");
        assert_eq!(
            line.event,
            WarmEvent::Heartbeat {
                stage: None,
                msg: Some("still waiting".into()),
            }
        );
    }

    #[test]
    fn parses_done() {
        let line =
            parse_progress_line("::engram-warm:: event=done stage=deps-up").expect("should parse");
        assert_eq!(
            line.event,
            WarmEvent::Done {
                stage: "deps-up".into()
            }
        );
    }

    #[test]
    fn tolerates_leading_whitespace() {
        assert!(parse_progress_line("   ::engram-warm:: event=start stage=x").is_some());
    }

    #[test]
    fn unknown_keys_are_ignored_forward_compatibly() {
        let line = parse_progress_line("::engram-warm:: event=start stage=x future_field=42")
            .expect("should parse despite an unknown key");
        assert_eq!(
            line.event,
            WarmEvent::Start {
                stage: "x".into(),
                deadline_secs: None,
                msg: None,
            }
        );
    }

    #[test]
    fn msg_consumes_to_end_of_line_including_equals_signs() {
        let line = parse_progress_line("::engram-warm:: event=heartbeat msg=waiting on x=y here")
            .expect("should parse");
        assert_eq!(
            line.event,
            WarmEvent::Heartbeat {
                stage: None,
                msg: Some("waiting on x=y here".into()),
            }
        );
    }

    #[test]
    fn ordinary_output_does_not_parse() {
        assert!(parse_progress_line("Downloading dependency foo-1.2.3.jar").is_none());
    }

    #[test]
    fn malformed_token_without_equals_fails_the_whole_line() {
        // `blah` has no `=` — the whole line is treated as ordinary
        // output, not a partial parse.
        assert!(parse_progress_line("::engram-warm:: event=start stage=x blah").is_none());
    }

    #[test]
    fn non_numeric_deadline_secs_fails_the_line() {
        assert!(
            parse_progress_line("::engram-warm:: event=start stage=x deadline_secs=soon").is_none()
        );
    }

    #[test]
    fn unrecognized_event_value_fails_the_line() {
        assert!(parse_progress_line("::engram-warm:: event=pause stage=x").is_none());
    }

    #[test]
    fn start_without_stage_fails_the_line() {
        assert!(parse_progress_line("::engram-warm:: event=start deadline_secs=10").is_none());
    }

    // ── OutputTail ──────────────────────────────────────────────

    #[test]
    fn output_tail_renders_pushed_bytes() {
        let mut tail = OutputTail::new(1024);
        tail.push(b"hello ");
        tail.push(b"world");
        assert_eq!(tail.render(), "hello world");
    }

    #[test]
    fn output_tail_drops_oldest_bytes_past_cap() {
        let mut tail = OutputTail::new(8);
        tail.push(b"12345678");
        tail.push(b"90"); // now 10 bytes pushed total, cap 8 -> drop-front
        assert_eq!(tail.render(), "34567890");
    }

    #[test]
    fn output_tail_handles_a_single_chunk_larger_than_cap() {
        let mut tail = OutputTail::new(4);
        tail.push(b"abcdefgh");
        assert_eq!(tail.render(), "efgh");
    }

    #[test]
    fn output_tail_lossy_renders_invalid_utf8() {
        let mut tail = OutputTail::new(16);
        tail.push(&[0xff, 0xfe, b'a', b'b']);
        assert!(tail.render().ends_with("ab"));
    }

    // ── WarmWatchdog ────────────────────────────────────────────

    fn cfg(stall_secs: u64, global_secs: u64) -> WarmWatchdogConfig {
        WarmWatchdogConfig {
            stall: Duration::from_secs(stall_secs),
            global_timeout: Duration::from_secs(global_secs),
        }
    }

    fn progress(ev: WarmEvent) -> WatchdogInput {
        WatchdogInput::Progress(WarmProgressLine { event: ev })
    }

    #[test]
    fn non_conforming_hook_never_stalls() {
        let t0 = Instant::now();
        let wall0 = Utc::now();
        let mut wd = WarmWatchdog::new(cfg(5, 3600), t0);
        // No progress line ever emitted — plain output only.
        assert!(wd.on_event(WatchdogInput::OutputBytes, t0, wall0).is_none());
        // Way past the 5s stall budget, but never armed (not conforming).
        let t_far = t0 + Duration::from_secs(1000);
        assert_eq!(
            wd.on_event(WatchdogInput::Tick, t_far, wall0),
            None,
            "a hook that never emits a progress line must not stall — only \
             the global timeout backstop applies"
        );
    }

    #[test]
    fn stall_arms_only_after_first_progress_line() {
        let t0 = Instant::now();
        let wall0 = Utc::now();
        let mut wd = WarmWatchdog::new(cfg(5, 3600), t0);
        assert!(!wd.is_conforming());
        let t1 = t0 + Duration::from_secs(1);
        wd.on_event(
            progress(WarmEvent::Start {
                stage: "boot".into(),
                deadline_secs: None,
                msg: None,
            }),
            t1,
            wall0,
        );
        assert!(wd.is_conforming());
        // Now silence past the stall budget triggers a violation.
        let t2 = t1 + Duration::from_secs(6);
        let violation = wd.on_event(WatchdogInput::Tick, t2, wall0);
        assert_eq!(violation, Some(WarmViolation::Stall));
        assert_eq!(violation.unwrap().kind(), CaptureFailureKind::WarmStall);
    }

    #[test]
    fn heartbeat_resets_stall_but_not_stage_deadline() {
        let t0 = Instant::now();
        let wall0 = Utc::now();
        let mut wd = WarmWatchdog::new(cfg(5, 3600), t0);
        wd.on_event(
            progress(WarmEvent::Start {
                stage: "wait-condition".into(),
                deadline_secs: Some(10),
                msg: None,
            }),
            t0,
            wall0,
        );
        // Heartbeat at t+3 keeps the stall clock alive well inside the 5s
        // stall budget.
        let t1 = t0 + Duration::from_secs(3);
        assert!(wd
            .on_event(
                progress(WarmEvent::Heartbeat {
                    stage: Some("wait-condition".into()),
                    msg: None,
                }),
                t1,
                wall0,
            )
            .is_none());
        // Another heartbeat at t+6 (3s after the last one — stall never
        // fires), but the STAGE deadline (10s from stage start) is a
        // separate, non-resettable budget.
        let t2 = t0 + Duration::from_secs(6);
        assert!(wd
            .on_event(
                progress(WarmEvent::Heartbeat {
                    stage: Some("wait-condition".into()),
                    msg: None,
                }),
                t2,
                wall0,
            )
            .is_none());
        // At t+11 the stage deadline (10s) has elapsed even though
        // heartbeats kept arriving well within the stall budget.
        let t3 = t0 + Duration::from_secs(11);
        let violation = wd.on_event(
            progress(WarmEvent::Heartbeat {
                stage: Some("wait-condition".into()),
                msg: None,
            }),
            t3,
            wall0,
        );
        assert_eq!(violation, Some(WarmViolation::StageDeadline));
    }

    #[test]
    fn start_implicitly_closes_the_prior_stage() {
        let t0 = Instant::now();
        let wall0 = Utc::now();
        let mut wd = WarmWatchdog::new(cfg(60, 3600), t0);
        wd.on_event(
            progress(WarmEvent::Start {
                stage: "deps-up".into(),
                deadline_secs: None,
                msg: None,
            }),
            t0,
            wall0,
        );
        let t1 = t0 + Duration::from_secs(5);
        let wall1 = wall0 + chrono::Duration::seconds(5);
        // No `done` for deps-up — the next `start` closes it implicitly.
        wd.on_event(
            progress(WarmEvent::Start {
                stage: "migrations".into(),
                deadline_secs: None,
                msg: None,
            }),
            t1,
            wall1,
        );
        let history = wd.stage_history();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].name, "deps-up");
        assert_eq!(history[0].outcome, WarmStageOutcome::Done);
        assert!(history[0].ended_at.is_some());
        assert_eq!(history[1].name, "migrations");
        assert_eq!(history[1].outcome, WarmStageOutcome::Running);
        assert!(history[1].ended_at.is_none());
    }

    #[test]
    fn global_timeout_has_supremacy_over_a_healthy_stage() {
        let t0 = Instant::now();
        let wall0 = Utc::now();
        // Stall budget generous (60s), stage deadline generous (60s), but
        // the global timeout is tight (10s).
        let mut wd = WarmWatchdog::new(cfg(60, 10), t0);
        wd.on_event(
            progress(WarmEvent::Start {
                stage: "healthy".into(),
                deadline_secs: Some(60),
                msg: None,
            }),
            t0,
            wall0,
        );
        // Heartbeat right before the global timeout — stage + stall both
        // look healthy, but the global timeout still fires.
        let t1 = t0 + Duration::from_secs(11);
        let violation = wd.on_event(
            progress(WarmEvent::Heartbeat {
                stage: Some("healthy".into()),
                msg: None,
            }),
            t1,
            wall0,
        );
        assert_eq!(violation, Some(WarmViolation::GlobalTimeout));
    }

    #[test]
    fn next_deadline_tracks_the_earliest_applicable_budget() {
        let t0 = Instant::now();
        let wall0 = Utc::now();
        let mut wd = WarmWatchdog::new(cfg(120, 3600), t0);
        // Not conforming yet: only the global timeout applies.
        assert_eq!(wd.next_deadline(), t0 + Duration::from_secs(3600));
        wd.on_event(
            progress(WarmEvent::Start {
                stage: "x".into(),
                deadline_secs: Some(30),
                msg: None,
            }),
            t0,
            wall0,
        );
        // Now conforming with a 30s stage deadline — that's the nearest.
        assert_eq!(wd.next_deadline(), t0 + Duration::from_secs(30));
    }

    #[test]
    fn done_then_silence_falls_back_to_stall_not_stage_deadline() {
        let t0 = Instant::now();
        let wall0 = Utc::now();
        let mut wd = WarmWatchdog::new(cfg(5, 3600), t0);
        wd.on_event(
            progress(WarmEvent::Start {
                stage: "final".into(),
                deadline_secs: Some(2),
                msg: None,
            }),
            t0,
            wall0,
        );
        // `done` closes the stage before its deadline elapses.
        let t1 = t0 + Duration::from_millis(500);
        assert!(wd
            .on_event(
                progress(WarmEvent::Done {
                    stage: "final".into()
                }),
                t1,
                wall0
            )
            .is_none());
        // No open stage anymore, so only the stall budget (5s) applies —
        // silence past it still fails the capture (the hook must exit).
        let t2 = t0 + Duration::from_secs(6);
        assert_eq!(
            wd.on_event(WatchdogInput::Tick, t2, wall0),
            Some(WarmViolation::Stall)
        );
    }

    #[test]
    fn finish_failed_marks_the_open_stage_failed() {
        let t0 = Instant::now();
        let wall0 = Utc::now();
        let mut wd = WarmWatchdog::new(cfg(5, 3600), t0);
        wd.on_event(
            progress(WarmEvent::Start {
                stage: "wedged".into(),
                deadline_secs: None,
                msg: None,
            }),
            t0,
            wall0,
        );
        let wall1 = wall0 + chrono::Duration::seconds(200);
        let history = wd.finish_failed(wall1);
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].name, "wedged");
        assert_eq!(history[0].outcome, WarmStageOutcome::Failed);
        assert_eq!(history[0].ended_at, Some(wall1));
    }
}
