//! Per-test Postgres isolation (ADR 0099 H1).
//!
//! Every caller gets its OWN freshly-cloned database, so tests can run in
//! parallel with zero cross-talk: no shared rows (the ADR 0047 global
//! `hosts` table pollution class) and no shared LISTEN/NOTIFY channels
//! (`NOTIFY` is per-database, which is what forced the coordinator PG lane
//! to `--test-threads=1` before this existed).
//!
//! Mechanics: a template database `engram_test_tmpl_<fingerprint>` is
//! migrated once — the fingerprint hashes the embedded migrator's
//! (version, checksum) pairs, so adding a migration automatically mints a
//! new template — and each test then runs
//! `CREATE DATABASE … TEMPLATE …` (~100–300 ms) instead of replaying the
//! whole migration chain. Bootstrap and clones are serialized under a
//! session-scoped advisory lock: concurrent `CREATE DATABASE … TEMPLATE x`
//! fails with "source database is being accessed by other users", and
//! nextest runs each test in its own process, so an in-process guard is
//! not enough.
//!
//! Per-test databases are deliberately leaked: CI's Postgres is an
//! ephemeral service container. For long-lived dev databases, template
//! bootstrap opportunistically drops testkit-named databases older than a
//! few hours (in-use databases refuse the DROP and are skipped).

use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};
use sqlx::migrate::Migrator;
use sqlx::{Connection, PgConnection};

/// The same embedded migrator `PostgresStore::migrate` runs
/// (`deploy/migrations`), so a template is byte-for-byte what a
/// freshly-migrated database would be.
static MIGRATOR: Migrator = sqlx::migrate!("../../deploy/migrations");

/// Session-scoped advisory-lock key serializing template bootstrap and
/// template clones across processes. Arbitrary but stable.
const SETUP_LOCK_KEY: i64 = 0x656e_6772_616d_5447; // "engramTG"

/// Leaked per-test databases older than this are swept at template
/// bootstrap on long-lived (dev) Postgres instances.
const STALE_AFTER_SECS: u64 = 3 * 60 * 60;

const TMPL_PREFIX: &str = "engram_test_tmpl_";
const DB_PREFIX: &str = "engram_test_";

/// A per-test database. Hold it for the life of the test; the database
/// itself is intentionally leaked afterwards (see module docs).
pub struct TestDb {
    /// Connection URL for this test's private database.
    pub url: String,
    /// The generated database name (`engram_test_<secs-hex>_<uuid>`).
    pub db_name: String,
    /// A connected store on `url`. Already migrated (via the template) —
    /// do not call `.migrate()` again; skipping it is the speed win.
    pub store: engram_postgres::PostgresStore,
}

