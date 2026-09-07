use std::{collections::BTreeMap, fs};

use serde_yaml_ng::Value;

use super::*;
use crate::{
    config::{
        ArtifactSpec, BuildCommand, ComponentSetup, DestinationSettings, EnvironmentSetup,
        HostKeyFingerprint, ProjectSetup, TargetSetup,
    },
    domain::{ComponentName, DestinationKey},
    drivers::CredentialHandle,
};

struct Fixture {
    directory: tempfile::TempDir,
    original: ProjectConfig,
    registry: PathBuf,
    session: Arc<DeploymentSession>,
    service: ProjectReinitializeService,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let destination = DestinationKey::new();
        let mut registry = DestinationRegistry::new();
        registry
            .create(
                destination.clone(),
                DestinationSettings::LinuxSsh {
                    host: "fixture.invalid".into(),
                    port: 22,
                    user: "deploy".into(),
                    credential: CredentialHandle::new(),
                    host_key: HostKeyFingerprint::parse("SHA256:fixture").unwrap(),
                },
            )
            .unwrap();
        let registry_path = directory.path().join("destinations.yaml");
        registry.save(&registry_path).unwrap();
        let original = config::initialize(
            directory.path(),
            ProjectSetup {
                project: "demo".into(),
                components: BTreeMap::from([(
                    name(),
                    ComponentSetup {
                        working_directory: Some("backend".into()),
                        build: vec![BuildCommand::argv("cargo", ["build"])],
                        artifact: ArtifactSpec {
                            path: "target/backend".into(),
                        },
                    },
                )]),
                environments: BTreeMap::from([(
                    "production".into(),
                    EnvironmentSetup {
                        components: BTreeMap::from([(
                            name(),
                            TargetSetup {
                                destination,
                                root: None,
                                service: Some("demo.service".into()),
                                health: Some("http://127.0.0.1:8080/health".into()),
                                after: Vec::new(),
                            },
                        )]),
                    },
                )]),
            },
        )
        .unwrap();
        let session = Arc::new(DeploymentSession::default());
        let service = ProjectReinitializeService::new(registry_path.clone(), session.clone());
        Self {
            directory,
            original,
            registry: registry_path,
            session,
            service,
        }
    }

    fn root(&self) -> &Path {
        self.directory.path()
    }
    fn path(&self) -> PathBuf {
        self.root().join(config::PROJECT_FILE)
    }
    fn edit(&self, apply: impl FnOnce(&mut Value)) {
        let mut yaml = serde_yaml_ng::from_str(&fs::read_to_string(self.path()).unwrap()).unwrap();
        apply(&mut yaml);
        fs::write(self.path(), serde_yaml_ng::to_string(&yaml).unwrap()).unwrap();
    }
    fn remove_managed(&self) {
        self.edit(|yaml| {
            yaml.as_mapping_mut()
                .unwrap()
                .remove(Value::String("_shipforge".into()));
        });
    }
    async fn preview(&self) -> ProjectReinitializePreview {
        self.service
            .preview(self.root(), &CancellationToken::new())
            .await
            .unwrap()
    }
    async fn save(
        &self,
        preview: ProjectReinitializePreview,
    ) -> Result<ProjectConfig, ProjectReinitializeError> {
        self.service
            .save(
                preview,
                ReinitializeConfirmation::confirmed(),
                &CancellationToken::new(),
            )
            .await
    }
}

fn name() -> ComponentName {
    ComponentName::parse("backend").unwrap()
}

#[tokio::test]
async fn missing_managed_preview_is_read_only_and_save_uses_exact_fresh_identity() {
    let fixture = Fixture::new();
    fixture.remove_managed();
    let before = fs::read(fixture.path()).unwrap();
    let registry_before = fs::read(&fixture.registry).unwrap();
    let preview = fixture.preview().await;
    assert_eq!(fs::read(fixture.path()).unwrap(), before);
    assert_ne!(preview.config().project_id, fixture.original.project_id);
    assert_ne!(
        preview.config().environments["production"].id,
        fixture.original.environments["production"].id
    );
    assert_eq!(preview.config().components, fixture.original.components);
    assert_eq!(
        preview.config().environments["production"].components,
        fixture.original.environments["production"].components
    );
    assert_eq!(preview.destinations().len(), 1);
    let config = preview.config().clone();
    let yaml = preview.yaml().to_owned();
    let replay = preview.clone();
    assert_eq!(fixture.save(preview).await.unwrap(), config);
    assert_eq!(fs::read_to_string(fixture.path()).unwrap(), yaml);
    assert_eq!(fs::read(&fixture.registry).unwrap(), registry_before);
    assert_eq!(
        fixture.save(replay).await,
        Err(ProjectReinitializeError::Stale)
    );
    assert_eq!(fs::read_dir(fixture.root()).unwrap().count(), 2);
}

