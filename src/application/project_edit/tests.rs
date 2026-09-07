use super::*;
use crate::{
    config::{ArtifactSpec, BuildCommand, DestinationSettings, HostKeyFingerprint},
    domain::{ComponentGeneration, ComponentName},
    drivers::CredentialHandle,
};

struct Fixture {
    directory: tempfile::TempDir,
    registry_path: PathBuf,
    original: ProjectConfig,
    destination: DestinationKey,
    service: ProjectEditService,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let destination = DestinationKey::new();
        let mut registry = DestinationRegistry::new();
        registry
            .create(destination.clone(), settings("fixture.invalid"))
            .unwrap();
        let registry_path = directory.path().join("destinations.yaml");
        registry.save(&registry_path).unwrap();
        let setup = ProjectSetup {
            project: "demo".into(),
            components: BTreeMap::from([(name("backend"), component())]),
            environments: BTreeMap::from([(
                "production".into(),
                EnvironmentSetup {
                    components: BTreeMap::from([(
                        name("backend"),
                        TargetSetup {
                            destination: destination.clone(),
                            root: None,
                            service: None,
                            health: None,
                            after: Vec::new(),
                        },
                    )]),
                },
            )]),
        };
        let original = config::initialize(directory.path(), setup).unwrap();
        let service = ProjectEditService::new(
            registry_path.clone(),
            Arc::new(DeploymentSession::default()),
        );
        Self {
            directory,
            registry_path,
            original,
            destination,
            service,
        }
    }

    fn path(&self) -> PathBuf {
        self.directory.path().join(config::PROJECT_FILE)
    }

    async fn draft(&self) -> ProjectEditDraft {
        self.service
            .load(self.directory.path(), &CancellationToken::new())
            .await
            .unwrap()
    }

    async fn preview(&self, draft: ProjectEditDraft) -> ProjectEditPreview {
        self.service
            .preview(draft, &CancellationToken::new())
            .await
            .unwrap()
    }

    async fn save(&self, preview: ProjectEditPreview) -> ProjectConfig {
        self.service
            .save(preview, &CancellationToken::new())
            .await
            .unwrap()
    }
}

fn name(value: &str) -> ComponentName {
    ComponentName::parse(value).unwrap()
}

fn component() -> ComponentSetup {
    ComponentSetup {
        working_directory: None,
        build: vec![BuildCommand::argv("cargo", ["build"])],
        artifact: ArtifactSpec {
            path: "target/backend".into(),
        },
    }
}

fn settings(host: &str) -> DestinationSettings {
    DestinationSettings::LinuxSsh {
        host: host.into(),
        port: 22,
        user: "deploy".into(),
        credential: CredentialHandle::new(),
        host_key: HostKeyFingerprint::parse("SHA256:fixture").unwrap(),
    }
}

#[tokio::test]
async fn load_and_preview_never_write_and_rename_preserves_original_identity_and_root() {
    let fixture = Fixture::new();
    let before = fs::read(fixture.path()).unwrap();
    let mut draft = fixture.draft().await;
    assert_eq!(draft.original(), &fixture.original);
    assert_eq!(draft.destinations().len(), 1);
    draft.setup.project = "renamed".into();
    let environment = draft.setup.environments.remove("production").unwrap();
    draft.setup.environments.insert("live".into(), environment);
    draft.environment_renames.push(EnvironmentRename {
        from: "production".into(),
        to: "live".into(),
    });
    let preview = fixture.preview(draft).await;
    assert_eq!(fs::read(fixture.path()).unwrap(), before);
    assert_eq!(preview.config().project_id, fixture.original.project_id);
    assert_eq!(
        preview.config().environments["live"],
        fixture.original.environments["production"]
    );
    let yaml = preview.yaml().to_owned();
    let config = fixture.save(preview).await;
    assert_eq!(fs::read_to_string(fixture.path()).unwrap(), yaml);
    assert_eq!(config.project, "renamed");
    assert!(config.environments.contains_key("live"));
}

