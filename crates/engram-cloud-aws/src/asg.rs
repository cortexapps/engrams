//! ADR 0122: EC2 Auto Scaling actuator — the `NodePoolScaler` impl for
//! AWS. The `node_pool` identifier is the Auto Scaling group name.
//!
//! **UNVALIDATED ON REAL INFRA.** No EKS fleet exists to exercise the
//! full loop yet (it arrives with the Phase G bring-up). The calls are
//! structurally correct and fixture-tested against the wire protocol,
//! but treat this as observe-with-caution until a real ASG exercises
//! it.
//!
//! Contract mapping (ADR 0048 invariants):
//! - `set_size` → `SetDesiredCapacity` (grow-only per the trait; the
//!   operator owns policy). No LRO polling — the next reconcile
//!   re-asserts idempotently, the GKE posture.
//! - `remove_node` → resolve the K8s node name to an instance id
//!   (`ec2:DescribeInstances` on `private-dns-name` — the default EKS
//!   node name IS the EC2 private DNS name), verify the instance is in
//!   THIS group (`autoscaling:DescribeAutoScalingInstances` — the MIG
//!   ownership check's twin), then
//!   `TerminateInstanceInAutoScalingGroup{ShouldDecrementDesiredCapacity: true}`
//!   so the group doesn't replace the victim. A node already gone, or
//!   in a different group, is `Ok(())` (idempotent, per the trait).
//!
//! Documented v1 constraint: EKS-default node naming only. A fleet
//! using Karpenter or kubelet `--hostname-override` breaks the
//! name→instance resolution and is unsupported.
//!
//! Terraform-side coupling (the aws `host-operator-iam` +
//! `kvm-nodegroup` modules must match): the ASG suspends `AZRebalance`
//! (it would pick its own victims), keeps scale-in protection OFF
//! (this call is the sanctioned terminator), sets `max_size` above the
//! operator's ceiling, and `ignore_changes` on desired capacity.
//!
//! Auth: the SDK default chain via `engram-aws` — IRSA on EKS (the
//! operator pod is not hostNetwork, and IRSA is env/file-based so it
//! would work even if it were).

use async_trait::async_trait;

use engram_core::traits::cloud::NodePoolScaler;
use engram_core::BackendError;

/// Resizes an EC2 Auto Scaling group. `node_pool` = the ASG name.
pub struct AsgNodePoolScaler {
    asg: aws_sdk_autoscaling::Client,
    ec2: aws_sdk_ec2::Client,
}

impl AsgNodePoolScaler {
    /// Build against the SDK default chains. Fails fast when no region
    /// resolves (off-AWS, or a pod without `AWS_REGION`/IRSA env) so
    /// the operator falls back to the noop scaler instead of wedging.
    pub async fn detect() -> Result<Self, BackendError> {
        let cfg = engram_aws::sdk_config(engram_aws::AwsOverrides::default()).await;
        let Some(region) = cfg.region().cloned() else {
            return Err(BackendError::Protocol(
                "no AWS region resolved (set AWS_REGION or run with IRSA); asg scaler unavailable"
                    .into(),
            ));
        };
        tracing::info!(region = %region, "asg scaler: detected region");
        Ok(Self::from_config(&cfg))
    }

    fn from_config(cfg: &engram_aws::SdkConfig) -> Self {
        Self {
            asg: aws_sdk_autoscaling::Client::new(cfg),
            ec2: aws_sdk_ec2::Client::new(cfg),
        }
    }

    /// Test constructor: explicit region + endpoint (wiremock).
    #[cfg(test)]
    async fn for_tests(endpoint: String) -> Self {
        std::env::set_var("AWS_ACCESS_KEY_ID", "test");
        std::env::set_var("AWS_SECRET_ACCESS_KEY", "test");
        let cfg = engram_aws::sdk_config(engram_aws::AwsOverrides {
            region: Some("us-east-1".into()),
            endpoint_url: Some(endpoint),
        })
        .await;
        Self::from_config(&cfg)
    }

