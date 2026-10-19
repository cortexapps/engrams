//! engrams host … — FleetService passthrough verbs (admin surfaces).

use serde::Serialize;

use engram_app_client::proto::*;
use engram_app_client::Clients;

use crate::output::{detail, fail, print_json, table, CliResult};

#[derive(Serialize)]
struct HostJson {
    id: String,
    hostname: String,
    status: String,
    cordoned: bool,
    // ADR 0046/0047 figures — what placement actually schedules on.
    allocatable_mib: u64,
    reserved_mib: u64,
    free_mib: u64,
    base_shm_mib: u64,
    cpu_budget_vcpus: u64,
    free_vcpus: u64,
    failing_capabilities: Vec<String>,
    running_sandboxes: u32,
    ready_images: u64,
    ready_image_digests: Vec<String>,
    last_heartbeat_at: String,
}

fn host_json(h: HostView) -> HostJson {
    HostJson {
        id: h.id,
        hostname: h.hostname,
        status: h.status,
        cordoned: h.cordoned,
        allocatable_mib: h.allocatable_mib,
        reserved_mib: h.reserved_mib,
        free_mib: h.free_mib,
        base_shm_mib: h.util_base_shm_mib,
        cpu_budget_vcpus: h.cpu_budget_vcpus,
        free_vcpus: h.free_vcpus,
        failing_capabilities: h.failing_capabilities,
        running_sandboxes: h.running_sandboxes,
        ready_images: h.ready_images,
        ready_image_digests: h.ready_image_digests,
        last_heartbeat_at: h.last_heartbeat_at,
    }
}

fn flagged(h: &HostView) -> bool {
    h.cordoned || !h.failing_capabilities.is_empty()
}

/// `ready*` — cordoned or capability-failed hosts don't take placements.
fn status_cell(h: &HostView) -> String {
    if flagged(h) {
        format!("{}*", h.status)
    } else {
        h.status.clone()
    }
}

pub async fn list(c: &Clients, json: bool) -> CliResult {
    let resp = c.list_hosts(ListHostsRequest::default()).await?;
    if json {
        #[derive(Serialize)]
        struct Out {
            hosts: Vec<HostJson>,
        }
        print_json(&Out {
            hosts: resp.hosts.into_iter().map(host_json).collect(),
        });
        return Ok(());
    }
    if resp.hosts.is_empty() {
        println!("(no hosts registered)");
        return Ok(());
    }
    table(
        &[
            "ID",
            "STATUS",
            "FREE_MIB",
            "ALLOC_MIB",
            "BASE_SHM",
            "VCPU_FREE",
            "SANDBOXES",
        ],
        &resp
            .hosts
            .iter()
            .map(|h| {
                vec![
                    h.id.clone(),
                    status_cell(h),
                    h.free_mib.to_string(),
                    h.allocatable_mib.to_string(),
                    h.util_base_shm_mib.to_string(),
                    h.free_vcpus.to_string(),
                    h.running_sandboxes.to_string(),
                ]
            })
            .collect::<Vec<_>>(),
        &[36, 10, 9, 10, 9, 9, 9],
    );
    for h in resp.hosts.iter().filter(|h| flagged(h)) {
        let mut why: Vec<String> = Vec::new();
        if h.cordoned {
            why.push("cordoned".to_string());
        }
        why.extend(h.failing_capabilities.iter().map(|c| format!("cap:{c}")));
        println!("  * {}: {}", h.id, why.join(", "));
    }
    Ok(())
}

pub async fn get(c: &Clients, id: &str, json: bool) -> CliResult {
    let resp = c
        .get_host(GetHostRequest {
            host_id: id.to_string(),
        })
        .await?;
    let Some(h) = resp.host else {
        return fail("response carried no host");
    };
    if json {
        print_json(&host_json(h));
        return Ok(());
    }
    detail(&[
        ("id", Some(h.id.clone())),
        ("hostname", Some(h.hostname.clone())),
        ("status", Some(h.status.clone())),
        ("cordoned", Some(h.cordoned.to_string())),
        ("allocatable", Some(format!("{} MiB", h.allocatable_mib))),
        ("reserved", Some(format!("{} MiB", h.reserved_mib))),
        ("free", Some(format!("{} MiB", h.free_mib))),
        ("base_shm", Some(format!("{} MiB", h.util_base_shm_mib))),
        ("cpu_budget", Some(format!("{} vCPU", h.cpu_budget_vcpus))),
        ("cpu_free", Some(format!("{} vCPU", h.free_vcpus))),
        (
            "failing_capabilities",
            Some(if h.failing_capabilities.is_empty() {
                "(none)".to_string()
            } else {
                h.failing_capabilities.join(", ")
            }),
        ),
        ("running_sandboxes", Some(h.running_sandboxes.to_string())),
    ]);
    Ok(())
}

pub async fn drain(c: &Clients, id: &str) -> CliResult {
    c.drain_host(DrainHostRequest {
        host_id: id.to_string(),
    })
    .await?;
    println!("draining");
    Ok(())
}

/// FleetService.DeleteHost — deregister a host row. FAILED_PRECONDITION while
/// any session is still bound (drain / reap sessions first); idempotent-success
/// if already gone.
pub async fn remove(c: &Clients, id: &str) -> CliResult {
    c.delete_host(DeleteHostRequest {
        host_id: id.to_string(),
    })
    .await?;
    println!("deleted");
    Ok(())
}

pub async fn uncordon(c: &Clients, id: &str, json: bool) -> CliResult {
    let resp = c
        .uncordon_host(UncordonHostRequest {
            host_id: id.to_string(),
            owner: "admin".to_string(),
        })
        .await?;
    if json {
        #[derive(Serialize)]
        struct Out {
            host_id: String,
            status: String,
        }
        print_json(&Out {
            host_id: resp.host_id,
            status: resp.status,
        });
    } else {
        println!("{}: {}", resp.host_id, resp.status);
    }
    Ok(())
}

pub async fn evacuate(c: &Clients, session_id: &str, json: bool) -> CliResult {
    let resp = c
        .teleport_session(TeleportSessionRequest {
            session_id: session_id.to_string(),
            target_host: None,
        })
        .await?;
    if json {
        #[derive(Serialize)]
        struct Out {
            teleport_id: String,
            kind: String,
            dest_host_id: String,
        }
        print_json(&Out {
            teleport_id: resp.teleport_id,
            kind: resp.kind,
            dest_host_id: resp.dest_host_id,
        });
    } else {
        println!(
            "{}: {} to {}",
            resp.teleport_id, resp.kind, resp.dest_host_id
        );
    }
    Ok(())
}
