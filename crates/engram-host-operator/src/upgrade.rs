//! ADR 0126: the host-fleet side of an upgrade the host agents cannot
//! adopt. The drain itself is `engram_upgrade_drain`; this module records
//! that it ran and, after the upgrade, rolls the host-agent pods.

use std::collections::BTreeMap;

use engram_upgrade_drain::Coordinator;
use k8s_openapi::api::apps::v1::DaemonSet;
use k8s_openapi::api::core::v1::{ConfigMap, Pod};
use kube::api::{Api, DeleteParams, ListParams, PostParams};

use crate::error::OperatorError;

/// Record, in `namespace`, that a drain ran, so the post-upgrade roll
/// knows the nodes are empty.
pub async fn write_marker(
    client: kube::Client,
    namespace: &str,
    name: &str,
) -> Result<(), OperatorError> {
    let api: Api<ConfigMap> = Api::namespaced(client, namespace);
    let marker = ConfigMap {
        metadata: kube::api::ObjectMeta {
            name: Some(name.into()),
            ..Default::default()
        },
        data: Some(BTreeMap::from([(
            "reattach_generation".into(),
            engram_protocol::REATTACH_GENERATION.to_string(),
        )])),
        ..Default::default()
    };
    match api.create(&PostParams::default(), &marker).await {
        Ok(_) => Ok(()),
        Err(kube::Error::Api(e)) if e.code == 409 => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// The DaemonSet pods not on its current template.
pub fn stale_pods(pods: &[Pod], ds_generation: i64) -> Vec<String> {
    let current = ds_generation.to_string();
    pods.iter()
        .filter(|p| {
            p.metadata
                .labels
                .as_ref()
                .and_then(|l| l.get("pod-template-generation"))
                != Some(&current)
        })
        .filter_map(|p| p.metadata.name.clone())
        .collect()
}

#[derive(Debug, PartialEq, Eq)]
pub enum RollOutcome {
    /// No drain ran, so this upgrade rolls the usual way.
    NoDrain,
    Rolled {
        deleted: Vec<String>,
    },
}

/// After a drain: delete the host-agent pods not on the DaemonSet's current
/// template, so the `OnDelete` DaemonSet rolls onto the new build, then
/// remove the marker. With the operator enabled the operator rolls them.
pub async fn roll(
    client: kube::Client,
    coord: &Coordinator,
    namespace: &str,
    daemon_set: &str,
    marker: &str,
    operator_rolls: bool,
) -> Result<RollOutcome, OperatorError> {
    let markers: Api<ConfigMap> = Api::namespaced(client.clone(), namespace);
    if markers.get_opt(marker).await?.is_none() {
        return Ok(RollOutcome::NoDrain);
    }
    let busy: Vec<_> = coord
        .list_hosts()
        .await?
        .into_iter()
        .filter(|h| {
            h.live
                && h.reattach_generation < engram_protocol::REATTACH_GENERATION
                && h.running_sandboxes > 0
        })
        .map(|h| h.id.to_string())
        .collect();
    if !busy.is_empty() {
        return Err(OperatorError::Upgrade(format!(
            "hosts the new build cannot adopt still run sandboxes: {}",
            busy.join(", ")
        )));
    }
    let mut deleted = Vec::new();
    if !operator_rolls {
        let ds = Api::<DaemonSet>::namespaced(client.clone(), namespace)
            .get(daemon_set)
            .await?;
        let generation = ds.metadata.generation.ok_or_else(|| {
            OperatorError::Invalid(format!("DaemonSet {daemon_set} has no generation"))
        })?;
        let selector = ds
            .spec
            .selector
            .match_labels
            .as_ref()
            .map(|labels| {
                labels
                    .iter()
                    .map(|(k, v)| format!("{k}={v}"))
                    .collect::<Vec<_>>()
                    .join(",")
            })
            .ok_or_else(|| {
                OperatorError::Invalid(format!("DaemonSet {daemon_set} has no label selector"))
            })?;
        let pods: Api<Pod> = Api::namespaced(client.clone(), namespace);
        let listed = pods.list(&ListParams::default().labels(&selector)).await?;
        for name in stale_pods(&listed.items, generation) {
            pods.delete(&name, &DeleteParams::default()).await?;
            deleted.push(name);
        }
    }
    markers.delete(marker, &DeleteParams::default()).await?;
    Ok(RollOutcome::Rolled { deleted })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_pods_are_the_ones_off_the_current_template() {
        let pod = |name: &str, generation: Option<&str>| Pod {
            metadata: kube::api::ObjectMeta {
                name: Some(name.into()),
                labels: generation.map(|g| {
                    BTreeMap::from([("pod-template-generation".to_string(), g.to_string())])
                }),
                ..Default::default()
            },
            ..Default::default()
        };
        let pods = [
            pod("old", Some("3")),
            pod("new", Some("4")),
            pod("unlabelled", None),
        ];
        assert_eq!(stale_pods(&pods, 4), vec!["old", "unlabelled"]);
    }
}