    /// Resolve a K8s node name (= EC2 private DNS name on default EKS)
    /// to its instance id. `Ok(None)` when no live instance carries the
    /// name — the node is already gone. More than one match is a
    /// protocol error: private DNS names are unique among live
    /// instances in a VPC, so an ambiguous answer means the query (or
    /// the fleet) is not what we think it is, and guessing a victim is
    /// how a wrong node dies.
    async fn resolve_instance_id(&self, node_name: &str) -> Result<Option<String>, BackendError> {
        let resp = self
            .ec2
            .describe_instances()
            .filters(
                aws_sdk_ec2::types::Filter::builder()
                    .name("private-dns-name")
                    .values(node_name)
                    .build(),
            )
            .filters(
                aws_sdk_ec2::types::Filter::builder()
                    .name("instance-state-name")
                    .values("pending")
                    .values("running")
                    .values("stopping")
                    .values("shutting-down")
                    .build(),
            )
            .send()
            .await
            .map_err(|e| BackendError::Sdk(Box::new(e)))?;

        let mut ids = resp
            .reservations()
            .iter()
            .flat_map(|r| r.instances())
            .filter_map(|i| i.instance_id().map(str::to_string));
        let first = ids.next();
        if let Some(second) = ids.next() {
            return Err(BackendError::Protocol(format!(
                "private-dns-name {node_name} matched multiple instances \
                 ({first:?}, {second:?}, ...); refusing to pick a victim"
            )));
        }
        Ok(first)
    }
}

#[async_trait]
impl NodePoolScaler for AsgNodePoolScaler {
    async fn set_size(&self, node_pool: &str, desired: u32) -> Result<(), BackendError> {
        self.asg
            .set_desired_capacity()
            .auto_scaling_group_name(node_pool)
            .desired_capacity(desired as i32)
            // No cooldown gate: the operator's reconcile owns pacing,
            // and a queue-driven grow must not wait out a cooldown.
            .honor_cooldown(false)
            .send()
            .await
            .map_err(|e| BackendError::Protocol(format!("asg SetDesiredCapacity: {e}")))?;
        // SetDesiredCapacity is asynchronous; we don't poll the scaling
        // activity — the next reconcile re-asserts the desired size.
        tracing::info!(node_pool, desired, "asg: SetDesiredCapacity accepted");
        Ok(())
    }