#[tokio::test]
async fn added_environment_identity_is_generated_once_in_the_confirmed_preview() {
    let fixture = Fixture::new();
    let mut draft = fixture.draft().await;
    let mut target = draft.setup.environments["production"].components[&name("backend")].clone();
    target.root = None;
    draft.setup.environments.insert(
        "staging".into(),
        EnvironmentSetup {
            components: BTreeMap::from([(name("backend"), target)]),
        },
    );
    let preview = fixture.preview(draft).await;
    let expected = preview.config().clone();
    assert_ne!(
        expected.environments["staging"].id,
        expected.environments["production"].id
    );
    assert_eq!(
        expected.environments["staging"].components[&name("backend")].root,
        "/srv/shipforge/demo/staging/backend"
    );
    let yaml = preview.yaml().to_owned();
    assert_eq!(fixture.save(preview).await, expected);
    assert_eq!(fs::read_to_string(fixture.path()).unwrap(), yaml);
}

#[tokio::test]
async fn legacy_systemd_conversion_is_previewed_without_identity_or_generation_changes() {
    let fixture = Fixture::new();
    let mut yaml: serde_yaml_ng::Value =
        serde_yaml_ng::from_str(&fs::read_to_string(fixture.path()).unwrap()).unwrap();
    yaml["schemaVersion"] = 1.into();
    yaml["environments"]["production"]["components"]["backend"]["systemd"] = "api.service".into();
    let old = serde_yaml_ng::to_string(&yaml).unwrap();
    fs::write(fixture.path(), &old).unwrap();
    let draft = fixture.draft().await;
    let preview = fixture.preview(draft).await;
    assert_eq!(fs::read_to_string(fixture.path()).unwrap(), old);
    assert_eq!(preview.config().schema_version, 2);
    assert_eq!(preview.config().project_id, fixture.original.project_id);
    assert_eq!(
        preview.config().environments["production"].id,
        fixture.original.environments["production"].id
    );
    let target = &preview.config().environments["production"].components[&name("backend")];
    assert_eq!(target.generation, ComponentGeneration::INITIAL);
    assert_eq!(
        target.service.as_ref().unwrap().preset_unit(),
        Some("api.service")
    );
    assert!(!preview.yaml().contains("systemd: api.service"));
    let expected = preview.config().clone();
    assert_eq!(fixture.save(preview).await, expected);
    assert_eq!(
        config::load(fixture.directory.path()).unwrap(),
        config::ProjectConfigState::Loaded(expected)
    );
}

#[tokio::test]
async fn custom_service_changes_are_frozen_and_increment_generation() {
    let fixture = Fixture::new();
    let mut draft = fixture.draft().await;
    let target = draft
        .setup
        .environments
        .get_mut("production")
        .unwrap()
        .components
        .get_mut(&name("backend"))
        .unwrap();
    let mut service = crate::config::ServiceConfig::systemd("api.service");
    service.start = vec![vec![
        "pm2".into(),
        "start".into(),
        "ecosystem.config.cjs".into(),
    ]];
    service.stop = vec![vec!["pm2".into(), "delete".into(), "api".into()]];
    service.check = None;
    target.service = Some(service.clone());
    let before = fs::read(fixture.path()).unwrap();
    let preview = fixture.preview(draft).await;
    assert_eq!(fs::read(fixture.path()).unwrap(), before);
    let config = fixture.save(preview).await;
    let target = &config.environments["production"].components[&name("backend")];
    assert_eq!(target.generation.get(), 2);
    assert_eq!(target.service, Some(service));
}

