output "pool_name" {
  value       = google_container_node_pool.kvm_nodes.name
  description = "Node-pool name — set it as the host-fleet chart's `operator.autoscaling.nodePool` (the `gke` scaler's pool identifier)."
}
