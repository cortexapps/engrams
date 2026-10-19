//! engrams image … / engrams registry … — ImageService passthrough verbs.
//!
//! An image's config is a structured message (name, description, env, workdir,
//! resources, and the warm hook). The dashboard's enable dialog is the full
//! editor; the CLI covers the fields a script needs as flags and leaves the
//! warm hook to the dashboard. STRICT validation stays server-side.

use std::collections::BTreeMap;
use std::io::Read;
use std::time::Duration;

use clap::Args;
use serde::Serialize;

use engram_app_client::proto::*;
use engram_app_client::Clients;

use crate::output::{
    detail, eprint_flush, fail, js_quote, pad_end, print_json, table, truncate, CliResult,
};
use crate::ConfigFlags;

// ---- config flags → proto ---------------------------------------------------

/// `--env` is a repeatable option with an empty-list default, so it is always
/// "set": every config-flag check passes, as it did before.
fn has_config_flags(_f: &ConfigFlags) -> bool {
    true
}

fn positive(flag: &str, v: Option<&str>) -> CliResult<Option<u32>> {
    let Some(v) = v else { return Ok(None) };
    let ok =
        v.starts_with(|c: char| ('1'..='9').contains(&c)) && v.chars().all(|c| c.is_ascii_digit());
    match v.parse::<u32>() {
        Ok(n) if ok => Ok(Some(n)),
        _ => fail(format!(
            "--{flag} must be a positive integer, got {}",
            js_quote(v)
        )),
    }
}

fn non_negative(flag: &str, v: Option<&str>) -> CliResult<Option<u32>> {
    let Some(v) = v else { return Ok(None) };
    let ok = !v.is_empty() && v.chars().all(|c| c.is_ascii_digit());
    match v.parse::<u32>() {
        Ok(n) if ok => Ok(Some(n)),
        _ => fail(format!(
            "--{flag} must be a non-negative integer, got {}",
            js_quote(v)
        )),
    }
}

fn parse_env(entries: &[String]) -> CliResult<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    for e in entries {
        match e.find('=') {
            Some(i) if i > 0 => {
                out.insert(e[..i].to_string(), e[i + 1..].to_string());
            }
            _ => return fail(format!("--env expects KEY=VALUE, got {}", js_quote(e))),
        }
    }
    Ok(out)
}

/// Build the config to send. With a `base` (the stored config of an enabled
/// image) the flags overlay it: `--env` adds or overrides keys, the warm hook
/// passes through untouched. Without a base this is a first enable, and the
/// server requires a name and a vCPU count.
fn build_image_config(flags: &ConfigFlags, base: Option<ImageConfig>) -> CliResult<ImageConfig> {
    let base = base.unwrap_or_default();
    let mut env = base.env;
    env.extend(parse_env(&flags.env)?);
    let name = flags
        .name
        .clone()
        .filter(|s| !s.is_empty())
        .unwrap_or(base.name);
    if name.is_empty() {
        return fail("--name is required on first enable");
    }
    let base_res = base.resources.unwrap_or_default();
    let vcpus = positive("vcpus", flags.vcpus.as_deref())?.or(base_res.suggested_vcpus);
    if vcpus.unwrap_or(0) == 0 {
        return fail("--vcpus is required on first enable (placement reserves it)");
    }
    Ok(ImageConfig {
        name,
        description: flags.description.clone().or(base.description),
        env,
        workdir: flags.workdir.clone().or(base.workdir),
        resources: Some(ImageResources {
            suggested_vcpus: vcpus,
            suggested_memory_mib: positive("memory-mib", flags.memory_mib.as_deref())?
                .or(base_res.suggested_memory_mib),
            suggested_disk_gib: positive("disk-gib", flags.disk_gib.as_deref())?
                .or(base_res.suggested_disk_gib),
            suggested_swap_mib: non_negative("swap-mib", flags.swap_mib.as_deref())?
                .or(base_res.suggested_swap_mib),
        }),
        warm: base.warm,
    })
}

// ---- enable-job rendering -------------------------------------------------

#[derive(Serialize)]
struct JobJson {
    id: String,
    image_uri: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    manifest_digest: Option<String>,
    state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    chunks_total: Option<u32>,
    chunks_done: u32,
    attempts: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    created_at: String,
    updated_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    capture_phase: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    warm_stage: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    warm_stage_started_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output_tail: Option<String>,
    prestage_hosts: serde_json::Value,
}

