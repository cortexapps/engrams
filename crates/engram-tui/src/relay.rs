//! Program status reports from programs in the shell pane. The pane's
//! emulator is their terminal: it reads their OSC 7501 reports, checks
//! each one against the protocol's grammar and limits, and keeps the
//! records under `shell/` so the outer terminal shows them as children of
//! the TUI's own record. A report never reaches the outer terminal byte
//! for byte: every record is decoded and encoded again.
//!
//! The emulator also answers what a program asks before it reports: the
//! OSC 7501 detection query (only when the outer terminal takes reports)
//! and primary device attributes, the sentinel such a query relies on.

use crate::status::{encode_report, is_at_or_under, Kind, Report, State};

/// The records a pane keeps; the least recently updated goes first.
const MAX_RECORDS: usize = 16;
/// Limits from the protocol.
const MAX_SEQUENCE: usize = 4096;
const MAX_TITLE_ENCODED: usize = 256;
const MAX_TITLE: usize = 192;
const MAX_MSG_ENCODED: usize = 2732;
const MAX_MSG: usize = 2048;
const MAX_ID: usize = 128;
const MAX_KEY: usize = 16;
const MAX_APP: usize = 32;
const MAX_DEPTH: usize = 8;
/// The prefix every relayed id gets. It takes one of the eight levels.
const PREFIX: &str = "shell";

#[derive(Debug)]
struct Relayed {
    /// The id on the outer terminal (`shell`, `shell/<id>`).
    id: String,
    state: State,
    /// The report, encoded for the outer terminal.
    report: Vec<u8>,
}

/// The vt100 callbacks of one shell pane.
#[derive(Debug, Default)]
pub struct Relay {
    /// The outer terminal takes reports: answer the detection query.
    pub enabled: bool,
    records: Vec<Relayed>,
    /// Bumped on every change to `records`.
    rev: u64,
    /// Bytes to send back to the program (query replies).
    replies: Vec<u8>,
}

impl Relay {
    pub fn new(enabled: bool) -> Self {
        Relay {
            enabled,
            ..Default::default()
        }
    }

    pub fn rev(&self) -> u64 {
        self.rev
    }

    /// The records as (outer id, encoded report).
    pub fn records(&self) -> impl Iterator<Item = (&str, &[u8])> {
        self.records
            .iter()
            .map(|r| (r.id.as_str(), r.report.as_slice()))
    }

    /// Replies owed to the program since the last call.
    pub fn take_replies(&mut self) -> Option<Vec<u8>> {
        (!self.replies.is_empty()).then(|| std::mem::take(&mut self.replies))
    }

    /// The program ended, or a new shell prompt began: what was working
    /// or blocked is no longer true. Results stay until replaced.
    pub fn drop_transient(&mut self) {
        let before = self.records.len();
        self.records
            .retain(|r| !matches!(r.state, State::Working | State::Blocked(_)));
        if self.records.len() != before {
            self.rev += 1;
        }
    }

    /// A full reset (RIS) of the pane's terminal: every record goes.
    pub fn reset(&mut self) {
        if !self.records.is_empty() {
            self.records.clear();
            self.rev += 1;
        }
    }

    /// Apply one report body (what follows `OSC 7501 ;`).
    pub fn report(&mut self, body: &[u8]) {
        if body == b"?" {
            if self.enabled {
                self.replies.extend_from_slice(b"\x1b]7501;?\x1b\\");
            }
            return;
        }
        let Some(p) = parse(body) else {
            return;
        };
        let id = match p.id {
            Some(id) => format!("{PREFIX}/{id}"),
            None => PREFIX.to_string(),
        };
        let Some(state) = p.state else {
            // `clear`: the record and everything beneath it.
            let before = self.records.len();
            self.records.retain(|r| !is_at_or_under(&r.id, &id));
            if self.records.len() != before {
                self.rev += 1;
            }
            return;
        };
        let mut report = Vec::with_capacity(128);
        encode_report(
            &mut report,
            &Report {
                id: Some(&id),
                state,
                app: p.app,
                progress: p.progress,
                title: &p.title,
                msg: &p.msg,
            },
        );
        if let Some(pos) = self.records.iter().position(|r| r.id == id) {
            let r = self.records.remove(pos);
            if r.report == report {
                // Unchanged: keep its place as the most recent.
                self.records.push(r);
                return;
            }
        } else if self.records.len() >= MAX_RECORDS {
            self.records.remove(0);
        }
        self.records.push(Relayed { id, state, report });
        self.rev += 1;
    }
}

