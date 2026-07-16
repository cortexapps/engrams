use std::borrow::Cow;

use engram_harness_proto::HarnessEvent;

const INTENT_ENV: &str = "ENGRAM_BROWSER_INTENT";
const MAX_INTENT_CHARS: usize = 160;

/// Enrich a native shell tool call with the browser-domain event consumed by
/// transcript presenters. The original ToolCallStarted still flows after this
/// event; both carry the same id so clients can replace rather than duplicate
/// the generic shell card.
pub fn from_tool_call(event: &HarnessEvent) -> Option<HarnessEvent> {
    let HarnessEvent::ToolCallStarted {
        run_id,
        tool_call_id,
        tool_name,
        args_summary: Some(args),
    } = event
    else {
        return None;
    };
    if !matches!(tool_name.as_str(), "Bash" | "Shell") {
        return None;
    }

    let command = shell_command(args);
    let intent = activity_intent(&command)?;
    Some(HarnessEvent::BrowserActivity {
        run_id: run_id.clone(),
        tool_call_id: tool_call_id.clone(),
        intent,
    })
}

fn shell_command(summary: &str) -> Cow<'_, str> {
    serde_json::from_str::<serde_json::Value>(summary)
        .ok()
        .and_then(|value| value.get("command")?.as_str().map(str::to_owned))
        .map(Cow::Owned)
        .unwrap_or(Cow::Borrowed(summary))
}

fn activity_intent(command: &str) -> Option<String> {
    for segment in shell_segments(command) {
        if let Some((intent, cli_index)) = browser_invocation(&segment) {
            return Some(
                intent
                    .and_then(sanitize_intent)
                    .unwrap_or_else(|| fallback_intent(&segment[cli_index..])),
            );
        }
    }
    None
}

fn browser_invocation(tokens: &[String]) -> Option<(Option<String>, usize)> {
    let mut i = 0;
    let mut intent = None;
    if tokens.first().is_some_and(|token| token == "env") {
        i += 1;
    }
    while let Some(token) = tokens.get(i) {
        if let Some((name, value)) = assignment(token) {
            if name == INTENT_ENV {
                intent = Some(value.to_owned());
            }
            i += 1;
            continue;
        }
        if matches!(token.as_str(), "command" | "exec") {
            i += 1;
            continue;
        }
        break;
    }
    let executable = tokens.get(i)?.rsplit('/').next()?;
    matches!(executable, "playwright-cli" | "agent-browser").then_some((intent, i))
}

fn assignment(token: &str) -> Option<(&str, &str)> {
    let (name, value) = token.split_once('=')?;
    let mut chars = name.chars();
    let first = chars.next()?;
    if !(first == '_' || first.is_ascii_alphabetic())
        || !chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
    {
        return None;
    }
    Some((name, value))
}

fn sanitize_intent(raw: String) -> Option<String> {
    let clean = raw
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_INTENT_CHARS)
        .collect::<String>();
    (!clean.is_empty()).then_some(clean)
}

fn fallback_intent(invocation: &[String]) -> String {
    let command = invocation.get(1).map(String::as_str).unwrap_or("");
    let target = invocation
        .get(2)
        .map(String::as_str)
        .filter(|s| !s.starts_with('-'));
    match (command, target) {
        ("open" | "goto", Some(target)) => format!("Navigating to {target}"),
        ("open" | "goto", None) => "Navigating browser".into(),
        ("click", Some(target)) => format!("Clicking {target}"),
        ("click", None) => "Clicking page control".into(),
        ("fill" | "type", Some(target)) => format!("Filling {target}"),
        ("fill" | "type", None) => "Entering text".into(),
        ("select" | "check" | "uncheck", Some(target)) => format!("Updating {target}"),
        ("press", Some(target)) => format!("Pressing {target}"),
        ("snapshot" | "read", _) => "Inspecting page".into(),
        ("screenshot", _) => "Capturing browser screenshot".into(),
        ("wait", _) => "Waiting for page".into(),
        ("tab", _) => "Switching browser tab".into(),
        ("eval" | "run-code", _) => "Running browser script".into(),
        _ => "Using browser".into(),
    }
}