#[tokio::test]
async fn damaged_ids_preserve_surviving_frozen_roots_after_project_rename() {
    let fixture = Fixture::new();
    fixture.edit(|yaml| {
        yaml["project"] = "renamed".into();
        yaml["_shipforge"]["projectId"] = "broken".into();
        yaml["_shipforge"]["environments"]["production"]["id"] = "broken".into();
        yaml["_shipforge"]["environments"]["production"]["components"]["backend"]["generation"] =
            99.into();
    });
    let preview = fixture.preview().await;
    let target = &preview.config().environments["production"].components[&name()];
    assert_eq!(target.root, "/srv/shipforge/demo/production/backend");
    assert_eq!(
        target.generation,
        fixture.original.environments["production"].components[&name()].generation
    );
    let expected = preview.config().clone();
    assert_eq!(fixture.save(preview).await.unwrap(), expected);
}

#[tokio::test]
async fn explicit_root_can_supply_missing_managed_root_but_disagreement_is_rejected() {
    for conflict in [false, true] {
        let fixture = Fixture::new();
        fixture.edit(|yaml| {
            yaml["_shipforge"]["projectId"] = "broken".into();
            yaml["environments"]["production"]["components"]["backend"]["root"] =
                "/srv/explicit/backend".into();
            if !conflict {
                yaml["_shipforge"]["environments"]["production"]["components"]["backend"] =
                    Value::Null;
            }
        });
        let before = fs::read(fixture.path()).unwrap();
        let result = fixture
            .service
            .preview(fixture.root(), &CancellationToken::new())
            .await;
        if conflict {
            assert!(matches!(result, Err(ProjectReinitializeError::Invalid(_))));
        } else {
            assert_eq!(
                result.unwrap().config().environments["production"].components[&name()].root,
                "/srv/explicit/backend"
            );
        }
        assert_eq!(fs::read(fixture.path()).unwrap(), before);
    }
}

#[tokio::test]
async fn valid_and_missing_yaml_never_enter_reinitialization() {
    let fixture = Fixture::new();
    assert_eq!(
        fixture
            .service
            .preview(fixture.root(), &CancellationToken::new())
            .await
            .unwrap_err(),
        ProjectReinitializeError::NotRequired
    );
    fs::remove_file(fixture.path()).unwrap();
    assert_eq!(
        fixture
            .service
            .preview(fixture.root(), &CancellationToken::new())
            .await
            .unwrap_err(),
        ProjectReinitializeError::Missing
    );
    assert!(!fixture.path().exists());
}

#[tokio::test]
async fn malformed_human_intent_and_sensitive_or_ambiguous_managed_state_are_rejected_without_echo()
{
    for case in 0..12 {
        let fixture = Fixture::new();
        fixture.edit(|yaml| {
            yaml["_shipforge"]["projectId"] = "broken".into();
            match case {
                0 => yaml["schemaVersion"] = 99.into(),
                1 => yaml["components"]["backend"]["build"] = "secret-sentinel".into(),
                2 => yaml["components"]["backend"]["artifact"] = "../secret-sentinel".into(),
                3 => yaml["secret-sentinel"] = "must-not-echo".into(),
                4 => yaml["environments"]["production"]["components"]["backend"]["to"] = "secret-sentinel".into(),
                5 => yaml["environments"]["production"]["components"]["backend"]["after"] = Value::Sequence(vec!["unknown".into()]),
                6 => yaml["_shipforge"]["environments"]["production"]["components"]["backend"]["resolvedRoot"] = 17.into(),
                7 => yaml["_shipforge"]["environments"]["production"]["components"]["backend"]["resolvedRoot"] = "/srv/../secret-sentinel".into(),
                8 => yaml["_shipforge"]["environments"]["production"]["components"]["backend"] = Value::Null,
                9 => yaml["components"]["backend"]["unknown"] = "secret-sentinel".into(),
                10 => yaml["components"]["backend"]["build"] = Value::Sequence(Vec::new()),
                _ => yaml["_shipforge"]["privateKey"] = "secret-sentinel".into(),
            }
        });
        let before = fs::read(fixture.path()).unwrap();
        let error = fixture
            .service
            .preview(fixture.root(), &CancellationToken::new())
            .await
            .unwrap_err();
        assert!(!format!("{error:?} {error}").contains("secret-sentinel"));
        assert_eq!(fs::read(fixture.path()).unwrap(), before);
    }
}

