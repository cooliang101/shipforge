use super::*;

fn create_deployment(store: &HistoryStore) -> DeploymentId {
    let deployment = DeploymentId::new();
    store
        .create_deployment(&deployment, &ProjectId::new(), &EnvironmentId::new(), 1)
        .unwrap();
    deployment
}

#[test]
fn registered_log_round_trips_generated_relative_path_and_rotation_limits() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("history.sqlite3");
    let store = HistoryStore::open(&path).unwrap();
    let deployment = create_deployment(&store);
    assert!(store.deployment_log(&deployment).unwrap().is_none());
    let registered = store
        .register_deployment_log(&deployment, 1024 * 1024, 3)
        .unwrap();
    assert_eq!(
        registered.relative_path,
        PathBuf::from(format!("logs/{deployment}.log"))
    );
    assert_eq!(registered.max_bytes, 1024 * 1024);
    assert_eq!(registered.retained_files, 3);
    assert_eq!(registered.deployment, deployment);
    drop(store);
    let reopened = HistoryStore::open(&path).unwrap();
    assert_eq!(reopened.schema_version().unwrap(), 7);
    assert_eq!(
        reopened.deployment_log(&deployment).unwrap(),
        Some(registered)
    );
}

#[test]
fn log_index_requires_an_existing_deployment_and_cannot_replace_its_registration() {
    let store = HistoryStore::in_memory().unwrap();
    assert!(
        store
            .register_deployment_log(&DeploymentId::new(), 1024, 3)
            .is_err()
    );
    let deployment = create_deployment(&store);
    let registered = store.register_deployment_log(&deployment, 1024, 3).unwrap();
    assert!(store.register_deployment_log(&deployment, 4096, 1).is_err());
    assert_eq!(store.deployment_log(&deployment).unwrap(), Some(registered));
    assert!(
        store
            .connection
            .execute(
                "DELETE FROM deployments WHERE id=?1",
                [deployment.to_string()]
            )
            .is_err()
    );
}

#[test]
fn log_registration_rejects_invalid_limits_without_leaving_metadata() {
    let store = HistoryStore::in_memory().unwrap();
    let deployment = create_deployment(&store);
    for (max_bytes, retained_files) in [(0, 3), (1024, 0), (u64::MAX, 3)] {
        assert!(matches!(
            store.register_deployment_log(&deployment, max_bytes, retained_files),
            Err(HistoryError::InvalidLogLimits)
        ));
        assert!(store.deployment_log(&deployment).unwrap().is_none());
    }
}

#[test]
fn migration_from_version_three_preserves_existing_deployments() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("history.sqlite3");
    let connection = Connection::open(&path).unwrap();
    connection.execute_batch(MIGRATION_1).unwrap();
    connection.execute_batch(MIGRATION_2).unwrap();
    connection.execute_batch(MIGRATION_3).unwrap();
    connection
        .pragma_update(None, "user_version", 3_u32)
        .unwrap();
    let deployment = DeploymentId::new();
    connection.execute(
        "INSERT INTO deployments (id,project_id,environment_id,state,created_at_ms,updated_at_ms)
         VALUES (?1,?2,?3,'created',1,1)",
        params![deployment.to_string(), ProjectId::new().to_string(), EnvironmentId::new().to_string()],
    ).unwrap();
    drop(connection);
    let store = HistoryStore::open(&path).unwrap();
    assert_eq!(store.schema_version().unwrap(), 7);
    assert_eq!(
        store.deployment_state(&deployment).unwrap().as_deref(),
        Some("created")
    );
    assert_eq!(
        store.deployment_kind(&deployment).unwrap(),
        Some(("deploy".into(), None))
    );
    assert!(store.deployment_log(&deployment).unwrap().is_none());
    store.register_deployment_log(&deployment, 1024, 3).unwrap();
}

