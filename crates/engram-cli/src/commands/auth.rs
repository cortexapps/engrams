//! engrams auth login|logout|status — the `gh auth` model.
//!
//! login: RFC 8628 device flow against the orchestrator's better-auth
//! device-authorization plugin, then ONE exchange RPC:
//!   1. POST /api/auth/device/code {client_id} → user_code + verification_uri
//!   2. print the code, open the browser to the /device approval page
//!   3. poll POST /api/auth/device/token until approved → short-lived session
//!   4. ApiKeyService.CreateCliKey (authed by that session as a Bearer) →
//!      durable user-owned engk_… key → hosts.json (0600); session discarded.
//!
//! logout: RevokeCliKey (authed by the stored key itself) + drop the entry.
//! status: ApiKeyService.WhoAmI with the resolved key → who am I.

use std::io::BufRead;

use serde::Serialize;

use engram_app_client::config::{
    delete_credential, hosts_path, resolve_api_key, store_credential, stored_credential,
    HostCredential,
};
use engram_app_client::device_flow::{poll_token, request_grant, GrantError, PollError};
use engram_app_client::proto::*;
use engram_app_client::{Auth, Clients, Transport};

use crate::output::{fail, print_json, CliError, CliResult};

/// Fire-and-forget the browser opener; the URL is always printed too, so a
/// headless box (SSH, container) just falls back to copy/paste. `$BROWSER`,
/// when set, names the opener (so a test can point it at `true`).
fn open_browser(url: &str) {
    let cmd = std::env::var("BROWSER")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            if cfg!(target_os = "macos") {
                "open".to_string()
            } else if cfg!(windows) {
                "start".to_string()
            } else {
                "xdg-open".to_string()
            }
        });
    let _ = std::process::Command::new(cmd)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

fn env_api_key_set() -> bool {
    std::env::var("ENGRAMS_API_KEY")
        .map(|v| !v.is_empty())
        .unwrap_or(false)
}

fn hostname() -> String {
    #[cfg(unix)]
    {
        nix::unistd::gethostname()
            .map(|h| h.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "unknown".to_string())
    }
    #[cfg(not(unix))]
    {
        std::env::var("COMPUTERNAME").unwrap_or_else(|_| "unknown".to_string())
    }
}

pub async fn login(host: &str) -> CliResult {
    if env_api_key_set() {
        return fail(
            "ENGRAMS_API_KEY is set — it would shadow any stored login. Unset it to use `auth login`.",
        );
    }
    // A plain client for the non-RPC auth routes.
    let plain = Transport::new(host, Auth::None);
    let http = plain.http();

    // 1. Request the device grant.
    let grant = match request_grant(http, host).await {
        Ok(g) => g,
        Err(GrantError::Unreachable(msg)) => return fail(format!("cannot reach {host}: {msg}")),
        Err(GrantError::Rejected { status }) => {
            return fail(format!(
                "device authorization rejected (HTTP {status}) — is this an engrams host?"
            ))
        }
        Err(GrantError::Malformed(msg)) => return fail(msg),
    };

    // 2. Hand the human their code (gh's exact choreography).
    let url = grant.url().to_string();
    eprintln!("First, copy your one-time code: {}", grant.user_code);
    eprintln!("Then approve the CLI in your browser: {url}");
    crate::output::eprint_flush("Press Enter to open the browser…");
    let mut line = String::new();
    let _ = std::io::stdin().lock().read_line(&mut line);
    open_browser(&url);

    // 3. Poll for the approval.
    let access_token = match poll_token(http, host, &grant).await {
        Ok(t) => t,
        Err(PollError::Denied) => return fail("the request was denied in the browser"),
        Err(PollError::Expired) => {
            return fail("the one-time code expired — run `engrams auth login` again")
        }
        Err(PollError::Failed(e)) => return fail(format!("device token poll failed: {e}")),
        Err(PollError::Transport(msg)) => return fail(msg),
    };

    // 4. Exchange the short-lived session for the durable user-owned key.
    let exchange = Clients::new(Transport::new(host, Auth::Bearer(access_token)));
    let minted = exchange
        .create_cli_key(CreateCliKeyRequest {
            name: format!("cli:{}", hostname()),
        })
        .await?;

    // Who did we just become? (Also sanity-proves the minted key works.)
    let me = whoami(host, &minted.key).await;
    let email = me
        .as_ref()
        .map(|m| m.email.clone())
        .filter(|e| !e.is_empty());
    store_credential(
        host,
        HostCredential {
            api_key: minted.key,
            key_id: minted.meta.map(|m| m.id).unwrap_or_default(),
            user: email.clone(),
        },
    )?;
    eprintln!(
        "Logged in to {host} as {}",
        email.unwrap_or_else(|| "(unknown)".to_string())
    );
    eprintln!("Credential saved to {}", hosts_path().display());
    Ok(())
}

/// Resolve the identity a key acts as via ApiKeyService.WhoAmI. An RPC, not
/// better-auth's GET /api/auth/get-session: in prod that HTTP route stays
/// behind the login wall (it's the SPA's session bootstrap), while /rpc/* is
/// routed for keyed callers.
async fn whoami(host: &str, api_key: &str) -> Option<WhoAmIResponse> {
    Clients::new(Transport::new(host, Auth::ApiKey(api_key.to_string())))
        .who_am_i(WhoAmIRequest {})
        .await
        .ok()
}

pub async fn logout(host: &str) -> CliResult {
    let Some(stored) = stored_credential(host) else {
        return fail(format!("no stored login for {host}"));
    };
    // Best-effort server-side revoke, authenticated by the key being revoked.
    // A dead/already-revoked key must not wedge logout — drop the entry anyway.
    if !stored.key_id.is_empty() {
        let c = Clients::new(Transport::new(host, Auth::ApiKey(stored.api_key.clone())));
        if let Err(e) = c
            .revoke_cli_key(RevokeCliKeyRequest {
                id: stored.key_id.clone(),
            })
            .await
        {
            eprintln!(
                "engrams: warning: server-side revoke failed ({e}); removing the local credential anyway — revoke it in Settings → API keys"
            );
        }
    }
    delete_credential(host)?;
    eprintln!("Logged out of {host}");
    Ok(())
}

#[derive(Serialize)]
struct StatusJson<'a> {
    host: &'a str,
    #[serde(rename = "loggedIn")]
    logged_in: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    user: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<String>,
}

pub async fn status(host: &str, json: bool) -> CliResult {
    let key = resolve_api_key(host);
    let source = if env_api_key_set() {
        "ENGRAMS_API_KEY".to_string()
    } else {
        hosts_path().display().to_string()
    };
    let Some(key) = key else {
        if json {
            print_json(&StatusJson {
                host,
                logged_in: false,
                user: None,
                role: None,
                source: None,
            });
        } else {
            println!("{host}: not logged in — run `engrams auth login`");
        }
        return Err(CliError::Exit(1));
    };
    let me = whoami(host, &key).await;
    if json {
        print_json(&StatusJson {
            host,
            logged_in: me.is_some(),
            user: me.as_ref().map(|m| m.email.clone()),
            role: me.as_ref().map(|m| m.role.clone()),
            source: Some(source),
        });
        return if me.is_some() {
            Ok(())
        } else {
            Err(CliError::Exit(1))
        };
    }
    let Some(me) = me else {
        println!("{host}: stored credential is invalid or revoked (from {source})");
        return Err(CliError::Exit(1));
    };
    println!(
        "{host}: logged in as {} ({}) via {source}",
        me.email, me.role
    );
    Ok(())
}