/// Clone a fresh, fully-migrated database off `ENGRAM_TEST_DATABASE_URL`.
///
/// Returns `None` (after printing the standard skip line) when the env var
/// is unset, so `#[ignore]`'d live-PG tests keep their skip-not-fail
/// behavior:
///
/// ```ignore
/// let Some(db) = engram_testkit::pg::fresh_db().await else { return };
/// let meta: Arc<dyn MetadataStore> = Arc::new(db.store);
/// ```
pub async fn fresh_db() -> Option<TestDb> {
    let admin_url = match std::env::var("ENGRAM_TEST_DATABASE_URL") {
        Ok(v) => v,
        Err(_) => {
            eprintln!(
                "skipping: ENGRAM_TEST_DATABASE_URL not set. Bring up the dev DB with \
                 `docker compose -f deploy/docker-compose.dev.yml up -d postgres` and re-run with \
                 ENGRAM_TEST_DATABASE_URL=postgres://engram:engram@localhost:5435/engram"
            );
            return None;
        }
    };

    let mut admin = PgConnection::connect(&admin_url)
        .await
        .expect("testkit: connect postgres (admin)");
    // Session-scoped: released when `admin` closes, even if the test
    // process dies mid-bootstrap.
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(SETUP_LOCK_KEY)
        .execute(&mut admin)
        .await
        .expect("testkit: take setup advisory lock");

    let tmpl = template_name();
    let tmpl_exists: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_database WHERE datname = $1)")
            .bind(&tmpl)
            .fetch_one(&mut admin)
            .await
            .expect("testkit: probe template existence");
    if !tmpl_exists {
        sweep_stale(&mut admin, &tmpl).await;
        bootstrap_template(&mut admin, &admin_url, &tmpl).await;
    }

    let db_name = format!(
        "{DB_PREFIX}{:08x}_{}",
        now_secs(),
        uuid::Uuid::new_v4().simple()
    );
    sqlx::query(&format!(r#"CREATE DATABASE "{db_name}" TEMPLATE "{tmpl}""#))
        .execute(&mut admin)
        .await
        .expect("testkit: clone per-test database from template");

    // Release the advisory lock promptly rather than waiting for drop.
    admin
        .close()
        .await
        .expect("testkit: close admin connection");

    let url = with_db(&admin_url, &db_name);
    let store = engram_postgres::PostgresStore::connect(&url)
        .await
        .expect("testkit: connect per-test database");
    Some(TestDb {
        url,
        db_name,
        store,
    })
}

/// Create + migrate the template under a work-in-progress name, then
/// publish it with an atomic `ALTER DATABASE … RENAME`. If the process
/// dies mid-migrate, the half-built `_wip` database is never seen as a
/// valid template; the next bootstrap replaces it.
async fn bootstrap_template(admin: &mut PgConnection, admin_url: &str, tmpl: &str) {
    let wip = format!("{tmpl}_wip");
    sqlx::query(&format!(r#"DROP DATABASE IF EXISTS "{wip}""#))
        .execute(&mut *admin)
        .await
        .expect("testkit: drop stale wip template");
    sqlx::query(&format!(r#"CREATE DATABASE "{wip}""#))
        .execute(&mut *admin)
        .await
        .expect("testkit: create wip template");

    let mut conn = PgConnection::connect(&with_db(admin_url, &wip))
        .await
        .expect("testkit: connect wip template");
    MIGRATOR
        .run(&mut conn)
        .await
        .expect("testkit: migrate template");
    // `RENAME`/`TEMPLATE` require zero connections to the source, so close
    // (not just drop) before publishing.
    conn.close()
        .await
        .expect("testkit: close template connection");

    sqlx::query(&format!(r#"ALTER DATABASE "{wip}" RENAME TO "{tmpl}""#))
        .execute(admin)
        .await
        .expect("testkit: publish template");
}

/// Hash of the migrator's (version, checksum) pairs — a new or edited
/// migration mints a new template automatically.
fn template_name() -> String {
    let mut h = Sha256::new();
    for m in MIGRATOR.iter() {
        h.update(m.version.to_be_bytes());
        h.update(&m.checksum);
    }
    let digest = h.finalize();
    let mut fp = String::with_capacity(16);
    for b in &digest[..8] {
        fp.push_str(&format!("{b:02x}"));
    }
    format!("{TMPL_PREFIX}{fp}")
}

/// Drop leftovers from previous runs: templates for other migration sets,
/// and per-test databases older than [`STALE_AFTER_SECS`]. Runs only at
/// template bootstrap (i.e. after a migration change), always under the
/// setup lock. In-use databases make `DROP DATABASE` error; that is the
/// safety mechanism, so errors are deliberately ignored. Names that don't
/// parse as testkit-format (e.g. hand-made databases) are left alone.
async fn sweep_stale(admin: &mut PgConnection, current_tmpl: &str) {
    let names: Vec<String> = sqlx::query_scalar(
        "SELECT datname FROM pg_database WHERE datname LIKE 'engram\\_test\\_%'",
    )
    .fetch_all(&mut *admin)
    .await
    .unwrap_or_default();
    let now = now_secs();
    for name in names {
        let stale = if name == current_tmpl {
            false
        } else if name.starts_with(TMPL_PREFIX) {
            true // template (or wip) for a different migration set
        } else {
            match created_secs(&name) {
                Some(created) => now.saturating_sub(created) > STALE_AFTER_SECS,
                None => false,
            }
        };
        if stale {
            let _ = sqlx::query(&format!(r#"DROP DATABASE IF EXISTS "{name}""#))
                .execute(&mut *admin)
                .await;
        }
    }
}

/// Parse the creation timestamp out of `engram_test_<8-hex-secs>_<uuid>`.
fn created_secs(db_name: &str) -> Option<u64> {
    let rest = db_name.strip_prefix(DB_PREFIX)?;
    let (secs_hex, _uuid) = rest.split_once('_')?;
    if secs_hex.len() != 8 {
        return None;
    }
    u64::from_str_radix(secs_hex, 16).ok()
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock before unix epoch")
        .as_secs()
}

/// Swap the database path of a Postgres URL, preserving any query string.
fn with_db(url: &str, db: &str) -> String {
    let (rest, query) = match url.split_once('?') {
        Some((r, q)) => (r, Some(q)),
        None => (url, None),
    };
    let (base, _) = rest
        .rsplit_once('/')
        .expect("testkit: database url has a path component");
    match query {
        Some(q) => format!("{base}/{db}?{q}"),
        None => format!("{base}/{db}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn with_db_swaps_path_and_keeps_query() {
        assert_eq!(
            with_db("postgres://u:p@h:5435/engram", "x"),
            "postgres://u:p@h:5435/x"
        );
        assert_eq!(
            with_db("postgres://u:p@h:5435/engram?sslmode=disable", "x"),
            "postgres://u:p@h:5435/x?sslmode=disable"
        );
    }

    #[test]
    fn created_secs_parses_testkit_names_only() {
        assert_eq!(created_secs("engram_test_0000abcd_deadbeef"), Some(0xabcd));
        assert_eq!(created_secs("engram_test_deadbeef"), None); // legacy bespoke name
        assert_eq!(created_secs("engram_test_tmpl_0011223344556677"), None);
    }

    #[test]
    fn template_name_is_stable_and_bounded() {
        let a = template_name();
        assert_eq!(a, template_name());
        assert!(a.len() <= 63, "postgres identifier limit");
        // room for the `_wip` suffix too
        assert!(a.len() + 4 <= 63);
    }
}