fn job_json(job: EnableJob) -> JobJson {
    JobJson {
        id: job.id,
        image_uri: job.image_uri,
        manifest_digest: job.manifest_digest,
        state: job.state,
        chunks_total: job.chunks_total,
        chunks_done: job.chunks_done,
        attempts: job.attempts,
        error: job.error,
        created_at: job.created_at,
        updated_at: job.updated_at,
        capture_phase: job.capture_phase,
        warm_stage: job.warm_stage,
        warm_stage_started_at: job.warm_stage_started_at,
        output_tail: job.output_tail,
        prestage_hosts: parse_or(&job.prestage_hosts),
    }
}

fn parse_or(s: &str) -> serde_json::Value {
    serde_json::from_str(s).unwrap_or_else(|_| serde_json::Value::Object(Default::default()))
}

fn job_progress(job: &EnableJob) -> String {
    if let Some(total) = job.chunks_total.filter(|t| *t > 0) {
        return format!("{}/{total} chunks", job.chunks_done);
    }
    [&job.capture_phase, &job.warm_stage]
        .into_iter()
        .filter_map(|s| s.as_deref().filter(|s| !s.is_empty()))
        .collect::<Vec<_>>()
        .join(" / ")
}

fn print_job(job: &EnableJob) {
    detail(&[
        ("id", Some(job.id.clone())),
        ("state", Some(job.state.clone())),
        ("image_uri", Some(job.image_uri.clone())),
        ("manifest_digest", job.manifest_digest.clone()),
        ("progress", Some(job_progress(job))),
        ("attempts", Some(job.attempts.to_string())),
        ("error", job.error.clone()),
        ("capture_phase", job.capture_phase.clone()),
        ("warm_stage", job.warm_stage.clone()),
        ("warm_started_at", job.warm_stage_started_at.clone()),
        ("created_at", Some(job.created_at.clone())),
        ("updated_at", Some(job.updated_at.clone())),
    ]);
    if !job.prestage_hosts.is_empty() && job.prestage_hosts != "{}" {
        println!("prestage_hosts  : {}", job.prestage_hosts);
    }
    if let Some(tail) = job.output_tail.as_deref().filter(|t| !t.is_empty()) {
        println!("\noutput_tail:\n{tail}");
    }
}

/// Poll an enable job until ready/failed, rendering a progress line.
async fn poll_job(c: &Clients, job_id: &str, json: bool) -> CliResult {
    let mut printed_progress = false;
    loop {
        let resp = c
            .get_enable_job(GetEnableJobRequest {
                job_id: job_id.to_string(),
            })
            .await?;
        let Some(job) = resp.job else {
            return fail("get-enable-job carried no job");
        };
        if job.state == "ready" {
            if printed_progress {
                eprint_flush("\n");
            }
            if json {
                print_json(&job_json(job));
            } else {
                println!(
                    "enabled: uri={} digest={}",
                    job.image_uri,
                    job.manifest_digest.as_deref().unwrap_or("")
                );
            }
            return Ok(());
        }
        if job.state == "failed" {
            if printed_progress {
                eprint_flush("\n");
            }
            return fail(format!(
                "enable job {job_id} failed: {} (retry with `engrams image retry-job {job_id}` after fixing the cause)",
                job.error.as_deref().unwrap_or("unknown error")
            ));
        }
        eprint_flush(&format!(
            "\r{} {}",
            pad_end(&job.state, 14),
            pad_end(&job_progress(&job), 24)
        ));
        printed_progress = true;
        tokio::time::sleep(Duration::from_millis(2000)).await;
    }
}

// ---- image verbs ----------------------------------------------------------

async fn find_enabled(c: &Clients, uri: &str) -> CliResult<EnabledImageSummary> {
    let resp = c.list_enabled_images(ListEnabledImagesRequest {}).await?;
    match resp.images.into_iter().find(|i| i.image_uri == uri) {
        Some(img) => Ok(img),
        None => fail(format!("no enabled image with uri {uri}")),
    }
}

