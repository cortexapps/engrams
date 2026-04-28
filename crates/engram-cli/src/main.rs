//! Ops/admin CLI. Talks to the coordinator's HTTP API for session
//! lifecycle operations; invokes the `engram-image-builder` library
//! directly for `image build` (it doesn't go through the coordinator).
//! Implementations land progressively as the surface grows.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use chrono::Utc;
use clap::{Parser, Subcommand};
use engram_image_builder::{BuildRequest, Builder, DockerCli, Format};
use serde_json::Value;

#[derive(Parser, Debug)]
#[command(name = "engram", version, about = "Engram ops CLI")]
struct Cli {
    #[arg(long, env = "ENGRAM_ENDPOINT", default_value = "http://localhost:8080")]
    endpoint: String,

    /// Bearer token for the coordinator's protected endpoints. When
    /// the coordinator runs without `ENGRAM_AUTH_TOKENS` (dev mode),
    /// leave this unset.
    #[arg(long, env = "ENGRAM_TOKEN")]
    token: Option<String>,

    /// Print raw JSON instead of the human-readable view. Off by
    /// default so an interactive terminal stays readable; tooling
    /// (jq pipelines, tests) opts in.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Operations on sessions.
    Session {
        #[command(subcommand)]
        cmd: SessionCmd,
    },
    /// Operations on hosts.
    Host {
        #[command(subcommand)]
        cmd: HostCmd,
    },
    /// Operations on warm images.
    Image {
        #[command(subcommand)]
        cmd: ImageCmd,
    },
}

#[derive(Subcommand, Debug)]
enum SessionCmd {
    /// List sessions in `pending` / `active` / `idle` status.
    List,
    /// Print one session's row.
    Get { id: String },
    /// Mark the session completed and tear down its sandbox.
    Delete { id: String },
    /// Tail the persistent event log via SSE. Stays open; Ctrl-C to
    /// stop. `--since N` resumes after a given idx (matches
    /// `?since=N` on the events endpoint).
    Logs {
        id: String,
        #[arg(long)]
        since: Option<i64>,
    },
}

#[derive(Subcommand, Debug)]
enum HostCmd {
    List,
    Drain { id: String },
}

#[derive(Subcommand, Debug)]
enum ImageCmd {
    List {
        repo: String,
    },
    /// Bake an image from a Dockerfile + engram.toml in the source repo.
    Build {
        /// Repo identifier (e.g. `cortex/api`). Determines the registry
        /// path the produced image lands at.
        #[arg(long)]
        repo: String,

        /// Path to the source repo. Default `.`.
        #[arg(long, default_value = ".")]
        source: PathBuf,

        /// Override the produced tag. Default `warm-<rfc3339>`.
        #[arg(long)]
        tag: Option<String>,

        /// Where the registry lives — must match the coordinator's
        /// `<storage_local_path>/images`.
        #[arg(
            long,
            env = "ENGRAM_IMAGES_DIR",
            default_value = "./var/snapshots/images"
        )]
        images_dir: PathBuf,

        /// Override the docker binary (e.g. `podman`).
        #[arg(long, env = "ENGRAM_DOCKER_BIN")]
        docker_bin: Option<String>,

        /// Output format. `directory` for the dev backend (Process),
        /// `ext4` for Firecracker. Defaults to `directory`.
        #[arg(long, value_parser = parse_image_format, default_value = "directory")]
        format: Format,
    },
}