impl vt100::Callbacks for Relay {
    fn unhandled_osc(&mut self, _: &mut vt100::Screen, params: &[&[u8]]) {
        match params {
            // The body has no `;`: the value set excludes it.
            [b"7501", body] => self.report(body),
            // OSC 133 ; A: a new shell prompt.
            [b"133", mark, ..] if mark.first() == Some(&b'A') => self.drop_transient(),
            _ => {}
        }
    }

    fn unhandled_csi(
        &mut self,
        _: &mut vt100::Screen,
        i1: Option<u8>,
        _i2: Option<u8>,
        params: &[&[u16]],
        c: char,
    ) {
        // Primary device attributes (`CSI c`, `CSI 0 c`): a VT220 with
        // color, what most emulators answer.
        if c == 'c' && i1.is_none() && params.iter().all(|p| p.iter().all(|&v| v == 0)) {
            self.replies.extend_from_slice(b"\x1b[?62;22c");
        }
    }
}

/// A checked report. `state` is `None` for `clear`.
#[derive(Debug, PartialEq, Eq)]
struct Parsed<'a> {
    state: Option<State>,
    id: Option<&'a str>,
    app: Option<&'a str>,
    progress: Option<u8>,
    title: String,
    msg: String,
}

/// Parse a report body under the protocol's rules: malformed pairs are
/// skipped, unknown keys ignored, the last of a repeated key wins. Every
/// pair is checked before that, so a report with no recognized state, a
/// bad id, undecodable base64, a control character, or anything past a
/// limit, in any pair, is discarded whole.
fn parse(body: &[u8]) -> Option<Parsed<'_>> {
    // `OSC 7501 ;` and the terminator count toward the sequence limit.
    if body.len() + 10 > MAX_SEQUENCE {
        return None;
    }
    let body = std::str::from_utf8(body).ok()?;
    let (mut state, mut id, mut kind, mut progress, mut app) = (None, None, None, None, None);
    let (mut title, mut msg) = (String::new(), String::new());
    for pair in body.split(':') {
        let Some((key, value)) = pair.split_once('=') else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        if key.is_empty()
            || !key.bytes().all(|b| b.is_ascii_lowercase())
            || !value.bytes().all(is_value_byte)
        {
            continue;
        }
        // A key past its limit breaks the report, not just the pair.
        if key.len() > MAX_KEY {
            return None;
        }
        match key {
            "state" => state = Some(value),
            "id" if !valid_id(value) => return None,
            "id" => id = Some(value),
            "kind" => kind = Some(value),
            "progress" => progress = Some(value),
            "app" if value.len() > MAX_APP => return None,
            "app" => app = Some(value),
            "title" => title = decode_text(value, MAX_TITLE_ENCODED, MAX_TITLE)?,
            "msg" => msg = decode_text(value, MAX_MSG_ENCODED, MAX_MSG)?,
            _ => {}
        }
    }
    let state = match state? {
        "idle" => Some(State::Idle),
        "working" => Some(State::Working),
        "done" => Some(State::Done),
        "error" => Some(State::Error),
        "blocked" => Some(State::Blocked(match kind {
            Some("permission") => Kind::Permission,
            Some("question") => Kind::Question,
            Some("auth") => Kind::Auth,
            // Absent or unrecognized: the record has no kind.
            _ => Kind::Unstated,
        })),
        "clear" => None,
        _ => return None,
    };
    let progress = match state {
        Some(State::Working | State::Blocked(_)) => progress
            .and_then(|p| p.parse::<u8>().ok())
            .filter(|&p| p <= 100),
        _ => None,
    };
    // A value outside the character set is treated as absent.
    let app = app.filter(|a| !a.is_empty() && a.bytes().all(is_segment_byte));
    Some(Parsed {
        state,
        id,
        app,
        progress,
        title,
        msg,
    })
}