#[derive(Serialize)]
struct ResourcesJson {
    #[serde(skip_serializing_if = "Option::is_none")]
    suggested_vcpus: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    suggested_memory_mib: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    suggested_disk_gib: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    suggested_swap_mib: Option<u32>,
}

#[derive(Serialize)]
struct WarmEnvJson {
    name: String,
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    value: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    secret_ref: Option<String>,
}

#[derive(Serialize)]
struct NetworkJson {
    default: String,
    allow_hosts: Vec<String>,
    allow_host_patterns: Vec<String>,
}

#[derive(Serialize)]
struct WarmJson {
    command: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    timeout_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    workdir: Option<String>,
    env: Vec<WarmEnvJson>,
    #[serde(skip_serializing_if = "Option::is_none")]
    network: Option<NetworkJson>,
}

#[derive(Serialize)]
struct ConfigJson {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    env: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    workdir: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    resources: Option<ResourcesJson>,
    #[serde(skip_serializing_if = "Option::is_none")]
    warm: Option<WarmJson>,
}

/// Print an enabled image's stored config, the row the dashboard edits, as JSON.
pub async fn config(c: &Clients, uri: &str) -> CliResult {
    let img = find_enabled(c, uri).await?;
    let Some(cfg) = img.config else {
        return fail(format!("enabled image {uri} has no stored config"));
    };
    print_json(&ConfigJson {
        name: cfg.name,
        description: cfg.description,
        env: cfg.env,
        workdir: cfg.workdir,
        resources: cfg.resources.map(|r| ResourcesJson {
            suggested_vcpus: r.suggested_vcpus,
            suggested_memory_mib: r.suggested_memory_mib,
            suggested_disk_gib: r.suggested_disk_gib,
            suggested_swap_mib: r.suggested_swap_mib,
        }),
        warm: cfg.warm.map(|w| WarmJson {
            command: w.command,
            timeout_secs: w.timeout_secs,
            workdir: w.workdir,
            env: w
                .env
                .into_iter()
                .map(|e| match e.value {
                    Some(capture_env_entry::Value::Literal(v)) => WarmEnvJson {
                        name: e.name,
                        kind: "literal",
                        value: Some(v),
                        secret_ref: None,
                    },
                    Some(capture_env_entry::Value::SecretRef(r)) => WarmEnvJson {
                        name: e.name,
                        kind: "secret_ref",
                        value: None,
                        secret_ref: Some(r),
                    },
                    None => WarmEnvJson {
                        name: e.name,
                        kind: "secret_ref",
                        value: None,
                        secret_ref: None,
                    },
                })
                .collect(),
            network: w.network.map(|n| NetworkJson {
                default: n.default,
                allow_hosts: n.allow_hosts,
                allow_host_patterns: n.allow_host_patterns,
            }),
        }),
    });
    Ok(())
}

pub async fn list(c: &Clients, json: bool) -> CliResult {
    let resp = c.list_enabled_images(ListEnabledImagesRequest {}).await?;
    if json {
        #[derive(Serialize)]
        struct Img {
            id: String,
            image_uri: String,
            manifest_digest: String,
            #[serde(skip_serializing_if = "Option::is_none")]
            name: Option<String>,
            #[serde(skip_serializing_if = "Option::is_none")]
            description: Option<String>,
            last_refreshed_at: String,
            created_at: String,
        }
        #[derive(Serialize)]
        struct Out {
            images: Vec<Img>,
        }
        print_json(&Out {
            images: resp
                .images
                .into_iter()
                .map(|img| Img {
                    id: img.id,
                    image_uri: img.image_uri,
                    manifest_digest: img.manifest_digest,
                    name: img.config.as_ref().map(|c| c.name.clone()),
                    description: img.config.and_then(|c| c.description),
                    last_refreshed_at: img.last_refreshed_at,
                    created_at: img.created_at,
                })
                .collect(),
        });
        return Ok(());
    }
    if resp.images.is_empty() {
        println!("(no images enabled — `engrams image enable --uri <uri> --name <name> --vcpus <n>` to add one)");
        return Ok(());
    }
    table(
        &["URI", "NAME", "DIGEST"],
        &resp
            .images
            .iter()
            .map(|img| {
                vec![
                    img.image_uri.clone(),
                    img.config
                        .as_ref()
                        .map(|c| c.name.clone())
                        .unwrap_or_else(|| "?".into()),
                    img.manifest_digest.clone(),
                ]
            })
            .collect::<Vec<_>>(),
        &[48, 22, 71],
    );
    Ok(())
}