/// Minimal shell lexer: enough to preserve quoted intent strings and split
/// command lists without pretending to execute shell expansions.
fn shell_segments(command: &str) -> Vec<Vec<String>> {
    let mut segments = Vec::new();
    let mut segment = Vec::new();
    let mut token = String::new();
    let mut quote = None;
    let mut escaped = false;
    let mut chars = command.chars().peekable();

    while let Some(c) = chars.next() {
        if escaped {
            token.push(c);
            escaped = false;
            continue;
        }
        if c == '\\' && quote != Some('\'') {
            escaped = true;
            continue;
        }
        if let Some(q) = quote {
            if c == q {
                quote = None;
            } else {
                token.push(c);
            }
            continue;
        }
        if matches!(c, '\'' | '"') {
            quote = Some(c);
        } else if c.is_whitespace() {
            push_token(&mut segment, &mut token);
            if c == '\n' {
                push_segment(&mut segments, &mut segment);
            }
        } else if matches!(c, ';' | '|') || (c == '&' && chars.peek() == Some(&'&')) {
            push_token(&mut segment, &mut token);
            if matches!(c, '|' | '&') && chars.peek() == Some(&c) {
                chars.next();
            }
            push_segment(&mut segments, &mut segment);
        } else {
            token.push(c);
        }
    }
    if escaped {
        token.push('\\');
    }
    push_token(&mut segment, &mut token);
    push_segment(&mut segments, &mut segment);
    segments
}

fn push_token(segment: &mut Vec<String>, token: &mut String) {
    if !token.is_empty() {
        segment.push(std::mem::take(token));
    }
}

fn push_segment(segments: &mut Vec<Vec<String>>, segment: &mut Vec<String>) {
    if !segment.is_empty() {
        segments.push(std::mem::take(segment));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn started(tool: &str, args: &str) -> HarnessEvent {
        HarnessEvent::ToolCallStarted {
            run_id: "run-1".into(),
            tool_call_id: "tool-1".into(),
            tool_name: tool.into(),
            args_summary: Some(args.into()),
        }
    }

    #[test]
    fn extracts_agent_intent_from_codex_shell_command() {
        let event = started(
            "Shell",
            r#"ENGRAM_BROWSER_INTENT="Clicking Sign in" playwright-cli click e7"#,
        );
        assert!(matches!(
            from_tool_call(&event),
            Some(HarnessEvent::BrowserActivity { intent, .. }) if intent == "Clicking Sign in"
        ));
    }

    #[test]
    fn extracts_agent_intent_from_claude_bash_json() {
        let event = started(
            "Bash",
            r#"{"command":"ENGRAM_BROWSER_INTENT='Navigating to dashboard' playwright-cli open http://localhost:3000"}"#,
        );
        assert!(matches!(
            from_tool_call(&event),
            Some(HarnessEvent::BrowserActivity { intent, .. }) if intent == "Navigating to dashboard"
        ));
    }

    #[test]
    fn derives_fallback_and_ignores_mentions() {
        assert!(matches!(
            from_tool_call(&started("Shell", "playwright-cli snapshot")),
            Some(HarnessEvent::BrowserActivity { intent, .. }) if intent == "Inspecting page"
        ));
        assert!(from_tool_call(&started("Shell", "rg playwright-cli README.md")).is_none());
        assert!(from_tool_call(&started("Read", "playwright-cli snapshot")).is_none());
    }

    #[test]
    fn recognizes_chained_browser_command_at_command_boundary() {
        let event = started(
            "Bash",
            "cd /workspace && ENGRAM_BROWSER_INTENT='Checking app' /usr/local/bin/playwright-cli snapshot",
        );
        assert!(matches!(
            from_tool_call(&event),
            Some(HarnessEvent::BrowserActivity { intent, .. }) if intent == "Checking app"
        ));
    }
}
