# RDS Postgres for the control plane — the AWS twin of
# gcp/modules/cloudsql (ADR 0122). Plain Postgres: no connector
# protocol, just a private-subnet endpoint + TLS.
#
# One deliberate difference from Cloud SQL: the AWS provider cannot
# create LOGICAL databases inside an instance, so the quickstart runs
# a one-shot in-cluster psql Job that `CREATE DATABASE controlplane` /
# `orchestrator` idempotently. This module provisions the instance,
# the master user, and the two DSN secrets those databases will be
# reached by.
#
# THE SSLMODE SPLIT (same production lesson as GCP): the coordinator
# DSN is `sslmode=require` (Rust sqlx keeps libpq's lenient
# encrypt-don't-verify semantics); the orchestrator DSN is
# `sslmode=no-verify` (node-pg reads `require` as `verify-full`,
# which fails against the RDS cert chain unless the RDS CA bundle is
# distributed — `no-verify` keeps TLS encryption without the bundle).

resource "aws_db_subnet_group" "this" {
  name       = "${var.name_prefix}-pg"
  subnet_ids = var.subnet_ids
  tags       = var.tags
}

resource "aws_security_group" "db" {
  name_prefix = "${var.name_prefix}-pg-"
  vpc_id      = var.vpc_id
  description = "Postgres from the cluster only."

  ingress {
    description     = "Postgres from allowed security groups"
    from_port       = 5432
    to_port         = 5432
    protocol        = "tcp"
    security_groups = var.allowed_security_group_ids
  }

  egress {
    from_port   = 0
    to_port     = 0
    protocol    = "-1"
    cidr_blocks = ["0.0.0.0/0"]
  }

  tags = var.tags

  lifecycle {
    create_before_destroy = true
  }
}

resource "random_password" "master" {
  length  = 32
  special = false # `@` and `:` break some DSN parsers; not worth the entropy
}

resource "aws_db_instance" "this" {
  identifier     = "${var.name_prefix}-pg"
  engine         = "postgres"
  engine_version = var.engine_version
  instance_class = var.instance_class

  allocated_storage     = 50
  max_allocated_storage = 200
  storage_type          = "gp3"
  storage_encrypted     = true

  db_name  = "postgres"
  username = var.db_user_name
  password = random_password.master.result

  db_subnet_group_name   = aws_db_subnet_group.this.name
  vpc_security_group_ids = [aws_security_group.db.id]
  publicly_accessible    = false
  multi_az               = var.multi_az

  backup_retention_period = 7
  backup_window           = "03:00-04:00"

  performance_insights_enabled = true

  deletion_protection = var.deletion_protection
  skip_final_snapshot = !var.deletion_protection

  tags = var.tags
}

# ── DSN secrets ───────────────────────────────────────────────────
# TF generates the password, so the DSNs are the documented
# tfstate exception (same posture as the GCP cloudsql module).

resource "aws_secretsmanager_secret" "database_url" {
  name                    = "${var.name_prefix}/database-url"
  description             = "Coordinator DSN (sslmode=require)."
  recovery_window_in_days = var.secret_recovery_window_in_days
  tags                    = var.tags
}

resource "aws_secretsmanager_secret_version" "database_url" {
  secret_id     = aws_secretsmanager_secret.database_url.id
  secret_string = "postgres://${var.db_user_name}:${random_password.master.result}@${aws_db_instance.this.address}:5432/controlplane?sslmode=require"
}

resource "aws_secretsmanager_secret" "orchestrator_database_url" {
  name                    = "${var.name_prefix}/orchestrator-database-url"
  description             = "Orchestrator DSN (sslmode=no-verify — the node-pg posture)."
  recovery_window_in_days = var.secret_recovery_window_in_days
  tags                    = var.tags
}

resource "aws_secretsmanager_secret_version" "orchestrator_database_url" {
  secret_id     = aws_secretsmanager_secret.orchestrator_database_url.id
  secret_string = "postgres://${var.db_user_name}:${random_password.master.result}@${aws_db_instance.this.address}:5432/orchestrator?sslmode=no-verify"
}
