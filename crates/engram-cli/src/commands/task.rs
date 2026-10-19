//! engrams task … — the product-level verbs (native TaskService).
//!
//! A task starts from a PROFILE (ADR 0053): `task create --profile <name|id>`
//! resolves the profile server-side to an image + skills + integration grants +
//! network policy — the same compilation the web's composer runs. Sessions
//! created here are attributed to the caller (their name in the dashboard).

use clap::Args;
use serde::Serialize;

use engram_app_client::proto::*;
use engram_app_client::Clients;

use crate::output::{detail, fail, print_json, table, truncate, CliResult};

#[derive(Serialize)]
struct TaskSessionJson {
    session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    image: Option<String>,
}

#[derive(Serialize)]
struct TaskJson {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    created_by_user_id: Option<String>,
    created_at: String,
    sessions: Vec<TaskSessionJson>,
}

fn task_json(t: Task) -> TaskJson {
    TaskJson {
        id: t.id,
        kind: t.r#type,
        title: t.title,
        status: t.status,
        created_by_user_id: t.created_by_user_id,
        created_at: t.created_at,
        sessions: t
            .sessions
            .into_iter()
            .map(|s| TaskSessionJson {
                session_id: s.session_id,
                role: s.role,
                status: s.session.as_ref().map(|x| x.status.clone()),
                image: s.session.map(|x| x.image),
            })
            .collect(),
    }
}

/// Accepts a profile id or (unique) name; resolves via ListProfiles.
async fn resolve_profile_id(c: &Clients, reference: &str) -> CliResult<String> {
    let resp = c.list_profiles(ListProfilesRequest::default()).await?;
    if let Some(p) = resp.profiles.iter().find(|p| p.id == reference) {
        return Ok(p.id.clone());
    }
    let by_name: Vec<&Profile> = resp
        .profiles
        .iter()
        .filter(|p| p.name == reference && !p.archived)
        .collect();
    match by_name.len() {
        1 => Ok(by_name[0].id.clone()),
        n if n > 1 => fail(format!(
            "profile name \"{reference}\" is ambiguous — pass the id"
        )),
        _ => fail(format!(
            "no profile \"{reference}\" — `engrams profile list` shows what's available"
        )),
    }
}

#[derive(Args, Debug)]
pub struct CreateOpts {
    /// profile to launch from
    #[arg(long, value_name = "name-or-id")]
    pub profile: String,
    /// initial prompt for the agent
    #[arg(long, value_name = "text")]
    pub prompt: Option<String>,
    /// task title (defaults from the prompt)
    #[arg(long, value_name = "text")]
    pub title: Option<String>,
    /// override the profile's harness
    #[arg(long, value_name = "name")]
    pub harness: Option<String>,
    /// override the profile's model
    #[arg(long, value_name = "name")]
    pub model: Option<String>,
    /// override the profile's reasoning effort
    #[arg(long, value_name = "level")]
    pub effort: Option<String>,
    /// session mode for the initial prompt (e.g. plan)
    #[arg(long, value_name = "mode")]
    pub mode: Option<String>,
}

pub async fn create(c: &Clients, opts: CreateOpts, json: bool) -> CliResult {
    let profile_id = resolve_profile_id(c, &opts.profile).await?;
    let resp = c
        .create_task(CreateTaskRequest {
            r#type: "chat".to_string(),
            profile_id,
            prompt: opts.prompt,
            title: opts.title,
            harness: opts.harness,
            model: opts.model,
            effort: opts.effort,
            harness_mode: opts.mode,
            ..Default::default()
        })
        .await?;
    let Some(t) = resp.task else {
        return fail("response carried no task");
    };
    if json {
        print_json(&task_json(t));
        return Ok(());
    }
    // The primary session id is what scripts want next (exec/logs/prompt).
    println!(
        "{}",
        t.sessions
            .first()
            .map(|s| s.session_id.clone())
            .unwrap_or(t.id)
    );
    Ok(())
}

pub async fn list(c: &Clients, json: bool) -> CliResult {
    let resp = c.list_tasks(ListTasksRequest::default()).await?;
    if json {
        #[derive(Serialize)]
        struct Out {
            tasks: Vec<TaskJson>,
        }
        print_json(&Out {
            tasks: resp.tasks.into_iter().map(task_json).collect(),
        });
        return Ok(());
    }
    if resp.tasks.is_empty() {
        println!("(no tasks)");
        return Ok(());
    }
    table(
        &["ID", "STATUS", "SESSION", "TITLE"],
        &resp
            .tasks
            .iter()
            .map(|t| {
                vec![
                    t.id.clone(),
                    t.status.clone(),
                    t.sessions
                        .first()
                        .map(|s| s.session_id.clone())
                        .unwrap_or_else(|| "-".into()),
                    truncate(t.title.as_deref().unwrap_or(""), 48),
                ]
            })
            .collect::<Vec<_>>(),
        &[36, 10, 36, 48],
    );
    Ok(())
}

pub async fn get(c: &Clients, id: &str, json: bool) -> CliResult {
    let resp = c
        .get_task(GetTaskRequest {
            task_id: id.to_string(),
            ..Default::default()
        })
        .await?;
    let Some(t) = resp.task else {
        return fail("response carried no task");
    };
    if json {
        print_json(&task_json(t));
        return Ok(());
    }
    detail(&[
        ("id", Some(t.id)),
        ("type", Some(t.r#type)),
        ("status", Some(t.status)),
        ("title", t.title),
        ("created_by", t.created_by_user_id),
        ("created_at", Some(t.created_at)),
    ]);
    for s in &t.sessions {
        println!(
            "session         : {} ({}, {})",
            s.session_id,
            s.role.as_deref().unwrap_or("primary"),
            s.session.as_ref().map(|x| x.status.as_str()).unwrap_or("?")
        );
    }
    Ok(())
}

pub async fn remove(c: &Clients, id: &str) -> CliResult {
    c.delete_task(DeleteTaskRequest {
        task_id: id.to_string(),
    })
    .await?;
    println!("deleted");
    Ok(())
}