pub async fn enable(
    c: &Clients,
    uri: &str,
    flags: &ConfigFlags,
    no_wait: bool,
    json: bool,
) -> CliResult {
    // No config flags on an already-enabled URI re-uses the stored config.
    let config = if has_config_flags(flags) {
        Some(build_image_config(flags, None)?)
    } else {
        None
    };
    let resp = c
        .enable_image(EnableImageRequest {
            image_uri: uri.to_string(),
            config,
        })
        .await?;
    let Some(job) = resp.job else {
        return fail("enable response carried no job");
    };
    if no_wait {
        if json {
            print_json(&job_json(job));
        } else {
            println!(
                "enable job {} accepted; poll with `engrams image poll-job {}`",
                job.id, job.id
            );
        }
        return Ok(());
    }
    poll_job(c, &job.id, json).await
}

pub async fn update(
    c: &Clients,
    uri: &str,
    flags: &ConfigFlags,
    allow_recapture: bool,
    no_wait: bool,
    json: bool,
) -> CliResult {
    if !has_config_flags(flags) {
        return fail("nothing to change: pass at least one config flag");
    }
    let img = find_enabled(c, uri).await?;
    let config = build_image_config(flags, img.config)?;
    let resp = c
        .update_image(UpdateImageRequest {
            image_uri: uri.to_string(),
            config: Some(config),
            allow_recapture,
        })
        .await?;
    let Some(job) = resp.job else {
        if json {
            #[derive(Serialize)]
            struct Out {
                applied: &'static str,
            }
            print_json(&Out {
                applied: "immediate",
            });
        } else {
            println!("config updated (no recapture needed; live immediately)");
        }
        return Ok(());
    };
    if no_wait {
        if json {
            print_json(&job_json(job));
        } else {
            println!("recapture job {} accepted", job.id);
        }
        return Ok(());
    }
    poll_job(c, &job.id, json).await
}

pub async fn disable(c: &Clients, uri: &str) -> CliResult {
    c.disable_image(DisableImageRequest {
        image_uri: uri.to_string(),
    })
    .await?;
    println!("disabled");
    Ok(())
}

pub async fn refresh(c: &Clients, uri: &str, recapture: bool, json: bool) -> CliResult {
    let resp = c
        .refresh_image(RefreshImageRequest {
            image_uri: uri.to_string(),
            force_recapture: recapture,
        })
        .await?;
    let Some(job) = resp.job else {
        return fail("refresh response carried no job");
    };
    poll_job(c, &job.id, json).await
}

pub async fn jobs(c: &Clients, json: bool) -> CliResult {
    let resp = c.list_enable_jobs(ListEnableJobsRequest {}).await?;
    if json {
        #[derive(Serialize)]
        struct Out {
            jobs: Vec<JobJson>,
        }
        print_json(&Out {
            jobs: resp.jobs.into_iter().map(job_json).collect(),
        });
        return Ok(());
    }
    if resp.jobs.is_empty() {
        println!("(no recent enable jobs)");
        return Ok(());
    }
    table(
        &["ID", "STATE", "ATTEMPTS", "DETAIL", "URI"],
        &resp
            .jobs
            .iter()
            .map(|job| {
                let detail = if job.state == "failed" {
                    job.error.clone().unwrap_or_else(|| "failed".into())
                } else {
                    job_progress(job)
                };
                vec![
                    job.id.clone(),
                    job.state.clone(),
                    job.attempts.to_string(),
                    truncate(&detail, 22),
                    truncate(&job.image_uri, 64),
                ]
            })
            .collect::<Vec<_>>(),
        &[36, 13, 8, 22, 64],
    );
    Ok(())
}

pub async fn job(c: &Clients, id: &str, json: bool) -> CliResult {
    let resp = c
        .get_enable_job(GetEnableJobRequest {
            job_id: id.to_string(),
        })
        .await?;
    let Some(job) = resp.job else {
        return fail("get-enable-job response carried no job");
    };
    if json {
        print_json(&job_json(job));
    } else {
        print_job(&job);
    }
    Ok(())
}