#[test]
fn generated_log_paths_cannot_escape_the_local_logs_directory() {
    let store = HistoryStore::in_memory().unwrap();
    let deployment = create_deployment(&store);
    store.register_deployment_log(&deployment, 1024, 3).unwrap();
    let sql = "UPDATE deployment_logs SET relative_path='../other-file' WHERE deployment_id=?1";
    assert!(
        store
            .connection
            .execute(sql, [deployment.to_string()])
            .is_err()
    );
    // A damaged or externally edited database must not turn a displayed log
    // reference into a path outside the application's log directory either.
    store
        .connection
        .pragma_update(None, "ignore_check_constraints", true)
        .unwrap();
    store
        .connection
        .execute(sql, [deployment.to_string()])
        .unwrap();
    assert!(matches!(
        store.deployment_log(&deployment),
        Err(HistoryError::Corrupt(_))
    ));
}

#[test]
fn log_lookup_keeps_distinct_deployments_separate() {
    let store = HistoryStore::in_memory().unwrap();
    let first = create_deployment(&store);
    let second = create_deployment(&store);
    let first_log = store.register_deployment_log(&first, 1024, 3).unwrap();
    assert!(store.deployment_log(&second).unwrap().is_none());
    let second_log = store.register_deployment_log(&second, 2048, 1).unwrap();
    assert_ne!(first_log.relative_path, second_log.relative_path);
    assert_eq!(store.deployment_log(&first).unwrap(), Some(first_log));
    assert_eq!(store.deployment_log(&second).unwrap(), Some(second_log));
}

#[test]
fn event_format_is_explicit_and_cannot_relabel_legacy_or_accept_unknown_formats() {
    let store = HistoryStore::in_memory().unwrap();
    let legacy = create_deployment(&store);
    let current = create_deployment(&store);
    assert_eq!(
        store
            .register_deployment_log(&legacy, 1024, 3)
            .unwrap()
            .format,
        DeploymentLogFormat::LegacyText
    );
    assert_eq!(
        store.register_event_log(&current, 1024, 3).unwrap().format,
        DeploymentLogFormat::JsonlV1
    );
    assert!(store.register_event_log(&legacy, 1024, 3).is_err());
    assert!(
        store
            .connection
            .execute(
                "UPDATE deployment_logs SET format='guessed_json' WHERE deployment_id=?1",
                [legacy.to_string()]
            )
            .is_err()
    );
    store
        .connection
        .pragma_update(None, "ignore_check_constraints", true)
        .unwrap();
    store
        .connection
        .execute(
            "UPDATE deployment_logs SET format='guessed_json' WHERE deployment_id=?1",
            [legacy.to_string()],
        )
        .unwrap();
    assert!(matches!(
        store.deployment_log(&legacy),
        Err(HistoryError::Corrupt(_))
    ));
}

#[test]
fn schema_six_is_read_only_rejected_and_write_migration_preserves_legacy_logs() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("history.sqlite3");
    let mut connection = Connection::open(&path).unwrap();
    for migration in [
        MIGRATION_1,
        MIGRATION_2,
        MIGRATION_3,
        MIGRATION_4,
        details::MIGRATION_5,
    ] {
        connection.execute_batch(migration).unwrap();
    }
    let transaction = connection.transaction().unwrap();
    recovery::migrate(&transaction).unwrap();
    transaction
        .pragma_update(None, "user_version", 6_u32)
        .unwrap();
    transaction.commit().unwrap();
    let id = DeploymentId::new();
    connection.execute("INSERT INTO deployments (id,project_id,environment_id,state,created_at_ms,updated_at_ms) VALUES (?1,?2,?3,'failed',1,1)", params![id.to_string(),ProjectId::new().to_string(),EnvironmentId::new().to_string()]).unwrap();
    connection
        .execute(
            "INSERT INTO deployment_logs VALUES (?1,?2,1024,3)",
            params![id.to_string(), format!("logs/{id}.log")],
        )
        .unwrap();
    assert!(matches!(
        HistoryStore::open_existing_read_only(&path),
        Err(HistoryError::InvalidMetadata(_))
    ));
    assert_eq!(
        connection
            .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
            .unwrap(),
        6
    );
    drop(connection);
    let store = HistoryStore::open(&path).unwrap();
    assert_eq!(store.schema_version().unwrap(), 7);
    assert_eq!(
        store.deployment_log(&id).unwrap().unwrap().format,
        DeploymentLogFormat::LegacyText
    );
    assert_eq!(
        store.deployment_state(&id).unwrap().as_deref(),
        Some("failed")
    );
    assert!(!directory.path().join("logs").exists());
}