#[tokio::test]
async fn build_only_changes_preserve_generation_and_target_changes_increment_it() {
    for target_change in [false, true] {
        let fixture = Fixture::new();
        let mut draft = fixture.draft().await;
        let build = draft.setup.components.get_mut(&name("backend")).unwrap();
        build.build[0].args.push("--release".into());
        build.artifact.path = "target/release/backend".into();
        if target_change {
            draft
                .setup
                .environments
                .get_mut("production")
                .unwrap()
                .components
                .get_mut(&name("backend"))
                .unwrap()
                .root = Some("/srv/new-root/backend".into());
        }
        let preview = fixture.preview(draft).await;
        let generation =
            preview.config().environments["production"].components[&name("backend")].generation;
        assert_eq!(
            generation,
            if target_change {
                ComponentGeneration::INITIAL.checked_next().unwrap()
            } else {
                ComponentGeneration::INITIAL
            }
        );
        fixture.save(preview).await;
    }
}

#[tokio::test]
async fn selected_connection_change_increments_generation_without_rewriting_registry() {
    let fixture = Fixture::new();
    let second = DestinationKey::new();
    let mut registry = DestinationRegistry::load(&fixture.registry_path).unwrap();
    registry
        .create(second.clone(), settings("second.invalid"))
        .unwrap();
    registry.save(&fixture.registry_path).unwrap();
    let registry_bytes = fs::read(&fixture.registry_path).unwrap();
    let mut draft = fixture.draft().await;
    draft
        .setup
        .environments
        .get_mut("production")
        .unwrap()
        .components
        .get_mut(&name("backend"))
        .unwrap()
        .destination = second.clone();
    let preview = fixture.preview(draft).await;
    assert_eq!(preview.destinations().len(), 2);
    let saved = fixture.save(preview).await;
    let target = &saved.environments["production"].components[&name("backend")];
    assert_eq!(target.destination, second);
    assert_eq!(
        target.generation,
        ComponentGeneration::INITIAL.checked_next().unwrap()
    );
    assert_eq!(fs::read(&fixture.registry_path).unwrap(), registry_bytes);
}

#[tokio::test]
async fn component_replacement_is_new_identity_and_retains_no_old_target() {
    let fixture = Fixture::new();
    let mut draft = fixture.draft().await;
    draft.setup.components.remove(&name("backend"));
    draft.setup.components.insert(name("worker"), component());
    let environment = draft.setup.environments.get_mut("production").unwrap();
    let mut target = environment.components.remove(&name("backend")).unwrap();
    target.root = None;
    environment.components.insert(name("worker"), target);
    let saved = fixture.save(fixture.preview(draft).await).await;
    assert!(!saved.components.contains_key(&name("backend")));
    let target = &saved.environments["production"].components[&name("worker")];
    assert_eq!(target.generation, ComponentGeneration::INITIAL);
    assert_eq!(target.root, "/srv/shipforge/demo/production/worker");
    assert_eq!(
        saved.environments["production"].id,
        fixture.original.environments["production"].id
    );
}

#[tokio::test]
async fn cancelled_load_preview_and_save_never_write_config() {
    let fixture = Fixture::new();
    let draft = fixture.draft().await;
    let preview = fixture.preview(draft.clone()).await;
    let before = fs::read(fixture.path()).unwrap();
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert_eq!(
        fixture
            .service
            .load(fixture.directory.path(), &cancel)
            .await
            .unwrap_err(),
        ProjectEditError::Cancelled
    );
    assert_eq!(
        fixture.service.preview(draft, &cancel).await.unwrap_err(),
        ProjectEditError::Cancelled
    );
    assert_eq!(
        fixture.service.save(preview, &cancel).await.unwrap_err(),
        ProjectEditError::Cancelled
    );
    assert_eq!(fs::read(fixture.path()).unwrap(), before);
}

