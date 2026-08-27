# Cloud SQL Postgres for the control plane — promoted from the
# production deployment (ADR 0122).
#
# Two logical databases on one instance:
# - `controlplane` — the coordinator's metadata (sessions, events,
#   snapshots, hosts, registry credentials). The coordinator runs its
#   sqlx migrations at boot; this is the empty database to migrate
#   into.
# - `orchestrator` — the Bun/Hono orchestrator's own schema
#   (drizzle migrations via the chart's pre-upgrade Job).
#
# Private IP via a service-networking peering; reached from GKE pods
# over the shared VPC. The rendered DSNs land in Secret Manager —
# Terraform generates the password (random_password), so the DSN is
# necessarily TF-owned; every other secret follows the empty-shell
# pattern in the `secrets` module instead.
#
# THE SSLMODE SPLIT (load-bearing, learned in production):
# - coordinator DSN: `sslmode=require` — Rust sqlx keeps libpq's
#   lenient semantics (encrypt, don't verify).
# - orchestrator DSN: `sslmode=no-verify` — node-pg (and the drizzle
#   migrate Job's bundled pg-connection-string) treats `require` as
#   an alias for `verify-full`, which fails against Cloud SQL's
#   self-signed private-IP server cert (no SAN for 10.x). `no-verify`
#   keeps TLS encryption and skips cert verification — the correct
#   posture for a private-VPC hop.

resource "google_sql_database_instance" "engram" {
  name             = "${var.name_prefix}-pg"
  database_version = "POSTGRES_16"
  region           = var.region

  depends_on = [google_service_networking_connection.private_vpc]

  deletion_protection = var.deletion_protection

  settings {
    # ENTERPRISE (not the ENTERPRISE_PLUS default) accepts custom
    # `db-custom-*` tiers — cheaper and right-sized for the starting
    # load.
    edition           = "ENTERPRISE"
    tier              = var.tier
    availability_type = var.availability_type
    disk_type         = "PD_SSD"
    disk_size         = 50
    disk_autoresize   = true

    user_labels = merge(var.labels, {
      component = "database"
    })

    ip_configuration {
      # Public IP stays on for the Cloud SQL Auth Proxy path (no
      # authorized networks — direct password connections stay
      # blocked; the proxy still needs IAM). Private IP carries the
      # in-VPC pod traffic.
      ipv4_enabled                                  = var.enable_public_ip
      private_network                               = "projects/${var.project_id}/global/networks/${var.network_name}"
      enable_private_path_for_google_cloud_services = true
    }

    backup_configuration {
      enabled                        = true
      point_in_time_recovery_enabled = true
      start_time                     = "03:00"
      transaction_log_retention_days = 7
    }

    insights_config {
      query_insights_enabled  = true
      record_application_tags = true
    }

    database_flags {
      name  = "max_connections"
      value = "200"
    }

    database_flags {
      name  = "cloudsql.iam_authentication"
      value = "on"
    }
  }
}

# Private services networking — Cloud SQL talks to GKE pods over this
# peering. One-time per VPC; safe to re-apply.
resource "google_compute_global_address" "private_ip_alloc" {
  name          = "${var.name_prefix}-cloudsql-private-ip"
  purpose       = "VPC_PEERING"
  address_type  = "INTERNAL"
  prefix_length = 16
  network       = "projects/${var.project_id}/global/networks/${var.network_name}"
}

resource "google_service_networking_connection" "private_vpc" {
  network                 = "projects/${var.project_id}/global/networks/${var.network_name}"
  service                 = "servicenetworking.googleapis.com"
  reserved_peering_ranges = [google_compute_global_address.private_ip_alloc.name]
}

resource "google_sql_database" "controlplane" {
  name     = "controlplane"
  instance = google_sql_database_instance.engram.name
}

resource "google_sql_database" "orchestrator" {
  name     = "orchestrator"
  instance = google_sql_database_instance.engram.name
}

resource "random_password" "engram_db" {
  length  = 32
  special = false # `@` and `:` break some DSN parsers; not worth the entropy
}

resource "google_sql_user" "engram" {
  name     = var.db_user_name
  instance = google_sql_database_instance.engram.name
  password = random_password.engram_db.result
}

# ── DSN secrets ───────────────────────────────────────────────────

resource "google_secret_manager_secret" "database_url" {
  secret_id = "${var.name_prefix}-database-url"

  replication {
    auto {}
  }
}

resource "google_secret_manager_secret_version" "database_url" {
  secret      = google_secret_manager_secret.database_url.id
  secret_data = "postgres://${google_sql_user.engram.name}:${random_password.engram_db.result}@${google_sql_database_instance.engram.private_ip_address}:5432/${google_sql_database.controlplane.name}?sslmode=require"
}

resource "google_secret_manager_secret" "orchestrator_database_url" {
  secret_id = "${var.name_prefix}-orchestrator-database-url"

  replication {
    auto {}
  }
}

resource "google_secret_manager_secret_version" "orchestrator_database_url" {
  secret      = google_secret_manager_secret.orchestrator_database_url.id
  secret_data = "postgres://${google_sql_user.engram.name}:${random_password.engram_db.result}@${google_sql_database_instance.engram.private_ip_address}:5432/${google_sql_database.orchestrator.name}?sslmode=no-verify"
}

# Accessor grants for whoever relays the DSNs into the cluster
# (External Secrets Operator's GSA, and/or the coordinator GSA).
resource "google_secret_manager_secret_iam_member" "database_url_access" {
  for_each  = toset(var.secret_accessor_members)
  secret_id = google_secret_manager_secret.database_url.id
  role      = "roles/secretmanager.secretAccessor"
  member    = each.value
}

resource "google_secret_manager_secret_iam_member" "orchestrator_database_url_access" {
  for_each  = toset(var.secret_accessor_members)
  secret_id = google_secret_manager_secret.orchestrator_database_url.id
  role      = "roles/secretmanager.secretAccessor"
  member    = each.value
}
