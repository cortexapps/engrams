#!/bin/bash
# Create the orchestrator database on a fresh postgres volume.
# docker-entrypoint-initdb.d scripts run only when the data directory is
# empty (i.e. first boot of the container after volume creation). For
# existing volumes the Tilt `orchestrator-migrate` resource handles this
# idempotently via `createdb ... || true`.
set -e
psql -v ON_ERROR_STOP=1 --username "$POSTGRES_USER" --dbname "$POSTGRES_DB" <<-EOSQL
    CREATE DATABASE engram_orchestrator;
EOSQL
