//! The session list: rows from `TaskService.ListTasks`, state markers that
//! carry meaning without color, and a subsequence filter for the palette.

use engram_app_client::proto::{Profile, Task};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionRow {
    pub session_id: String,
    pub title: String,
    pub status: String,
    /// Seconds since the last activity, when the timestamp parsed.
    pub age_secs: Option<u64>,
}

/// The marker for a session state (ADR 0124: meaning without color).
pub fn marker(status: &str) -> &'static str {
    match status {
        "active" => "●",
        "pending" | "queued" | "created" | "evacuating" => "◐",
        "idle" => "○",
        "parked" => "‖",
        "failed" | "dead" | "host_lost" => "✕",
        "evicting" => "⤓",
        "unreachable" => "…",
        "completed" => "✓",
        _ => "·",
    }
}

/// Rows from a task listing, one per task's primary session.
pub fn rows_from_tasks(tasks: &[Task], now_unix: u64) -> Vec<SessionRow> {
    tasks
        .iter()
        .filter_map(|t| {
            let s = t.sessions.first()?;
            let session = s.session.as_ref();
            let title = t
                .title
                .clone()
                .filter(|t| !t.is_empty())
                .or_else(|| session.and_then(|s| s.suggested_title.clone()))
                .unwrap_or_else(|| s.session_id.clone());
            let status = session
                .map(|s| s.status.clone())
                .unwrap_or_else(|| "dead".to_string());
            let age_secs = session
                .and_then(|s| {
                    s.last_event_at
                        .as_deref()
                        .or(Some(s.last_active_at.as_str()))
                        .and_then(parse_rfc3339)
                })
                .map(|t| now_unix.saturating_sub(t));
            Some(SessionRow {
                session_id: s.session_id.clone(),
                title,
                status,
                age_secs,
            })
        })
        .collect()
}

/// A profile the new-session picker offers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProfileRow {
    pub id: String,
    pub name: String,
    pub description: String,
}

/// Rows from a profile listing, in the server's order.
pub fn rows_from_profiles(profiles: &[Profile]) -> Vec<ProfileRow> {
    profiles
        .iter()
        .map(|p| ProfileRow {
            id: p.id.clone(),
            name: if p.name.is_empty() {
                p.id.clone()
            } else {
                p.name.clone()
            },
            description: p.description.clone(),
        })
        .collect()
}

/// `2m`, `3h`, `4d`: a compact age.
pub fn age(secs: Option<u64>) -> String {
    match secs {
        None => String::new(),
        Some(s) if s < 60 => format!("{s}s"),
        Some(s) if s < 3600 => format!("{}m", s / 60),
        Some(s) if s < 86_400 => format!("{}h", s / 3600),
        Some(s) => format!("{}d", s / 86_400),
    }
}

/// Seconds since the epoch of an RFC 3339 timestamp (`Z` or `±hh:mm`).
/// Fractional seconds are ignored. `None` when the shape is wrong.
pub fn parse_rfc3339(s: &str) -> Option<u64> {
    let b = s.as_bytes();
    if b.len() < 19
        || b[4] != b'-'
        || b[7] != b'-'
        || b[10] != b'T' && b[10] != b't' && b[10] != b' '
    {
        return None;
    }
    let num = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, sec) = (num(11..13)?, num(14..16)?, num(17..19)?);
    let mut i = 19;
    if b.get(i) == Some(&b'.') {
        i += 1;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
    }
    let offset = match b.get(i) {
        Some(b'Z') | Some(b'z') | None => 0,
        Some(sign @ (b'+' | b'-')) => {
            let oh = num(i + 1..i + 3)?;
            let om = num(i + 4..i + 6)?;
            let o = oh * 3600 + om * 60;
            if *sign == b'+' {
                o
            } else {
                -o
            }
        }
        _ => return None,
    };
    // Days from civil (Howard Hinnant's algorithm).
    let (y2, mo2) = if mo <= 2 {
        (y - 1, mo + 9)
    } else {
        (y, mo - 3)
    };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let doy = (153 * mo2 + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let t = days * 86_400 + h * 3600 + mi * 60 + sec - offset;
    u64::try_from(t).ok()
}

/// Case-insensitive subsequence match for the palette.
pub fn fuzzy(query: &str, hay: &str) -> bool {
    let mut q = query.chars().flat_map(char::to_lowercase).peekable();
    for c in hay.chars().flat_map(char::to_lowercase) {
        match q.peek() {
            Some(&want) if want == c => {
                q.next();
            }
            Some(_) => {}
            None => return true,
        }
    }
    q.peek().is_none()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_epochs() {
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_rfc3339("2026-07-10T00:01:00Z"), Some(1_783_641_660));
        assert_eq!(
            parse_rfc3339("2026-07-10T00:01:00.123456+01:00"),
            Some(1_783_641_660 - 3600)
        );
        assert_eq!(parse_rfc3339("garbage"), None);
    }

    #[test]
    fn ages_and_fuzzy() {
        assert_eq!(age(Some(59)), "59s");
        assert_eq!(age(Some(3600 * 5)), "5h");
        assert_eq!(age(None), "");
        assert!(fuzzy("fxb", "fix the bug"));
        assert!(!fuzzy("z", "fix the bug"));
        assert!(fuzzy("", "anything"));
    }
}