    async fn remove_node(&self, node_pool: &str, node_name: &str) -> Result<(), BackendError> {
        // 1. Node name → instance id. Already gone ⇒ done.
        let Some(instance_id) = self.resolve_instance_id(node_name).await? else {
            tracing::info!(
                node_pool,
                node_name,
                "asg: no live instance carries this node name — already removed (idempotent)"
            );
            return Ok(());
        };

        // 2. Membership check — the twin of GKE's "which MIG owns this
        //    node". An instance outside THIS group is not ours to kill.
        let resp = self
            .asg
            .describe_auto_scaling_instances()
            .instance_ids(&instance_id)
            .send()
            .await
            .map_err(|e| BackendError::Sdk(Box::new(e)))?;
        let owner = resp
            .auto_scaling_instances()
            .iter()
            .find(|d| d.instance_id() == Some(instance_id.as_str()))
            .and_then(|d| d.auto_scaling_group_name());
        match owner {
            Some(group) if group == node_pool => {}
            other => {
                // Not in the pool (detached, another group, or already
                // terminated out of scaling). The wave driver may
                // re-issue after a restart; treat as done.
                tracing::info!(
                    node_pool,
                    node_name,
                    %instance_id,
                    owner = other.unwrap_or("<none>"),
                    "asg: instance not in this group — already removed (idempotent)"
                );
                return Ok(());
            }
        }

        // 3. Terminate THIS instance and decrement desired capacity in
        //    one atomic call, so the group doesn't recreate it — the
        //    deleteInstances twin.
        //
        //    Error discipline (review finding on this PR): only a
        //    ValidationError whose message says the instance is not
        //    found / not managed means "it left the group between the
        //    membership check above and now" — idempotent success.
        //    Every other error — ScalingActivityInProgress (a
        //    transient retry-later fault: the terminate did NOT run),
        //    min-size ValidationErrors, throttles — must surface as
        //    Err so the wave driver keeps the victim and retries next
        //    tick. Returning Ok on a retryable fault deletes the
        //    coordinator row and clears the victim annotation while
        //    the instance keeps running: an untracked zombie that
        //    still counts against ASG capacity.
        match self
            .asg
            .terminate_instance_in_auto_scaling_group()
            .instance_id(&instance_id)
            .should_decrement_desired_capacity(true)
            .send()
            .await
        {
            Ok(_) => {
                tracing::info!(
                    node_pool,
                    node_name,
                    %instance_id,
                    "asg: TerminateInstanceInAutoScalingGroup accepted"
                );
                Ok(())
            }
            Err(e) => {
                let instance_gone = e
                    .as_service_error()
                    .filter(|se| se.meta().code() == Some("ValidationError"))
                    .and_then(|se| se.meta().message())
                    .map(|msg| {
                        let msg = msg.to_ascii_lowercase();
                        msg.contains("not found") || msg.contains("no managed instance")
                    })
                    .unwrap_or(false);
                if instance_gone {
                    tracing::warn!(
                        node_pool,
                        node_name,
                        %instance_id,
                        error = %e,
                        "asg: terminate raced the instance leaving the group — treating as removed"
                    );
                    Ok(())
                } else {
                    Err(BackendError::Protocol(format!(
                        "asg TerminateInstanceInAutoScalingGroup: {e}"
                    )))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use wiremock::matchers::body_string_contains;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn xml(body: &str) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_raw(body.to_string(), "text/xml")
    }

    const EC2_NO_INSTANCES: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<DescribeInstancesResponse xmlns="http://ec2.amazonaws.com/doc/2016-11-15/">
  <requestId>req-1</requestId>
  <reservationSet/>
</DescribeInstancesResponse>"#;

    const EC2_ONE_INSTANCE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<DescribeInstancesResponse xmlns="http://ec2.amazonaws.com/doc/2016-11-15/">
  <requestId>req-1</requestId>
  <reservationSet>
    <item>
      <instancesSet>
        <item><instanceId>i-0deadbeef</instanceId></item>
      </instancesSet>
    </item>
  </reservationSet>
</DescribeInstancesResponse>"#;

    const ASG_MEMBER_OF_POOL: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<DescribeAutoScalingInstancesResponse xmlns="http://autoscaling.amazonaws.com/doc/2011-01-01/">
  <DescribeAutoScalingInstancesResult>
    <AutoScalingInstances>
      <member>
        <InstanceId>i-0deadbeef</InstanceId>
        <AutoScalingGroupName>engram-kvm</AutoScalingGroupName>
        <AvailabilityZone>us-east-1a</AvailabilityZone>
        <LifecycleState>InService</LifecycleState>
        <HealthStatus>HEALTHY</HealthStatus>
        <ProtectedFromScaleIn>false</ProtectedFromScaleIn>
      </member>
    </AutoScalingInstances>
  </DescribeAutoScalingInstancesResult>
  <ResponseMetadata><RequestId>req-2</RequestId></ResponseMetadata>
</DescribeAutoScalingInstancesResponse>"#;

    const ASG_NOT_A_MEMBER: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<DescribeAutoScalingInstancesResponse xmlns="http://autoscaling.amazonaws.com/doc/2011-01-01/">
  <DescribeAutoScalingInstancesResult>
    <AutoScalingInstances/>
  </DescribeAutoScalingInstancesResult>
  <ResponseMetadata><RequestId>req-2</RequestId></ResponseMetadata>
</DescribeAutoScalingInstancesResponse>"#;

    const ASG_TERMINATE_OK: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<TerminateInstanceInAutoScalingGroupResponse xmlns="http://autoscaling.amazonaws.com/doc/2011-01-01/">
  <TerminateInstanceInAutoScalingGroupResult/>
  <ResponseMetadata><RequestId>req-3</RequestId></ResponseMetadata>
</TerminateInstanceInAutoScalingGroupResponse>"#;

    const ASG_SET_CAPACITY_OK: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<SetDesiredCapacityResponse xmlns="http://autoscaling.amazonaws.com/doc/2011-01-01/">
  <ResponseMetadata><RequestId>req-4</RequestId></ResponseMetadata>
</SetDesiredCapacityResponse>"#;

    const ASG_VALIDATION_ERROR: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<ErrorResponse xmlns="http://autoscaling.amazonaws.com/doc/2011-01-01/">
  <Error>
    <Type>Sender</Type>
    <Code>ValidationError</Code>
    <Message>Instance Id not found - No managed instance found for instance ID: i-0deadbeef</Message>
  </Error>
  <RequestId>req-5</RequestId>
</ErrorResponse>"#;

    const ASG_SCALING_IN_PROGRESS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<ErrorResponse xmlns="http://autoscaling.amazonaws.com/doc/2011-01-01/">
  <Error>
    <Type>Sender</Type>
    <Code>ScalingActivityInProgress</Code>
    <Message>Activity 12345 is in progress and blocks this action</Message>
  </Error>
  <RequestId>req-6</RequestId>
</ErrorResponse>"#;

    const ASG_MIN_SIZE_VALIDATION_ERROR: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<ErrorResponse xmlns="http://autoscaling.amazonaws.com/doc/2011-01-01/">
  <Error>
    <Type>Sender</Type>
    <Code>ValidationError</Code>
    <Message>Terminating instance without replacement will violate group's min size constraint of 2</Message>
  </Error>
  <RequestId>req-7</RequestId>
</ErrorResponse>"#;

    /// Missing node → `Ok(())` without touching the autoscaling API
    /// (the trait's idempotency contract, pinned at the wire level).
    #[tokio::test(flavor = "current_thread")]
    async fn remove_node_missing_instance_is_idempotent_ok() {
        let server = MockServer::start().await;
        Mock::given(body_string_contains("Action=DescribeInstances"))
            .respond_with(xml(EC2_NO_INSTANCES))
            .expect(1)
            .mount(&server)
            .await;

        let scaler = AsgNodePoolScaler::for_tests(server.uri()).await;
        scaler
            .remove_node("engram-kvm", "ip-10-0-1-5.ec2.internal")
            .await
            .expect("missing node must be Ok");
    }

    /// Instance exists but is not in the named group → `Ok(())`, and
    /// TerminateInstanceInAutoScalingGroup is never called.
    #[tokio::test(flavor = "current_thread")]
    async fn remove_node_foreign_instance_is_idempotent_ok() {
        let server = MockServer::start().await;
        Mock::given(body_string_contains("Action=DescribeInstances"))
            .respond_with(xml(EC2_ONE_INSTANCE))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(body_string_contains("Action=DescribeAutoScalingInstances"))
            .respond_with(xml(ASG_NOT_A_MEMBER))
            .expect(1)
            .mount(&server)
            .await;
        // No mock for Terminate — a call to it 404s and fails the test.

        let scaler = AsgNodePoolScaler::for_tests(server.uri()).await;
        scaler
            .remove_node("engram-kvm", "ip-10-0-1-5.ec2.internal")
            .await
            .expect("foreign instance must be Ok without terminating");
    }

    /// The happy path: resolve → membership check → terminate with
    /// decrement.
    #[tokio::test(flavor = "current_thread")]
    async fn remove_node_terminates_a_pool_member() {
        let server = MockServer::start().await;
        Mock::given(body_string_contains("Action=DescribeInstances"))
            .respond_with(xml(EC2_ONE_INSTANCE))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(body_string_contains("Action=DescribeAutoScalingInstances"))
            .respond_with(xml(ASG_MEMBER_OF_POOL))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(body_string_contains(
            "Action=TerminateInstanceInAutoScalingGroup",
        ))
        .and(body_string_contains("InstanceId=i-0deadbeef"))
        .and(body_string_contains("ShouldDecrementDesiredCapacity=true"))
        .respond_with(xml(ASG_TERMINATE_OK))
        .expect(1)
        .mount(&server)
        .await;

        let scaler = AsgNodePoolScaler::for_tests(server.uri()).await;
        scaler
            .remove_node("engram-kvm", "ip-10-0-1-5.ec2.internal")
            .await
            .expect("member removal must succeed");
    }

    /// A terminate that races the instance leaving the group
    /// (ValidationError) is idempotent success, not an error.
    #[tokio::test(flavor = "current_thread")]
    async fn remove_node_validation_error_race_is_ok() {
        let server = MockServer::start().await;
        Mock::given(body_string_contains("Action=DescribeInstances"))
            .respond_with(xml(EC2_ONE_INSTANCE))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(body_string_contains("Action=DescribeAutoScalingInstances"))
            .respond_with(xml(ASG_MEMBER_OF_POOL))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(body_string_contains(
            "Action=TerminateInstanceInAutoScalingGroup",
        ))
        .respond_with(ResponseTemplate::new(400).set_body_raw(ASG_VALIDATION_ERROR, "text/xml"))
        .expect(1)
        .mount(&server)
        .await;

        let scaler = AsgNodePoolScaler::for_tests(server.uri()).await;
        scaler
            .remove_node("engram-kvm", "ip-10-0-1-5.ec2.internal")
            .await
            .expect("terminate race must be Ok");
    }

    /// A transient ScalingActivityInProgress means the terminate did
    /// NOT run — it must surface as Err so the wave driver keeps the
    /// victim and retries next tick, never as idempotent success
    /// (which would leak a running, untracked instance).
    #[tokio::test(flavor = "current_thread")]
    async fn remove_node_scaling_in_progress_surfaces_as_err() {
        let server = MockServer::start().await;
        Mock::given(body_string_contains("Action=DescribeInstances"))
            .respond_with(xml(EC2_ONE_INSTANCE))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(body_string_contains("Action=DescribeAutoScalingInstances"))
            .respond_with(xml(ASG_MEMBER_OF_POOL))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(body_string_contains(
            "Action=TerminateInstanceInAutoScalingGroup",
        ))
        .respond_with(ResponseTemplate::new(400).set_body_raw(ASG_SCALING_IN_PROGRESS, "text/xml"))
        .expect(1)
        .mount(&server)
        .await;

        let scaler = AsgNodePoolScaler::for_tests(server.uri()).await;
        scaler
            .remove_node("engram-kvm", "ip-10-0-1-5.ec2.internal")
            .await
            .expect_err("a retry-later fault must NOT read as removed");
    }

    /// A ValidationError that is NOT instance-gone (here: a min-size
    /// violation) must also surface as Err — only the
    /// instance-not-found message reads as idempotent success.
    #[tokio::test(flavor = "current_thread")]
    async fn remove_node_min_size_validation_error_surfaces_as_err() {
        let server = MockServer::start().await;
        Mock::given(body_string_contains("Action=DescribeInstances"))
            .respond_with(xml(EC2_ONE_INSTANCE))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(body_string_contains("Action=DescribeAutoScalingInstances"))
            .respond_with(xml(ASG_MEMBER_OF_POOL))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(body_string_contains(
            "Action=TerminateInstanceInAutoScalingGroup",
        ))
        .respond_with(
            ResponseTemplate::new(400).set_body_raw(ASG_MIN_SIZE_VALIDATION_ERROR, "text/xml"),
        )
        .expect(1)
        .mount(&server)
        .await;

        let scaler = AsgNodePoolScaler::for_tests(server.uri()).await;
        scaler
            .remove_node("engram-kvm", "ip-10-0-1-5.ec2.internal")
            .await
            .expect_err("a min-size violation must NOT read as removed");
    }

    /// `set_size` posts the group name + capacity and treats the empty
    /// success response as accepted.
    #[tokio::test(flavor = "current_thread")]
    async fn set_size_posts_desired_capacity() {
        let server = MockServer::start().await;
        Mock::given(body_string_contains("Action=SetDesiredCapacity"))
            .and(body_string_contains("AutoScalingGroupName=engram-kvm"))
            .and(body_string_contains("DesiredCapacity=4"))
            .and(body_string_contains("HonorCooldown=false"))
            .respond_with(xml(ASG_SET_CAPACITY_OK))
            .expect(1)
            .mount(&server)
            .await;

        let scaler = AsgNodePoolScaler::for_tests(server.uri()).await;
        scaler
            .set_size("engram-kvm", 4)
            .await
            .expect("set_size must succeed");
    }
}
