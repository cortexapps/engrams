# Network plumbing for an Engram GCP deployment.
#
# A dedicated VPC + subnet keeps FC microVM TAP traffic, the
# coord's Postgres + GCS egress, and any in-cluster gossip
# isolated from whatever else lives in the project.
#
# Cloud NAT lets FC guests reach the public internet (Docker
# registries, package mirrors) without per-host public IPs.
# That's important because each FC host runs many sessions; the
# host-agent's egress proxy MITMs guest TLS but still needs
# upstream IP connectivity.

resource "google_compute_network" "this" {
  name                    = var.name
  auto_create_subnetworks = false
  routing_mode            = "REGIONAL"
}

resource "google_compute_subnetwork" "primary" {
  name                     = "${var.name}-primary"
  ip_cidr_range            = var.primary_cidr
  region                   = var.region
  network                  = google_compute_network.this.id
  private_ip_google_access = true

  log_config {
    aggregation_interval = "INTERVAL_10_MIN"
    flow_sampling        = 0.5
    metadata             = "INCLUDE_ALL_METADATA"
  }
}

resource "google_compute_router" "nat" {
  name    = "${var.name}-router"
  region  = var.region
  network = google_compute_network.this.id
}

resource "google_compute_router_nat" "nat" {
  name                               = "${var.name}-nat"
  router                             = google_compute_router.nat.name
  region                             = var.region
  nat_ip_allocate_option             = "AUTO_ONLY"
  source_subnetwork_ip_ranges_to_nat = "ALL_SUBNETWORKS_ALL_IP_RANGES"

  log_config {
    enable = true
    filter = "ERRORS_ONLY"
  }
}

# Permissive intra-VPC firewall — coord<->host, host<->host
# (future cross-host migration), MIG members talking to the
# internal LB. Operators tighten by tag once the prod traffic
# pattern is locked.
resource "google_compute_firewall" "internal" {
  name      = "${var.name}-internal"
  network   = google_compute_network.this.id
  direction = "INGRESS"
  priority  = 1000

  source_ranges = [var.primary_cidr]

  allow {
    protocol = "tcp"
  }
  allow {
    protocol = "udp"
  }
  allow {
    protocol = "icmp"
  }
}

# IAP tunnel ingress so ops can SSH into a host without giving it
# a public IP. Locked to Google's IAP CIDR block.
resource "google_compute_firewall" "iap_ssh" {
  name      = "${var.name}-iap-ssh"
  network   = google_compute_network.this.id
  direction = "INGRESS"
  priority  = 1000

  source_ranges = ["35.235.240.0/20"]
  target_tags   = [var.iap_target_tag]

  allow {
    protocol = "tcp"
    ports    = ["22"]
  }
}