fn parse_image_format(s: &str) -> Result<Format, String> {
    match s {
        "directory" | "dir" => Ok(Format::Directory),
        "ext4" => Ok(Format::Ext4),
        other => Err(format!(
            "unknown format `{other}`; expected `directory` or `ext4`"
        )),
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();
    let cli = Cli::parse();

    match run(&cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(CliError::NotImplemented(what)) => {
            eprintln!("engram-cli: `{what}` is not implemented yet.");
            ExitCode::from(2)
        }
        Err(CliError::Http(status, body)) => {
            eprintln!("engram-cli: server returned {status}: {body}");
            ExitCode::from(1)
        }
        Err(CliError::Other(e)) => {
            eprintln!("engram-cli: {e}");
            ExitCode::from(1)
        }
    }
}

#[derive(Debug)]
enum CliError {
    NotImplemented(String),
    Http(u16, String),
    Other(String),
}

impl<E: std::error::Error> From<E> for CliError {
    fn from(e: E) -> Self {
        Self::Other(e.to_string())
    }
}

async fn run(cli: &Cli) -> Result<(), CliError> {
    let client = build_client(cli.token.as_deref())?;
    match &cli.cmd {
        Cmd::Session { cmd } => match cmd {
            SessionCmd::List => session_list(&client, &cli.endpoint, cli.json).await,
            SessionCmd::Get { id } => session_get(&client, &cli.endpoint, id, cli.json).await,
            SessionCmd::Delete { id } => session_delete(&client, &cli.endpoint, id).await,
            SessionCmd::Logs { id, since } => {
                session_logs(&client, &cli.endpoint, id, *since).await
            }
        },
        Cmd::Image { cmd } => match cmd {
            ImageCmd::Build {
                repo,
                source,
                tag,
                images_dir,
                docker_bin,
                format,
            } => {
                image_build(
                    repo,
                    source,
                    tag.as_deref(),
                    images_dir,
                    docker_bin.as_deref(),
                    *format,
                )
                .await
            }
            ImageCmd::List { .. } => Err(CliError::NotImplemented(
                "image list — coordinator endpoint not yet exposed".into(),
            )),
        },
        Cmd::Host { cmd } => Err(CliError::NotImplemented(format!(
            "host {cmd:?} — multi-host scheduling lands in Phase 3"
        ))),
    }
}

// ---- session subcommands ------------------------------------------------

async fn session_list(
    client: &reqwest::Client,
    endpoint: &str,
    json: bool,
) -> Result<(), CliError> {
    let body = get_json(client, &format!("{endpoint}/sessions")).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&body)?);
        return Ok(());
    }
    let empty = Vec::new();
    let sessions = body["sessions"].as_array().unwrap_or(&empty);
    if sessions.is_empty() {
        println!("(no active sessions)");
        return Ok(());
    }
    // Plain columnar layout: header + rows. Avoids pulling a TUI dep
    // for what's effectively two-line output most of the time.
    println!("{:<36}  {:<10}  {:<24}  BRANCH", "ID", "STATUS", "REPO");
    for s in sessions {
        println!(
            "{:<36}  {:<10}  {:<24}  {}",
            s["id"].as_str().unwrap_or(""),
            s["status"].as_str().unwrap_or(""),
            truncate(s["repo"].as_str().unwrap_or(""), 24),
            s["branch"].as_str().unwrap_or(""),
        );
    }
    Ok(())
}

async fn session_get(
    client: &reqwest::Client,
    endpoint: &str,
    id: &str,
    json: bool,
) -> Result<(), CliError> {
    let body = get_json(client, &format!("{endpoint}/sessions/{id}")).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&body)?);
        return Ok(());
    }
    println!("id           : {}", body["id"].as_str().unwrap_or(""));
    println!("status       : {}", body["status"].as_str().unwrap_or(""));
    println!("repo         : {}", body["repo"].as_str().unwrap_or(""));
    println!("branch       : {}", body["branch"].as_str().unwrap_or(""));
    println!(
        "image_version: {}",
        body["image_version"].as_str().unwrap_or(""),
    );
    if let Some(uid) = body["user_id"].as_str() {
        println!("user_id      : {uid}");
    }
    println!(
        "created_at   : {}",
        body["created_at"].as_str().unwrap_or(""),
    );
    println!(
        "last_active  : {}",
        body["last_active_at"].as_str().unwrap_or(""),
    );
    Ok(())
}

async fn session_delete(
    client: &reqwest::Client,
    endpoint: &str,
    id: &str,
) -> Result<(), CliError> {
    let resp = client
        .delete(format!("{endpoint}/sessions/{id}"))
        .send()
        .await?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(CliError::Http(status.as_u16(), body));
    }
    println!("deleted");
    Ok(())
}

async fn session_logs(
    client: &reqwest::Client,
    endpoint: &str,
    id: &str,
    since: Option<i64>,
) -> Result<(), CliError> {
    use futures::StreamExt;

    let mut url = format!("{endpoint}/sessions/{id}/events");
    if let Some(s) = since {
        url.push_str(&format!("?since={s}"));
    }
    let resp = client
        .get(url)
        .header("accept", "text/event-stream")
        .send()
        .await?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(CliError::Http(status.as_u16(), body));
    }

    // Minimal SSE parser: split on "\n\n" frame boundaries; print
    // `event:` + `data:` per frame. Server-sent comments (`:`-prefix
    // keep-alives) are ignored. We rely on the body being closed
    // upstream (or the user Ctrl-C'ing) to end the stream.
    let mut buf = String::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let bytes = chunk?;
        buf.push_str(&String::from_utf8_lossy(&bytes));
        while let Some(idx) = buf.find("\n\n") {
            let frame = buf[..idx].to_string();
            buf.drain(..idx + 2);
            print_sse_frame(&frame);
        }
    }
    Ok(())
}

fn print_sse_frame(frame: &str) {
    if let Some(line) = format_sse_frame(frame) {
        println!("{line}");
    }
}

/// Render one SSE frame as a single human-readable line. `None` for
/// frames that carry only a comment / keep-alive (no `event:` or
/// `data:` field). Pulled out of `print_sse_frame` so it's unit-testable.
fn format_sse_frame(frame: &str) -> Option<String> {
    let mut event: Option<String> = None;
    let mut id: Option<String> = None;
    let mut data_lines: Vec<&str> = Vec::new();
    for line in frame.lines() {
        if line.starts_with(':') {
            continue;
        }
        if let Some(rest) = line.strip_prefix("event:") {
            event = Some(rest.trim().to_string());
        } else if let Some(rest) = line.strip_prefix("id:") {
            id = Some(rest.trim().to_string());
        } else if let Some(rest) = line.strip_prefix("data:") {
            data_lines.push(rest.trim_start_matches(' '));
        }
    }
    if event.is_none() && data_lines.is_empty() {
        return None;
    }
    let event = event.unwrap_or_else(|| "message".to_string());
    let data = data_lines.join("\n");
    let parsed: Value = serde_json::from_str(&data).unwrap_or(Value::String(data));
    let id_str = id.as_deref().unwrap_or("-");
    Some(format!("[{id_str:>6}] {event}: {parsed}"))
}

