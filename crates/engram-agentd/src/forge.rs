//! ADR 0023 in-guest forge client.
//!
//! Invoked as a subcommand of the baked `engram-agentd` binary — the
//! `GIT_ASKPASS` helper runs `engram-agentd forge-credential [--host H]
//! [--username]`. We dial the host on `FORGE_VSOCK_PORT`, send a
//! [`ForgeRequest`] authenticated by the per-session broker token
//! (`ENGRAM_FORGE_TOKEN`), read one [`ForgeResponse`], and print the
//! credential's password, or its username with `--username`. The real
//! credential never persists in the guest — it's fetched fresh per git op,
//! so token expiry is invisible here.
//!
//! With no `--host`, the request names no host and the coordinator mints
//! for the session's only forge.

use std::process::ExitCode;

use engram_core::SessionId;
use engram_harness_proto::{
    read_msg, write_msg, ForgeOp, ForgeRequest, ForgeResponse, FORGE_VSOCK_PORT,
};

/// Entry from `main`: `sub` is `forge-credential`; `rest` is the
/// remaining argv. Builds a small runtime and runs one request. On
/// success prints the git credential password to stdout.
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
    let broker_token = std::env::var("ENGRAM_FORGE_TOKEN")
        .map_err(|_| "ENGRAM_FORGE_TOKEN not set in the guest env".to_string())?;
    let (op, want_username) = credential_request(sub, rest, non_empty_env("ENGRAM_FORGE_OWNER"))?;

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
        ForgeResponse::Credential { username, .. } if want_username => Ok(username),
        ForgeResponse::Credential { password, .. } => Ok(password),
        ForgeResponse::OAuthCredential { .. } | ForgeResponse::OAuthCredentialBroken => {
            Err("host returned OAuth credential to git helper".into())
        }
        ForgeResponse::Error { message } => Err(message),
    }
}

/// The forge op for `sub` and its argv, and whether to print the username
/// rather than the password. An empty host names no host.
fn credential_request(
    sub: &str,
    rest: &[String],
    env_owner: Option<String>,
) -> Result<(ForgeOp, bool), String> {
    match sub {
        "forge-credential" => Ok((
            ForgeOp::FetchCredential {
                host: flag(rest, "--host").unwrap_or_default(),
                owner: flag(rest, "--owner").or(env_owner),
            },
            rest.iter().any(|arg| arg == "--username"),
        )),
        other => Err(format!("unknown forge subcommand: {other}")),
    }
}

fn env_session_id() -> Result<SessionId, String> {
    let s = std::env::var("ENGRAM_SESSION_ID")
        .map_err(|_| "ENGRAM_SESSION_ID not set in the guest env".to_string())?;
    s.parse()
        .map_err(|_| format!("invalid ENGRAM_SESSION_ID: {s}"))
}

fn non_empty_env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|s| !s.is_empty())
}

/// Value following `name` in `argv`, if present.
fn flag(argv: &[String], name: &str) -> Option<String> {
    argv.iter()
        .position(|a| a == name)
        .and_then(|i| argv.get(i + 1))
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flag_parses_value_and_presence() {
        let argv: Vec<String> = ["--host", "github.com", "--owner", "cortexapps"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(flag(&argv, "--host").as_deref(), Some("github.com"));
        assert_eq!(flag(&argv, "--owner").as_deref(), Some("cortexapps"));
        assert!(flag(&argv, "--missing").is_none());
    }

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_credential_request_names_no_host_unless_asked() {
        let (op, want_username) = credential_request("forge-credential", &argv(&[]), None).unwrap();
        assert!(!want_username);
        let ForgeOp::FetchCredential { host, owner } = op else {
            panic!("expected FetchCredential");
        };
        assert_eq!(host, "");
        assert_eq!(owner, None);

        let (op, want_username) = credential_request(
            "forge-credential",
            &argv(&["--username", "--host", "gitlab.example.com"]),
            Some("acme".into()),
        )
        .unwrap();
        assert!(want_username);
        let ForgeOp::FetchCredential { host, owner } = op else {
            panic!("expected FetchCredential");
        };
        assert_eq!(host, "gitlab.example.com");
        assert_eq!(owner.as_deref(), Some("acme"));

        assert!(credential_request("forge-pull-request", &argv(&[]), None).is_err());
    }
}
