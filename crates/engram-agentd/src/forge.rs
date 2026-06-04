//! ADR 0023 in-guest forge client.
//!
//! Invoked as a subcommand of the baked `engram-agentd` binary — the
//! dogfood image's `GIT_ASKPASS` helper runs `engram-agentd
//! forge-credential`, and `engram-pr` runs `engram-agentd
//! forge-pull-request`. We dial the host on `FORGE_VSOCK_PORT`, send a
//! [`ForgeRequest`] authenticated by the per-session broker token
//! (`ENGRAM_FORGE_TOKEN`), read one [`ForgeResponse`], and print the
//! result. The real credential never persists in the guest — it's
//! fetched fresh per git op, so token expiry is invisible here.

use std::process::ExitCode;

use engram_core::SessionId;
use engram_harness_proto::{
    read_msg, read_session_var, write_msg, ForgeOp, ForgeRequest, ForgeResponse, FORGE_VSOCK_PORT,
};

/// Entry from `main`: `sub` is `forge-credential` or
/// `forge-pull-request`; `rest` is the remaining argv. Builds a small
/// runtime and runs one request. On success prints the result to stdout
/// (the password for a credential; the PR URL for a change request).
pub fn run(sub: &str, rest: Vec<String>) -> ExitCode {
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("engram-agentd forge: runtime: {e}");
            return ExitCode::from(1);
        }
    };
    match rt.block_on(run_inner(sub, &rest)) {
        Ok(out) => {
            print!("{out}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("engram-agentd forge: {e}");
            ExitCode::from(1)
        }
    }
}

async fn run_inner(sub: &str, rest: &[String]) -> Result<String, String> {
    let session_id = env_session_id()?;
    // ADR 0037: file-first (warm-bind path), env-fallback (cold path). On a
    // warm-captured harness, claude's inherited env carries no real token.
    let broker_token = read_session_var("ENGRAM_FORGE_TOKEN")
        .ok_or_else(|| "ENGRAM_FORGE_TOKEN not set in the guest env".to_string())?;
    let op = match sub {
        "forge-credential" => ForgeOp::FetchCredential {
            host: flag(rest, "--host").unwrap_or_else(|| "github.com".to_string()),
            owner: flag(rest, "--owner").or_else(|| non_empty_session_var("ENGRAM_FORGE_OWNER")),
        },
        "forge-pull-request" => ForgeOp::CreatePullRequest {
            repo: req_flag(rest, "--repo")?,
            head_branch: req_flag(rest, "--head")?,
            base_branch: flag(rest, "--base").unwrap_or_else(|| "main".to_string()),
            title: req_flag(rest, "--title")?,
            body: flag(rest, "--body").unwrap_or_default(),
            draft: rest.iter().any(|a| a == "--draft"),
        },
        other => return Err(format!("unknown forge subcommand: {other}")),
    };

    let req = ForgeRequest {
        session_id,
        broker_token,
        op,
    };
    let transport = engram_transport::from_env().map_err(|e| format!("transport: {e}"))?;
    let mut conn = transport
        .dial(FORGE_VSOCK_PORT)
        .await
        .map_err(|e| format!("dial host forge port {FORGE_VSOCK_PORT}: {e}"))?;
    write_msg(&mut conn, &req)
        .await
        .map_err(|e| format!("send forge request: {e}"))?;
    let resp: ForgeResponse = read_msg(&mut conn)
        .await
        .map_err(|e| format!("read forge response: {e}"))?;
    match resp {
        // GIT_ASKPASS prints the password; the username comes from git
        // config (`credential.<host>.username = x-access-token`).
        ForgeResponse::Credential { password, .. } => Ok(password),
        ForgeResponse::PullRequest { url, .. } => Ok(format!("{url}\n")),
        ForgeResponse::Error { message } => Err(message),
    }
}

fn env_session_id() -> Result<SessionId, String> {
    let s = read_session_var("ENGRAM_SESSION_ID")
        .ok_or_else(|| "ENGRAM_SESSION_ID not set in the guest env".to_string())?;
    s.parse()
        .map_err(|_| format!("invalid ENGRAM_SESSION_ID: {s}"))
}

fn non_empty_session_var(name: &str) -> Option<String> {
    read_session_var(name).filter(|s| !s.is_empty())
}

/// Value following `name` in `argv`, if present.
fn flag(argv: &[String], name: &str) -> Option<String> {
    argv.iter()
        .position(|a| a == name)
        .and_then(|i| argv.get(i + 1))
        .cloned()
}

fn req_flag(argv: &[String], name: &str) -> Result<String, String> {
    flag(argv, name).ok_or_else(|| format!("missing required flag {name}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flag_parses_value_and_presence() {
        let argv: Vec<String> = ["--repo", "o/n", "--draft"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(flag(&argv, "--repo").as_deref(), Some("o/n"));
        assert!(flag(&argv, "--missing").is_none());
        assert!(argv.iter().any(|a| a == "--draft"));
        assert!(req_flag(&argv, "--repo").is_ok());
        assert!(req_flag(&argv, "--title").is_err());
    }
}