fn is_value_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"_.,+/=-".contains(&b)
}

fn is_segment_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"_.+-".contains(&b)
}

/// The id grammar, with room for the `shell/` prefix inside the limits.
fn valid_id(id: &str) -> bool {
    id.len() + PREFIX.len() < MAX_ID
        && id.split('/').count() < MAX_DEPTH
        && id
            .split('/')
            .all(|s| (1..=32).contains(&s.len()) && s.bytes().all(is_segment_byte))
}

/// Decode a base64 text value: `None` when it is too long, does not
/// decode, is not UTF-8, or holds a control character.
fn decode_text(value: &str, max_encoded: usize, max: usize) -> Option<String> {
    if value.len() > max_encoded {
        return None;
    }
    let bytes = decode_base64(value)?;
    if bytes.len() > max {
        return None;
    }
    let text = String::from_utf8(bytes).ok()?;
    if text.chars().any(char::is_control) {
        return None;
    }
    Some(text)
}

/// Standard base64; padding optional, but when present it is the
/// standard one or two `=` that complete the last group.
fn decode_base64(s: &str) -> Option<Vec<u8>> {
    let data = s.trim_end_matches('=');
    let pad = s.len() - data.len();
    if pad > 2 || (pad > 0 && !s.len().is_multiple_of(4)) {
        return None;
    }
    let s = data;
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut acc = 0u32;
    let mut bits = 0;
    for b in s.bytes() {
        let v = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        };
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    // A single leftover character cannot encode a byte.
    if s.len() % 4 == 1 {
        return None;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parser(r: Relay) -> vt100::Parser<Relay> {
        let n = |v| std::num::NonZeroU16::new(v).unwrap();
        vt100::Parser::new_with_callbacks(n(4), n(20), 0, r)
    }

    fn text(r: &Relay) -> Vec<(String, String)> {
        r.records()
            .map(|(id, rep)| (id.to_string(), String::from_utf8(rep.to_vec()).unwrap()))
            .collect()
    }

    #[test]
    fn a_report_is_checked_and_encoded_again_under_shell() {
        let mut r = Relay::new(true);
        // "Installing updates", padding left off.
        r.report(b"state=working:app=brew:progress=40:msg=SW5zdGFsbGluZyB1cGRhdGVz");
        assert_eq!(
            text(&r),
            vec![(
                "shell".to_string(),
                "\x1b]7501;state=working:id=shell:app=brew:progress=40:msg=SW5zdGFsbGluZyB1cGRhdGVz\x1b\\"
                    .to_string()
            )]
        );
    }

    #[test]
    fn a_child_id_nests_and_clear_removes_the_subtree() {
        let mut r = Relay::new(true);
        r.report(b"state=working:id=build");
        r.report(b"state=blocked:kind=permission:id=build/test");
        r.report(b"state=done:id=other");
        let ids: Vec<_> = r.records().map(|(id, _)| id.to_string()).collect();
        assert_eq!(ids, ["shell/build", "shell/build/test", "shell/other"]);
        r.report(b"state=clear:id=build");
        let ids: Vec<_> = r.records().map(|(id, _)| id.to_string()).collect();
        assert_eq!(ids, ["shell/other"]);
        r.report(b"state=clear");
        assert_eq!(r.records().count(), 0);
    }

    #[test]
    fn a_bad_report_is_discarded_whole() {
        let mut r = Relay::new(true);
        for body in [
            &b"state=sleeping"[..],
            b"msg=aGk=",
            b"state=idle:id=a//b",
            // "a\nb": a control character.
            b"state=idle:msg=YQpi",
            // In the value set, but not base64.
            b"state=idle:msg=a,b",
            b"state=idle:title=x",
        ] {
            r.report(body);
            assert_eq!(r.records().count(), 0, "{}", String::from_utf8_lossy(body));
        }
        // A malformed pair is skipped; the rest applies.
        r.report(b"state=idle:junk:app=bad app");
        assert_eq!(text(&r)[0].1, "\x1b]7501;state=idle:id=shell\x1b\\");
    }

    #[test]
    fn the_oldest_record_makes_room() {
        let mut r = Relay::new(true);
        for i in 0..(MAX_RECORDS + 3) {
            r.report(format!("state=working:id=r{i}").as_bytes());
        }
        assert_eq!(r.records().count(), MAX_RECORDS);
        assert_eq!(r.records().next().unwrap().0, "shell/r3");
    }

    #[test]
    fn a_prompt_or_an_exit_drops_working_and_blocked_but_keeps_results() {
        let mut r = Relay::new(true);
        r.report(b"state=working:id=a");
        r.report(b"state=blocked:id=b");
        r.report(b"state=done:id=c");
        let mut p = parser(r);
        p.process(b"\x1b]133;A\x07");
        let ids: Vec<_> = p
            .callbacks()
            .records()
            .map(|(id, _)| id.to_string())
            .collect();
        assert_eq!(ids, ["shell/c"]);
    }

    #[test]
    fn the_queries_are_answered() {
        let mut p = parser(Relay::new(true));
        p.process(b"\x1b]7501;?\x1b\\\x1b[c");
        assert_eq!(
            p.callbacks_mut().take_replies().unwrap(),
            b"\x1b]7501;?\x1b\\\x1b[?62;22c"
        );
        // Without support outside, only the device attributes.
        let mut p = parser(Relay::new(false));
        p.process(b"\x1b]7501;?\x07\x1b[0c");
        assert_eq!(p.callbacks_mut().take_replies().unwrap(), b"\x1b[?62;22c");
    }

    #[test]
    fn an_unchanged_report_is_not_a_change() {
        let mut r = Relay::new(true);
        r.report(b"state=working:id=a");
        let rev = r.rev();
        r.report(b"state=working:id=a");
        assert_eq!(r.rev(), rev);
    }

    #[test]
    fn a_pair_past_a_limit_discards_the_report_even_when_overwritten() {
        let mut r = Relay::new(true);
        r.report(b"state=done:id=keep");
        // An oversized title, then a valid one: the report is still bad,
        // so the clear does not happen.
        let mut body = b"state=clear:title=".to_vec();
        body.extend(std::iter::repeat_n(b'A', 260));
        body.extend_from_slice(b":title=");
        r.report(&body);
        assert_eq!(r.records().count(), 1);
        // A key past 16 bytes, an app past 32.
        r.report(b"state=clear:abcdefghijklmnopq=x");
        r.report(b"state=clear:app=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        assert_eq!(r.records().count(), 1);
        // A bad id in an earlier pair.
        r.report(b"state=clear:id=a//b:id=keep");
        assert_eq!(r.records().count(), 1);
    }

    #[test]
    fn base64_padding_is_optional_but_never_malformed() {
        assert_eq!(decode_base64("Zg==").unwrap(), b"f");
        assert_eq!(decode_base64("Zg").unwrap(), b"f");
        assert_eq!(decode_base64("Zm8=").unwrap(), b"fo");
        for bad in ["Zg======", "Zg=", "Z", "Z===", "Zg=a"] {
            assert!(decode_base64(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn a_kind_the_program_did_not_state_is_not_invented() {
        let mut r = Relay::new(true);
        r.report(b"state=blocked:id=a");
        r.report(b"state=blocked:id=b:kind=telepathy");
        r.report(b"state=blocked:id=c:kind=question");
        let reports: Vec<_> = text(&r).into_iter().map(|(_, rep)| rep).collect();
        assert!(!reports[0].contains("kind="));
        assert!(!reports[1].contains("kind="));
        assert!(reports[2].contains(":kind=question"));
    }
}
