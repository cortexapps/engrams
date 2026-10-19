//! engrams apikey … — admin management of global service-account keys
//! (ADR 0086). Your own CLI login keys are managed by `auth login/logout`;
//! this is the CI/service-key surface (mirrors /settings/api-keys).

use serde::Serialize;

use engram_app_client::proto::*;
use engram_app_client::Clients;

use crate::output::{fail, print_json, table, truncate, CliResult};

#[derive(Serialize)]
struct KeyJson {
    id: String,
    name: String,
    role: String,
    start: String,
    owner_email: String,
    created_at: String,
    expires_at: String,
    last_used_at: String,
}

fn key_json(k: ApiKeyMeta) -> KeyJson {
    KeyJson {
        id: k.id,
        name: k.name,
        role: k.role,
        start: k.start,
        owner_email: k.owner_email,
        created_at: k.created_at,
        expires_at: k.expires_at,
        last_used_at: k.last_used_at,
    }
}

pub async fn create(
    c: &Clients,
    name: &str,
    role: &str,
    expires_at: &str,
    json: bool,
) -> CliResult {
    if role != "admin" && role != "user" {
        return fail("--role must be \"admin\" or \"user\"");
    }
    let resp = c
        .create_api_key(CreateApiKeyRequest {
            name: name.to_string(),
            role: role.to_string(),
            expires_at: expires_at.to_string(),
        })
        .await?;
    let Some(meta) = resp.meta else {
        return fail("response carried no key metadata");
    };
    if json {
        #[derive(Serialize)]
        struct Out {
            #[serde(flatten)]
            meta: KeyJson,
            key: String,
        }
        print_json(&Out {
            meta: key_json(meta),
            key: resp.key,
        });
        return Ok(());
    }
    // The one-time plaintext goes to stdout ALONE so `$(engrams apikey create …)`
    // captures exactly the key; everything human goes to stderr.
    eprintln!("created key \"{name}\" (role={role}, id={})", meta.id);
    eprintln!("this is the only time the key is shown — store it now:");
    println!("{}", resp.key);
    Ok(())
}

pub async fn list(c: &Clients, json: bool) -> CliResult {
    let resp = c.list_api_keys(ListApiKeysRequest {}).await?;
    if json {
        #[derive(Serialize)]
        struct Out {
            keys: Vec<KeyJson>,
        }
        print_json(&Out {
            keys: resp.keys.into_iter().map(key_json).collect(),
        });
        return Ok(());
    }
    if resp.keys.is_empty() {
        println!("(no API keys)");
        return Ok(());
    }
    table(
        &["ID", "NAME", "ROLE", "KEY", "OWNER"],
        &resp
            .keys
            .iter()
            .map(|k| {
                vec![
                    k.id.clone(),
                    truncate(&k.name, 24),
                    k.role.clone(),
                    format!("{}…", k.start),
                    truncate(&k.owner_email, 32),
                ]
            })
            .collect::<Vec<_>>(),
        &[36, 24, 6, 14, 32],
    );
    Ok(())
}

pub async fn revoke(c: &Clients, id: &str, json: bool) -> CliResult {
    let resp = c
        .revoke_api_key(RevokeApiKeyRequest { id: id.to_string() })
        .await?;
    if json {
        #[derive(Serialize)]
        struct Out {
            revoked: bool,
        }
        print_json(&Out {
            revoked: resp.revoked,
        });
    } else {
        println!(
            "{}",
            if resp.revoked {
                "revoked"
            } else {
                "no such key (already revoked?)"
            }
        );
    }
    Ok(())
}
