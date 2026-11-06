#[test]
fn migration_versions_are_unique_and_increasing() {
    let migrations = sqlx::migrate!("../../deploy/migrations");
    let mut previous = None;
    for migration in migrations.iter() {
        assert!(
            previous.is_none_or(|version| migration.version > version),
            "migration version {} is duplicated or out of order",
            migration.version,
        );
        previous = Some(migration.version);
    }
}