pub async fn poll_job_verb(c: &Clients, id: &str, json: bool) -> CliResult {
    poll_job(c, id, json).await
}

pub async fn retry_job(c: &Clients, id: &str, no_wait: bool, json: bool) -> CliResult {
    let resp = c
        .retry_enable_job(RetryEnableJobRequest {
            job_id: id.to_string(),
        })
        .await?;
    let Some(job) = resp.job else {
        return fail("retry-enable-job response carried no job");
    };
    if no_wait {
        if json {
            print_json(&job_json(job));
        } else {
            println!(
                "enable job {} requeued; poll with `engrams image poll-job {}`",
                job.id, job.id
            );
        }
        return Ok(());
    }
    poll_job(c, &job.id, json).await
}

// ---- registry verbs -------------------------------------------------------

#[derive(Args, Debug)]
pub struct RegistryAddOpts {
    /// registry host, e.g. ghcr.io
    #[arg(long, value_name = "host")]
    pub host: String,
    /// static | gcp-workload-identity
    #[arg(long, value_name = "kind", default_value = "static")]
    pub auth_kind: String,
    /// static: registry username
    #[arg(long, value_name = "name")]
    pub username: Option<String>,
    /// static: file containing the password/PAT
    #[arg(long, value_name = "path")]
    pub password_file: Option<String>,
    /// static: read the password from stdin
    #[arg(long)]
    pub password_stdin: bool,
    /// gcp-workload-identity: impersonate this SA
    #[arg(long, value_name = "email")]
    pub impersonate_sa: Option<String>,
}

fn strip_one_newline(mut s: String) -> String {
    if s.ends_with('\n') {
        s.pop();
    }
    s
}

pub async fn registry_add(c: &Clients, opts: RegistryAddOpts, json: bool) -> CliResult {
    let auth = match opts.auth_kind.as_str() {
        "static" => {
            let Some(username) = opts.username else {
                return fail("--auth-kind static requires --username");
            };
            let password = if let Some(path) = opts.password_file {
                strip_one_newline(std::fs::read_to_string(path)?)
            } else if opts.password_stdin {
                let mut s = String::new();
                std::io::stdin().lock().read_to_string(&mut s)?;
                strip_one_newline(s)
            } else {
                return fail("static auth requires --password-file <path> or --password-stdin");
            };
            if password.is_empty() {
                return fail("password is empty");
            }
            add_registry_request::Auth::Static(StaticRegistryAuth { username, password })
        }
        "gcp-workload-identity" | "gcp_workload_identity" => {
            add_registry_request::Auth::GcpWorkloadIdentity(GcpWorkloadIdentityRegistryAuth {
                impersonate_sa: opts.impersonate_sa,
            })
        }
        other => {
            return fail(format!(
                "unknown --auth-kind `{other}` (expected: static | gcp-workload-identity)"
            ))
        }
    };
    let resp = c
        .add_registry(AddRegistryRequest {
            host: opts.host,
            auth: Some(auth),
        })
        .await?;
    if json {
        #[derive(Serialize)]
        struct Out {
            id: String,
            host: String,
            auth_kind: String,
            #[serde(skip_serializing_if = "Option::is_none")]
            auth_principal: Option<String>,
        }
        print_json(&Out {
            id: resp.id,
            host: resp.host,
            auth_kind: resp.auth_kind,
            auth_principal: resp.auth_principal,
        });
    } else {
        println!(
            "added: host={} auth_kind={} principal={}",
            resp.host,
            resp.auth_kind,
            resp.auth_principal.as_deref().unwrap_or("(none)")
        );
    }
    Ok(())
}