// ---- image subcommand ---------------------------------------------------

async fn image_build(
    repo: &str,
    source: &Path,
    tag: Option<&str>,
    images_dir: &Path,
    docker_bin: Option<&str>,
    format: Format,
) -> Result<(), CliError> {
    let resolved_tag = tag
        .map(str::to_string)
        .unwrap_or_else(|| format!("warm-{}", Utc::now().format("%Y%m%dT%H%M%SZ")));
    let req = BuildRequest {
        source: source.to_path_buf(),
        repo: repo.to_string(),
        tag: resolved_tag.clone(),
        images_dir: images_dir.to_path_buf(),
        format,
    };
    let docker = match docker_bin {
        Some(bin) => DockerCli::with_binary(bin.to_string()),
        None => DockerCli::new(),
    };
    let builder = Builder::new(docker);
    let outcome = builder
        .build(&req)
        .await
        .map_err(|e| CliError::Other(e.to_string()))?;
    println!(
        "{repo} {tag} -> {dir} ({size} bytes)",
        repo = repo,
        tag = resolved_tag,
        dir = outcome.image_dir.display(),
        size = outcome.size_bytes,
    );
    Ok(())
}

// ---- shared helpers -----------------------------------------------------

fn build_client(token: Option<&str>) -> Result<reqwest::Client, CliError> {
    let mut builder = reqwest::Client::builder();
    if let Some(t) = token {
        // Default header travels on every request (incl. SSE GETs).
        // Errors here mean the token has illegal header bytes —
        // surface that early instead of letting reqwest 4xx for us.
        let mut headers = reqwest::header::HeaderMap::new();
        let value = reqwest::header::HeaderValue::from_str(&format!("Bearer {t}"))
            .map_err(|e| CliError::Other(format!("invalid token for header: {e}")))?;
        headers.insert(reqwest::header::AUTHORIZATION, value);
        builder = builder.default_headers(headers);
    }
    builder
        .build()
        .map_err(|e| CliError::Other(format!("http client: {e}")))
}

async fn get_json(client: &reqwest::Client, url: &str) -> Result<Value, CliError> {
    let resp = client.get(url).send().await?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(CliError::Http(status.as_u16(), body));
    }
    serde_json::from_str(&body).map_err(|e| CliError::Other(format!("invalid JSON: {e}")))
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_passes_short_strings_through() {
        assert_eq!(truncate("hi", 10), "hi");
        assert_eq!(truncate("", 10), "");
    }

    #[test]
    fn truncate_appends_ellipsis_when_too_long() {
        // Counts characters, not bytes. "abcdef" with max=4 yields
        // "abc…" (3 chars + ellipsis = 4 visible columns). Avoids the
        // classic UTF-8 boundary bug from byte-based truncation.
        assert_eq!(truncate("abcdef", 4), "abc…");
    }

    #[test]
    fn truncate_handles_multibyte_chars() {
        // Each emoji is one char; max=2 should leave one + ellipsis.
        let s = "🦀🦀🦀";
        assert_eq!(truncate(s, 2), "🦀…");
    }

    #[test]
    fn format_sse_frame_pretty_prints_event_id_data() {
        let frame = "event:exec_completed\nid:7\ndata:{\"exec_id\":\"x\",\"exit_status\":0}";
        let out = format_sse_frame(frame).unwrap();
        assert!(out.contains("exec_completed"), "got {out}");
        assert!(out.contains("\"exec_id\":\"x\""), "got {out}");
        assert!(
            out.contains("[     7]"),
            "id should be right-aligned: {out}"
        );
    }

    #[test]
    fn format_sse_frame_defaults_event_to_message() {
        let frame = "data:hello";
        let out = format_sse_frame(frame).unwrap();
        // No `event:` field -> default per the SSE spec.
        assert!(out.contains("message"), "got {out}");
    }

    #[test]
    fn format_sse_frame_skips_keepalive_comments() {
        // axum's keep-alive emits `: keep-alive\n\n` — there's no
        // event or data, just a comment line. Filtering at the
        // formatter stops every keep-alive from polluting the
        // user's terminal.
        assert!(format_sse_frame(": keep-alive").is_none());
        assert!(format_sse_frame(":").is_none());
    }

    #[test]
    fn format_sse_frame_treats_non_json_data_as_string() {
        let out = format_sse_frame("event:raw\ndata:not-json").unwrap();
        assert!(out.contains("\"not-json\""), "got {out}");
    }

    #[test]
    fn format_sse_frame_uses_dash_for_missing_id() {
        let out = format_sse_frame("event:x\ndata:1").unwrap();
        assert!(out.contains("[     -]"), "got {out}");
    }
}