#[tokio::test]
async fn source_registry_or_service_changes_invalidate_the_exact_preview() {
    for case in 0..4 {
        let fixture = Fixture::new();
        fixture.remove_managed();
        let preview = fixture.preview().await;
        match case {
            0 => fixture.edit(|yaml| yaml["project"] = "changed".into()),
            1 => {
                let text = fs::read_to_string(&fixture.registry).unwrap();
                fs::write(&fixture.registry, format!("{text}\n# external edit\n")).unwrap();
            }
            2 => {
                let path = fixture.path();
                let text = fs::read(&path).unwrap();
                fs::rename(&path, fixture.root().join("external-copy.yaml")).unwrap();
                fs::write(path, text).unwrap();
            }
            _ => {}
        }
        let source_before = fs::read(fixture.path()).unwrap();
        let result = if case == 3 {
            ProjectReinitializeService::new(
                fixture.root().join("other-registry.yaml"),
                fixture.session.clone(),
            )
            .save(
                preview,
                ReinitializeConfirmation::confirmed(),
                &CancellationToken::new(),
            )
            .await
        } else {
            fixture.save(preview).await
        };
        assert_eq!(result, Err(ProjectReinitializeError::Stale));
        assert_eq!(fs::read(fixture.path()).unwrap(), source_before);
    }
}

#[tokio::test]
async fn directory_replacement_is_rejected_before_any_write() {
    let fixture = Fixture::new();
    fixture.remove_managed();
    let preview = fixture.preview().await;
    let moved_parent = tempfile::tempdir().unwrap();
    let moved = moved_parent.path().join("moved");
    let before = fs::read(fixture.path()).unwrap();
    fs::rename(fixture.root(), &moved).unwrap();
    fs::create_dir(fixture.root()).unwrap();
    fs::write(fixture.path(), &before).unwrap();
    assert_eq!(
        fixture.save(preview).await,
        Err(ProjectReinitializeError::Stale)
    );
    assert_eq!(fs::read(fixture.path()).unwrap(), before);
}

#[tokio::test]
async fn cancellation_and_busy_sessions_never_replace_configuration() {
    let fixture = Fixture::new();
    fixture.remove_managed();
    let preview = fixture.preview().await;
    let before = fs::read(fixture.path()).unwrap();
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    assert_eq!(
        fixture
            .service
            .preview(fixture.root(), &cancellation)
            .await
            .unwrap_err(),
        ProjectReinitializeError::Cancelled
    );
    assert_eq!(
        fixture
            .service
            .save(
                preview.clone(),
                ReinitializeConfirmation::confirmed(),
                &cancellation
            )
            .await,
        Err(ProjectReinitializeError::Cancelled)
    );
    fixture
        .session
        .run(async {
            assert_eq!(
                fixture
                    .service
                    .preview(fixture.root(), &CancellationToken::new())
                    .await
                    .unwrap_err(),
                ProjectReinitializeError::Busy
            );
            assert_eq!(
                fixture.save(preview).await,
                Err(ProjectReinitializeError::Busy)
            );
        })
        .await
        .unwrap();
    assert_eq!(fs::read(fixture.path()).unwrap(), before);
    assert_eq!(fs::read_dir(fixture.root()).unwrap().count(), 2);
}

#[tokio::test]
async fn bounded_reads_and_missing_connections_are_safe_errors() {
    for case in 0..4 {
        let fixture = Fixture::new();
        fixture.remove_managed();
        match case {
            0 => fs::write(fixture.path(), vec![b' '; MAX_FILE_BYTES + 1]).unwrap(),
            1 => fixture.edit(|yaml| {
                yaml["components"]["backend"]["artifact"] = "x".repeat(16 * 1024 + 1).into();
            }),
            2 => fs::write(&fixture.registry, "invalid: [secret-sentinel").unwrap(),
            _ => DestinationRegistry::new().save(&fixture.registry).unwrap(),
        }
        let before = fs::read(fixture.path()).unwrap();
        let error = fixture
            .service
            .preview(fixture.root(), &CancellationToken::new())
            .await
            .unwrap_err();
        if case < 2 {
            assert_eq!(error, ProjectReinitializeError::Limit);
        } else {
            assert_eq!(error, ProjectReinitializeError::Destination);
        }
        assert!(!format!("{error:?} {error}").contains("secret-sentinel"));
        assert_eq!(fs::read(fixture.path()).unwrap(), before);
    }
}

#[tokio::test]
async fn malformed_yaml_duplicate_keys_and_non_utf8_sources_are_never_rewritten() {
    for bytes in [
        b"project: [secret-sentinel".to_vec(),
        b"project: demo\nproject: secret-sentinel\n".to_vec(),
        vec![0xff, 0xfe, 0],
    ] {
        let fixture = Fixture::new();
        fs::write(fixture.path(), &bytes).unwrap();
        let error = fixture
            .service
            .preview(fixture.root(), &CancellationToken::new())
            .await
            .unwrap_err();
        assert!(matches!(error, ProjectReinitializeError::Invalid(_)));
        assert!(!format!("{error:?} {error}").contains("secret-sentinel"));
        assert_eq!(fs::read(fixture.path()).unwrap(), bytes);
    }
}

