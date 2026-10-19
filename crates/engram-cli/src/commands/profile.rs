//! engrams profile … — read surface of the native ProfileService (what a
//! `task create --profile` can reference). Profile AUTHORING stays in the
//! dashboard — the shapes (icons, env, secrets) are UI-first.

use std::collections::BTreeMap;

use serde::Serialize;

use engram_app_client::proto::*;
use engram_app_client::Clients;

use crate::output::{detail, fail, print_json, table, truncate, CliResult};

fn format_integration_grant(g: &ProfileIntegrationGrant) -> String {
    let operation = format!("{}:{}", g.connection_id, g.operation);
    if g.resource_constraints.is_empty() {
        operation
    } else {
        g.resource_constraints
            .iter()
            .map(|r| format!("{operation}@{r}"))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

#[derive(Serialize)]
struct GrantJson {
    connection_id: String,
    operation: String,
    resource_constraints: Vec<String>,
}

#[derive(Serialize)]
struct AppJson {
    name: String,
    port: u32,
}

#[derive(Serialize)]
struct ProfileJson {
    id: String,
    name: String,
    description: String,
    image_id: String,
    skills: Vec<String>,
    integration_grants: Vec<GrantJson>,
    // ADR 0118. Always present: a consumer writing `profile.apps ?? []`
    // cannot tell "no apps" from "field not serialized".
    apps: Vec<AppJson>,
    env_vars: BTreeMap<String, String>,
    archived: bool,
    created_at: String,
    updated_at: String,
}

fn profile_json(p: Profile) -> ProfileJson {
    ProfileJson {
        id: p.id,
        name: p.name,
        description: p.description,
        image_id: p.image_id,
        skills: p.skills,
        integration_grants: p
            .integration_grants
            .into_iter()
            .map(|g| GrantJson {
                connection_id: g.connection_id,
                operation: g.operation,
                resource_constraints: g.resource_constraints,
            })
            .collect(),
        apps: p
            .apps
            .into_iter()
            .map(|a| AppJson {
                name: a.name,
                port: a.port,
            })
            .collect(),
        env_vars: p.env_vars,
        archived: p.archived,
        created_at: p.created_at,
        updated_at: p.updated_at,
    }
}

pub async fn list(c: &Clients, json: bool) -> CliResult {
    let resp = c.list_profiles(ListProfilesRequest::default()).await?;
    let rows: Vec<Profile> = resp.profiles.into_iter().filter(|p| !p.archived).collect();
    if json {
        #[derive(Serialize)]
        struct Out {
            profiles: Vec<ProfileJson>,
        }
        print_json(&Out {
            profiles: rows.into_iter().map(profile_json).collect(),
        });
        return Ok(());
    }
    if rows.is_empty() {
        println!("(no profiles — create one in the dashboard under Settings → Profiles)");
        return Ok(());
    }
    table(
        &["ID", "NAME", "SKILLS", "DESCRIPTION"],
        &rows
            .iter()
            .map(|p| {
                vec![
                    p.id.clone(),
                    truncate(&p.name, 20),
                    truncate(&p.skills.join(","), 24),
                    truncate(&p.description, 40),
                ]
            })
            .collect::<Vec<_>>(),
        &[36, 20, 24, 40],
    );
    Ok(())
}

pub async fn get(c: &Clients, id: &str, json: bool) -> CliResult {
    let resp = c
        .get_profile(GetProfileRequest { id: id.to_string() })
        .await?;
    let Some(p) = resp.profile else {
        return fail("response carried no profile");
    };
    if json {
        print_json(&profile_json(p));
        return Ok(());
    }
    detail(&[
        ("id", Some(p.id.clone())),
        ("name", Some(p.name.clone())),
        ("description", Some(p.description.clone())),
        ("image_id", Some(p.image_id.clone())),
        ("skills", Some(p.skills.join(", "))),
        // ADR 0118: `<name>:<port>`, matching how the dashboard renders them.
        (
            "apps",
            Some(
                p.apps
                    .iter()
                    .map(|a| format!("{}:{}", a.name, a.port))
                    .collect::<Vec<_>>()
                    .join(", "),
            ),
        ),
        // NAMES only; `--json` carries the full map for anyone who asked.
        (
            "env_vars",
            Some(p.env_vars.keys().cloned().collect::<Vec<_>>().join(", ")),
        ),
        (
            "integration_grants",
            Some(
                p.integration_grants
                    .iter()
                    .map(format_integration_grant)
                    .collect::<Vec<_>>()
                    .join(", "),
            ),
        ),
        ("created_at", Some(p.created_at.clone())),
    ]);
    Ok(())
}