pub async fn registry_list(c: &Clients, json: bool) -> CliResult {
    let resp = c.list_registries(ListRegistriesRequest {}).await?;
    if json {
        #[derive(Serialize)]
        struct Reg {
            id: String,
            registry_host: String,
            auth_kind: String,
            #[serde(skip_serializing_if = "Option::is_none")]
            auth_principal: Option<String>,
            created_at: String,
            #[serde(skip_serializing_if = "Option::is_none")]
            updated_at: Option<String>,
        }
        #[derive(Serialize)]
        struct Out {
            registries: Vec<Reg>,
        }
        print_json(&Out {
            registries: resp
                .registries
                .into_iter()
                .map(|r| Reg {
                    id: r.id,
                    registry_host: r.registry_host,
                    auth_kind: r.auth_kind,
                    auth_principal: r.auth_principal,
                    created_at: r.created_at,
                    updated_at: r.updated_at,
                })
                .collect(),
        });
        return Ok(());
    }
    if resp.registries.is_empty() {
        println!("(no registries configured)");
        return Ok(());
    }
    table(
        &["HOST", "AUTH_KIND", "PRINCIPAL"],
        &resp
            .registries
            .iter()
            .map(|r| {
                vec![
                    r.registry_host.clone(),
                    r.auth_kind.clone(),
                    r.auth_principal.clone().unwrap_or_else(|| "(none)".into()),
                ]
            })
            .collect::<Vec<_>>(),
        &[40, 24, 40],
    );
    Ok(())
}

pub async fn registry_rm(c: &Clients, host: &str) -> CliResult {
    c.delete_registry(DeleteRegistryRequest {
        host: host.to_string(),
    })
    .await?;
    println!("removed");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flags() -> ConfigFlags {
        ConfigFlags::default()
    }

    #[test]
    fn positive_and_non_negative_follow_the_regexes() {
        assert_eq!(positive("vcpus", Some("4")).unwrap(), Some(4));
        assert!(positive("vcpus", Some("0")).is_err());
        assert!(positive("vcpus", Some("04")).is_err());
        assert!(positive("vcpus", Some("-1")).is_err());
        assert_eq!(non_negative("swap-mib", Some("0")).unwrap(), Some(0));
        assert!(non_negative("swap-mib", Some("")).is_err());
        assert_eq!(positive("vcpus", None).unwrap(), None);
    }

    #[test]
    fn env_requires_key_eq_value() {
        let env = parse_env(&["A=1".into(), "B=x=y".into()]).unwrap();
        assert_eq!(env["A"], "1");
        assert_eq!(env["B"], "x=y");
        assert!(parse_env(&["=v".into()]).is_err());
        assert!(parse_env(&["novalue".into()]).is_err());
    }

    #[test]
    fn first_enable_requires_name_and_vcpus() {
        let mut f = flags();
        assert!(build_image_config(&f, None).is_err());
        f.name = Some("demo".into());
        assert!(build_image_config(&f, None).is_err());
        f.vcpus = Some("2".into());
        let cfg = build_image_config(&f, None).unwrap();
        assert_eq!(cfg.name, "demo");
        assert_eq!(cfg.resources.unwrap().suggested_vcpus, Some(2));
    }

    #[test]
    fn update_overlays_the_stored_config() {
        let base = ImageConfig {
            name: "base".into(),
            description: Some("d".into()),
            env: [("A".to_string(), "1".to_string())].into_iter().collect(),
            workdir: Some("/w".into()),
            resources: Some(ImageResources {
                suggested_vcpus: Some(2),
                suggested_memory_mib: Some(1024),
                ..Default::default()
            }),
            warm: Some(ImageWarmConfig {
                command: vec!["true".into()],
                ..Default::default()
            }),
        };
        let mut f = flags();
        f.env = vec!["B=2".into()];
        f.memory_mib = Some("2048".into());
        let cfg = build_image_config(&f, Some(base)).unwrap();
        assert_eq!(cfg.name, "base");
        assert_eq!(cfg.env.len(), 2);
        let res = cfg.resources.unwrap();
        assert_eq!(res.suggested_vcpus, Some(2));
        assert_eq!(res.suggested_memory_mib, Some(2048));
        assert!(cfg.warm.is_some());
    }

    #[test]
    fn job_progress_prefers_chunks() {
        let mut job = EnableJob {
            chunks_total: Some(10),
            chunks_done: 3,
            ..Default::default()
        };
        assert_eq!(job_progress(&job), "3/10 chunks");
        job.chunks_total = None;
        job.capture_phase = Some("boot".into());
        job.warm_stage = Some("".into());
        assert_eq!(job_progress(&job), "boot");
        job.warm_stage = Some("npm".into());
        assert_eq!(job_progress(&job), "boot / npm");
    }
}