#[tokio::test]
async fn stale_yaml_and_registry_changes_are_not_overwritten() {
    for registry_change in [false, true] {
        let fixture = Fixture::new();
        let mut draft = fixture.draft().await;
        draft.setup.project = "updated".into();
        let preview = fixture.preview(draft.clone()).await;
        if registry_change {
            let mut registry = DestinationRegistry::load(&fixture.registry_path).unwrap();
            registry
                .revise(&fixture.destination, settings("changed.invalid"))
                .unwrap();
            registry.save(&fixture.registry_path).unwrap();
        } else {
            let mut contents = fs::read_to_string(fixture.path()).unwrap();
            contents.push_str("\n# concurrent user edit\n");
            fs::write(fixture.path(), contents).unwrap();
        }
        let before = fs::read(fixture.path()).unwrap();
        assert_eq!(
            fixture
                .service
                .preview(draft, &CancellationToken::new())
                .await
                .unwrap_err(),
            ProjectEditError::Stale
        );
        assert_eq!(
            fixture
                .service
                .save(preview, &CancellationToken::new())
                .await
                .unwrap_err(),
            ProjectEditError::Stale
        );
        assert_eq!(fs::read(fixture.path()).unwrap(), before);
    }
}

#[tokio::test]
async fn identical_bytes_in_a_replaced_file_invalidate_the_edit() {
    let fixture = Fixture::new();
    let preview = fixture.preview(fixture.draft().await).await;
    let bytes = fs::read(fixture.path()).unwrap();
    let mut replacement = AtomicWriteFile::open(fixture.path()).unwrap();
    replacement.write_all(&bytes).unwrap();
    replacement.commit().unwrap();
    assert_eq!(
        fixture
            .service
            .save(preview, &CancellationToken::new())
            .await
            .unwrap_err(),
        ProjectEditError::Stale
    );
    assert_eq!(fs::read(fixture.path()).unwrap(), bytes);
}

#[tokio::test]
async fn busy_session_does_not_poll_load_discovery_preview_or_save() {
    let fixture = Fixture::new();
    let draft = fixture.draft().await;
    let preview = fixture.preview(draft.clone()).await;
    let before = fs::read(fixture.path()).unwrap();
    fixture
        .service
        .session
        .run(async {
            let cancel = CancellationToken::new();
            assert_eq!(
                fixture
                    .service
                    .load(fixture.directory.path(), &cancel)
                    .await
                    .unwrap_err(),
                ProjectEditError::Busy
            );
            assert_eq!(
                fixture.service.discover(&draft, &cancel).await.unwrap_err(),
                ProjectEditError::Busy
            );
            assert_eq!(
                fixture.service.preview(draft, &cancel).await.unwrap_err(),
                ProjectEditError::Busy
            );
            assert_eq!(
                fixture.service.save(preview, &cancel).await.unwrap_err(),
                ProjectEditError::Busy
            );
        })
        .await
        .unwrap();
    assert_eq!(fs::read(fixture.path()).unwrap(), before);
}

#[tokio::test]
async fn missing_corrupt_oversized_and_nonregular_inputs_fail_closed() {
    for fault in 0..4 {
        let fixture = Fixture::new();
        match fault {
            0 => fs::remove_file(fixture.path()).unwrap(),
            1 => fs::write(fixture.path(), "project: SECRET_SENTINEL").unwrap(),
            2 => fs::write(fixture.path(), vec![b'x'; MAX_FILE_BYTES + 1]).unwrap(),
            _ => {
                fs::remove_file(fixture.path()).unwrap();
                fs::create_dir(fixture.path()).unwrap();
            }
        }
        let error = fixture
            .service
            .load(fixture.directory.path(), &CancellationToken::new())
            .await
            .unwrap_err();
        assert!(!format!("{error:?} {error}").contains("SECRET_SENTINEL"));
        if fault == 0 {
            assert!(!fixture.path().exists());
        }
    }
}

#[tokio::test]
async fn hard_linked_config_is_not_read_or_replaced() {
    let fixture = Fixture::new();
    let external = fixture.directory.path().join("linked-copy");
    fs::hard_link(fixture.path(), &external).unwrap();
    let before = fs::read(&external).unwrap();
    assert_eq!(
        fixture
            .service
            .load(fixture.directory.path(), &CancellationToken::new())
            .await
            .unwrap_err(),
        ProjectEditError::UnsafePath
    );
    assert_eq!(fs::read(external).unwrap(), before);
}