#[tokio::test]
async fn invisible_characters_in_human_values_are_rejected_without_rewriting_yaml() {
    for character in [
        '\u{200b}', '\u{200e}', '\u{202e}', '\u{2060}', '\u{2066}', '\u{feff}', '\u{001b}',
    ] {
        let fixture = Fixture::new();
        fixture.remove_managed();
        fixture.edit(|yaml| {
            yaml["components"]["backend"]["build"][0] = Value::Sequence(vec![
                "cargo".into(),
                format!("ordinary{character}tail-sentinel").into(),
            ]);
        });
        let before = fs::read(fixture.path()).unwrap();
        let error = fixture
            .service
            .preview(fixture.root(), &CancellationToken::new())
            .await
            .unwrap_err();
        assert!(matches!(error, ProjectReinitializeError::Invalid(_)));
        assert!(!format!("{error:?} {error}").contains("tail-sentinel"));
        assert_eq!(fs::read(fixture.path()).unwrap(), before);
    }
}

#[test]
fn exact_yaml_display_validation_allows_only_layout_newlines_among_controls() {
    assert!(ensure_displayable_yaml("project: demo\ncomponents: {}\n").is_ok());
    for character in [
        '\t', '\r', '\u{001b}', '\u{200b}', '\u{200e}', '\u{202e}', '\u{2060}', '\u{2066}',
        '\u{feff}',
    ] {
        assert!(ensure_displayable_yaml(&format!("project: demo{character}\n")).is_err());
    }
}

#[tokio::test]
async fn hardlinked_project_or_registry_is_rejected_on_preview_and_save() {
    for registry in [false, true] {
        for after_preview in [false, true] {
            let fixture = Fixture::new();
            fixture.remove_managed();
            let preview = if after_preview {
                Some(fixture.preview().await)
            } else {
                None
            };
            let path = if registry {
                fixture.registry.clone()
            } else {
                fixture.path()
            };
            let before = fs::read(&path).unwrap();
            fs::hard_link(&path, fixture.root().join("external-link.yaml")).unwrap();
            let error = if let Some(preview) = preview {
                fixture.save(preview).await.unwrap_err()
            } else {
                fixture
                    .service
                    .preview(fixture.root(), &CancellationToken::new())
                    .await
                    .unwrap_err()
            };
            assert_eq!(error, ProjectReinitializeError::UnsafePath);
            assert_eq!(fs::read(&path).unwrap(), before);
        }
    }
}

#[cfg(unix)]
#[tokio::test]
async fn unix_symlinks_are_rejected_and_confirmed_save_preserves_file_mode() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let fixture = Fixture::new();
    fixture.remove_managed();
    fs::set_permissions(fixture.path(), fs::Permissions::from_mode(0o600)).unwrap();
    let preview = fixture.preview().await;
    fixture.save(preview).await.unwrap();
    assert_eq!(
        fs::metadata(fixture.path()).unwrap().permissions().mode() & 0o777,
        0o600
    );
    fixture.remove_managed();
    let preview = fixture.preview().await;
    let original = fixture.root().join("original.yaml");
    fs::rename(fixture.path(), &original).unwrap();
    symlink(&original, fixture.path()).unwrap();
    assert_eq!(
        fixture.save(preview).await,
        Err(ProjectReinitializeError::UnsafePath)
    );
    assert_eq!(
        fixture
            .service
            .preview(fixture.root(), &CancellationToken::new())
            .await
            .unwrap_err(),
        ProjectReinitializeError::UnsafePath
    );
    let root_link = fixture.root().join("linked-root");
    symlink(fixture.root(), &root_link).unwrap();
    assert_eq!(
        fixture
            .service
            .preview(&root_link, &CancellationToken::new())
            .await
            .unwrap_err(),
        ProjectReinitializeError::UnsafePath
    );
}

#[cfg(unix)]
#[tokio::test]
async fn unix_permission_drift_invalidates_preview() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new();
    fixture.remove_managed();
    fs::set_permissions(fixture.path(), fs::Permissions::from_mode(0o600)).unwrap();
    let preview = fixture.preview().await;
    fs::set_permissions(fixture.path(), fs::Permissions::from_mode(0o640)).unwrap();
    assert_eq!(
        fixture.save(preview).await,
        Err(ProjectReinitializeError::Stale)
    );
}