#[cfg(unix)]
#[tokio::test]
async fn symlinked_config_registry_and_project_directory_are_rejected() {
    use std::os::unix::fs::symlink;
    for entry in 0..3 {
        let fixture = Fixture::new();
        let path = if entry == 0 {
            fixture.path()
        } else {
            fixture.registry_path.clone()
        };
        if entry < 2 {
            let contents = fs::read(&path).unwrap();
            let saved = fixture.directory.path().join("original");
            fs::rename(&path, &saved).unwrap();
            symlink(&saved, &path).unwrap();
            assert!(
                fixture
                    .service
                    .load(fixture.directory.path(), &CancellationToken::new())
                    .await
                    .is_err()
            );
            assert_eq!(fs::read(saved).unwrap(), contents);
        } else {
            let alias = fixture.directory.path().join("linked-root");
            symlink(fixture.directory.path(), &alias).unwrap();
            assert_eq!(
                fixture
                    .service
                    .load(&alias, &CancellationToken::new())
                    .await
                    .unwrap_err(),
                ProjectEditError::UnsafePath
            );
        }
    }
}

#[tokio::test]
async fn invalid_user_drafts_are_not_saved_and_diagnostics_do_not_echo_inputs() {
    for fault in 0..7 {
        let fixture = Fixture::new();
        let mut draft = fixture.draft().await;
        match fault {
            0 => {
                draft
                    .setup
                    .components
                    .get_mut(&name("backend"))
                    .unwrap()
                    .build[0] = BuildCommand::shell("SECRET_SENTINEL");
            }
            1 => draft.setup.project = "SECRET_SENTINEL".into(),
            2 => draft
                .setup
                .components
                .get_mut(&name("backend"))
                .unwrap()
                .build[0]
                .args
                .push("-----BEGIN OPENSSH PRIVATE KEY-----SECRET_SENTINEL".into()),
            3 => {
                draft
                    .setup
                    .components
                    .get_mut(&name("backend"))
                    .unwrap()
                    .artifact
                    .path = "../SECRET_SENTINEL".into();
            }
            4 => {
                draft
                    .setup
                    .environments
                    .get_mut("production")
                    .unwrap()
                    .components
                    .get_mut(&name("backend"))
                    .unwrap()
                    .destination = DestinationKey::new();
            }
            5 => draft.environment_renames.push(EnvironmentRename {
                from: "production".into(),
                to: "missing".into(),
            }),
            _ => draft
                .setup
                .components
                .get_mut(&name("backend"))
                .unwrap()
                .build[0]
                .args
                .push("x".repeat(MAX_FIELD_BYTES + 1)),
        }
        let before = fs::read(fixture.path()).unwrap();
        assert!(!format!("{draft:?}").contains("SECRET_SENTINEL"));
        let error = fixture
            .service
            .preview(draft, &CancellationToken::new())
            .await
            .unwrap_err();
        assert!(!format!("{error:?} {error}").contains("SECRET_SENTINEL"));
        assert_eq!(fs::read(fixture.path()).unwrap(), before);
    }
}

#[tokio::test]
async fn discovery_offers_candidates_without_running_scripts_or_modifying_draft() {
    let fixture = Fixture::new();
    fs::write(
        fixture.directory.path().join("package.json"),
        r#"{"name":"web","scripts":{"build":"vite build"}}"#,
    )
    .unwrap();
    let draft = fixture.draft().await;
    let before = fs::read(fixture.path()).unwrap();
    let report = fixture
        .service
        .discover(&draft, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(report.components.len(), 1);
    assert_eq!(report.components[0].name, name("web"));
    assert_eq!(draft.setup.components.len(), 1);
    assert_eq!(fs::read(fixture.path()).unwrap(), before);
}
