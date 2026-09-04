use std::{
    fs,
    path::{Path, PathBuf},
};

use tempfile::tempdir;

use super::*;

const VALID: &str = r"
schemaVersion: 1
_shipforge:
  projectId: prj_01994b8e1a7070008000000000000001
  environments:
    production:
      id: env_01994b8e1a7070008000000000000002
      components:
        backend:
          generation: 1
          resolvedRoot: /srv/shipforge/mall/production/backend
        worker:
          generation: 1
          resolvedRoot: /srv/shipforge/mall/production/worker
project: mall
components:
  backend:
    build:
      - [cargo, build, --release]
    artifact: target/release/backend
  worker:
    build:
      - [cargo, build, --release]
      - [cargo, test]
    artifact: target/release/worker
environments:
  production:
    components:
      backend:
        to: dst_00000000000000000000000000000001
      worker:
        to: dst_00000000000000000000000000000001
        after: [backend]
";

fn load_text(text: &str) -> Result<ProjectConfigState, ConfigError> {
    let directory = tempdir().unwrap();
    fs::write(directory.path().join(PROJECT_FILE), text).unwrap();
    load(directory.path())
}

fn setup() -> ProjectSetup {
    let backend = ComponentName::parse("backend").unwrap();
    ProjectSetup {
        project: "mall".into(),
        components: BTreeMap::from([(
            backend.clone(),
            ComponentSetup {
                working_directory: None,
                build: vec![BuildCommand::argv("cargo", ["build", "--release"])],
                artifact: ArtifactSpec {
                    path: "target/release/backend".into(),
                },
            },
        )]),
        environments: BTreeMap::from([(
            "production".into(),
            EnvironmentSetup {
                components: BTreeMap::from([(
                    backend,
                    TargetSetup {
                        destination: DestinationKey::parse("dst_00000000000000000000000000000001")
                            .unwrap(),
                        root: None,
                        systemd: Some("mall-api.service".into()),
                        health: None,
                        after: Vec::new(),
                    },
                )]),
            },
        )]),
    }
}

#[test]
fn missing_file_is_a_new_project_without_writing_anything() {
    let directory = tempdir().unwrap();
    assert_eq!(load(directory.path()).unwrap(), ProjectConfigState::Missing);
    assert!(!directory.path().join(PROJECT_FILE).exists());
}

#[test]
fn initialize_generates_identity_default_root_and_atomic_file() {
    let directory = tempdir().unwrap();
    let config = initialize(directory.path(), setup()).unwrap();
    let backend = ComponentName::parse("backend").unwrap();
    assert_eq!(
        config.environments["production"].components[&backend].root,
        "/srv/shipforge/mall/production/backend"
    );
    assert!(matches!(
        load(directory.path()),
        Ok(ProjectConfigState::Loaded(loaded)) if loaded.project_id == config.project_id
    ));
}

#[test]
fn initialize_never_overwrites_an_existing_file() {
    let directory = tempdir().unwrap();
    let path = directory.path().join(PROJECT_FILE);
    fs::write(&path, "keep me").unwrap();
    assert!(matches!(
        initialize(directory.path(), setup()),
        Err(ConfigError::AlreadyExists(_))
    ));
    assert_eq!(fs::read_to_string(path).unwrap(), "keep me");
}

#[test]
fn prepared_initialization_commits_exact_preview_and_identity() {
    let directory = tempdir().unwrap();
    let prepared = prepare_initialize(setup()).unwrap();
    let preview = prepared.preview().to_owned();
    let project_id = prepared.config().project_id.clone();
    assert!(!directory.path().join(PROJECT_FILE).exists());

    let committed = prepared.commit(directory.path()).unwrap();
    assert_eq!(committed.project_id, project_id);
    assert_eq!(
        fs::read_to_string(directory.path().join(PROJECT_FILE)).unwrap(),
        preview
    );
}

#[test]
fn invalid_setup_is_not_written() {
    let directory = tempdir().unwrap();
    let mut invalid = setup();
    invalid
        .components
        .values_mut()
        .next()
        .unwrap()
        .build
        .clear();
    assert!(matches!(
        initialize(directory.path(), invalid),
        Err(ConfigError::EmptyBuild(_))
    ));
    assert!(!directory.path().join(PROJECT_FILE).exists());
}

#[test]
fn confirmed_reinitialize_replaces_identity() {
    let directory = tempdir().unwrap();
    let original = initialize(directory.path(), setup()).unwrap();
    let replacement = reinitialize(
        directory.path(),
        setup(),
        ReinitializeConfirmation::confirmed(),
    )
    .unwrap();
    assert_ne!(original.project_id, replacement.project_id);
    assert!(matches!(
        load(directory.path()),
        Ok(ProjectConfigState::Loaded(loaded)) if loaded.project_id == replacement.project_id
    ));
}

#[test]
fn project_rename_preserves_project_environment_identity_and_root() {
    let directory = tempdir().unwrap();
    let original = initialize(directory.path(), setup()).unwrap();
    let mut renamed = setup();
    renamed.project = "mall-renamed".into();
    let updated = update(directory.path(), renamed, &[]).unwrap();
    let backend = ComponentName::parse("backend").unwrap();
    assert_eq!(updated.project_id, original.project_id);
    assert_eq!(
        updated.environments["production"].id,
        original.environments["production"].id
    );
    assert_eq!(
        updated.environments["production"].components[&backend].root,
        "/srv/shipforge/mall/production/backend"
    );
}

#[test]
fn explicit_environment_rename_preserves_identity_generation_and_root() {
    let directory = tempdir().unwrap();
    let original = initialize(directory.path(), setup()).unwrap();
    let mut renamed = setup();
    let environment = renamed.environments.remove("production").unwrap();
    renamed.environments.insert("live".into(), environment);
    let updated = update(
        directory.path(),
        renamed,
        &[EnvironmentRename {
            from: "production".into(),
            to: "live".into(),
        }],
    )
    .unwrap();
    let backend = ComponentName::parse("backend").unwrap();
    assert_eq!(
        updated.environments["live"].id,
        original.environments["production"].id
    );
    assert_eq!(
        updated.environments["live"].components[&backend],
        original.environments["production"].components[&backend]
    );
}

#[test]
fn target_change_increments_generation_but_build_change_does_not() {
    let directory = tempdir().unwrap();
    initialize(directory.path(), setup()).unwrap();
    let backend = ComponentName::parse("backend").unwrap();

    let mut build_changed = setup();
    build_changed.components.get_mut(&backend).unwrap().build =
        vec![BuildCommand::argv("cargo", ["build", "--locked"])];
    let after_build = update(directory.path(), build_changed, &[]).unwrap();
    assert_eq!(
        after_build.environments["production"].components[&backend]
            .generation
            .get(),
        1
    );

    let mut target_changed = setup();
    target_changed
        .environments
        .get_mut("production")
        .unwrap()
        .components
        .get_mut(&backend)
        .unwrap()
        .destination = DestinationKey::parse("dst_00000000000000000000000000000002").unwrap();
    let after_target = update(directory.path(), target_changed, &[]).unwrap();
    assert_eq!(
        after_target.environments["production"].components[&backend]
            .generation
            .get(),
        2
    );
}

#[test]
fn component_rename_creates_a_new_generation_and_default_root() {
    let directory = tempdir().unwrap();
    initialize(directory.path(), setup()).unwrap();
    let mut renamed = setup();
    let backend = ComponentName::parse("backend").unwrap();
    let api = ComponentName::parse("api").unwrap();
    let component = renamed.components.remove(&backend).unwrap();
    renamed.components.insert(api.clone(), component);
    let target = renamed
        .environments
        .get_mut("production")
        .unwrap()
        .components
        .remove(&backend)
        .unwrap();
    renamed
        .environments
        .get_mut("production")
        .unwrap()
        .components
        .insert(api.clone(), target);
    let updated = update(directory.path(), renamed, &[]).unwrap();
    let target = &updated.environments["production"].components[&api];
    assert_eq!(target.generation.get(), 1);
    assert_eq!(target.root, "/srv/shipforge/mall/production/api");
}

#[test]
fn invalid_environment_rename_does_not_rewrite_file() {
    let directory = tempdir().unwrap();
    initialize(directory.path(), setup()).unwrap();
    let path = directory.path().join(PROJECT_FILE);
    let before = fs::read_to_string(&path).unwrap();
    assert!(matches!(
        update(
            directory.path(),
            setup(),
            &[EnvironmentRename {
                from: "missing".into(),
                to: "production".into(),
            }]
        ),
        Err(ConfigError::InvalidEnvironmentRename(_))
    ));
    assert_eq!(fs::read_to_string(path).unwrap(), before);
}

#[test]
fn artifact_type_is_detected_after_build() {
    let directory = tempdir().unwrap();
    fs::create_dir(directory.path().join("backend")).unwrap();
    fs::write(directory.path().join("backend/server"), b"binary").unwrap();
    let artifact = ArtifactSpec {
        path: "server".into(),
    };
    assert_eq!(
        artifact
            .resolve(directory.path(), Path::new("backend"))
            .unwrap()
            .kind,
        ResolvedArtifactKind::File
    );
}

#[test]
fn artifact_rejects_empty_output() {
    let directory = tempdir().unwrap();
    fs::create_dir(directory.path().join("backend")).unwrap();
    fs::write(directory.path().join("backend/empty"), []).unwrap();
    let empty = ArtifactSpec {
        path: "empty".into(),
    };
    assert!(matches!(
        empty.resolve(directory.path(), Path::new("backend")),
        Err(ConfigError::EmptyArtifact { .. })
    ));
}

#[test]
fn artifact_resolution_rejects_project_escape() {
    let container = tempdir().unwrap();
    let project = container.path().join("project");
    fs::create_dir(&project).unwrap();
    fs::write(container.path().join("secret"), b"secret").unwrap();
    let artifact = ArtifactSpec {
        path: "secret".into(),
    };
    assert!(matches!(
        artifact.resolve(&project, Path::new("..")),
        Err(ConfigError::ArtifactOutsideProject { .. })
    ));
}

#[test]
fn canonical_config_loads_typed_values() {
    let ProjectConfigState::Loaded(config) = load_text(VALID).unwrap() else {
        panic!("expected loaded config");
    };
    let backend = &config.components[&ComponentName::parse("backend").unwrap()];
    assert_eq!(backend.build[0].program, "cargo");
    assert!(!backend.build[0].shell);
    assert_eq!(
        backend.artifact.path,
        PathBuf::from("target/release/backend")
    );
    assert_eq!(backend.working_directory, PathBuf::from("backend"));

    let worker = &config.components[&ComponentName::parse("worker").unwrap()];
    assert_eq!(worker.build.len(), 2);
    assert_eq!(worker.artifact.path, PathBuf::from("target/release/worker"));
}

#[test]
fn artifact_object_form_is_rejected() {
    let verbose = VALID.replacen(
        "artifact: target/release/backend",
        "artifact: { path: target/release/backend, kind: file }",
        1,
    );
    assert!(matches!(load_text(&verbose), Err(ConfigError::Yaml { .. })));
}

#[test]
fn alternate_build_forms_are_rejected() {
    let structured = VALID.replace(
        "build:\n      - [cargo, build, --release]",
        "build: { program: cargo, args: [build, --release], shell: true }",
    );
    assert!(load_text(&structured).is_err());

    let single_argv = VALID.replace(
        "build:\n      - [cargo, build, --release]",
        "build: [cargo, build, --release]",
    );
    assert!(load_text(&single_argv).is_err());
}

#[test]
fn repository_example_is_a_valid_contract_fixture() {
    let example = include_str!("../../docs/examples/shipforge.yaml");
    let result = load_text(example);
    assert!(
        matches!(result, Ok(ProjectConfigState::Loaded(_))),
        "example failed to load: {result:?}"
    );
}

#[test]
fn selected_subgraph_does_not_auto_select_after_dependency() {
    let ProjectConfigState::Loaded(config) = load_text(VALID).unwrap() else {
        panic!("expected loaded config");
    };
    let environment = &config.environments["production"];
    let backend = ComponentName::parse("backend").unwrap();
    let worker = ComponentName::parse("worker").unwrap();
    assert_eq!(
        environment
            .activation_order("production", [&worker])
            .unwrap(),
        vec![worker.clone()]
    );
    assert_eq!(
        environment
            .activation_order("production", [&worker, &backend])
            .unwrap(),
        vec![backend, worker]
    );
}

#[test]
fn corrupt_managed_section_is_rejected() {
    let missing_generation = VALID.replacen("          generation: 1\n", "", 1);
    assert!(matches!(
        load_text(&missing_generation),
        Err(ConfigError::Yaml { .. })
    ));
}

#[test]
fn missing_managed_section_requires_explicit_reinitialization() {
    let without_managed = VALID.replace(
        "_shipforge:\n  projectId: prj_01994b8e1a7070008000000000000001\n  environments:\n    production:\n      id: env_01994b8e1a7070008000000000000002\n      components:\n        backend:\n          generation: 1\n          resolvedRoot: /srv/shipforge/mall/production/backend\n        worker:\n          generation: 1\n          resolvedRoot: /srv/shipforge/mall/production/worker\n",
        "",
    );
    assert!(matches!(
        load_text(&without_managed),
        Err(ConfigError::ManagedState(_))
    ));
}

#[test]
fn dependency_cycle_is_rejected() {
    let cycle = VALID.replace(
        "      backend:\n        to: dst_00000000000000000000000000000001",
        "      backend:\n        to: dst_00000000000000000000000000000001\n        after: [worker]",
    );
    assert!(matches!(
        load_text(&cycle),
        Err(ConfigError::DependencyCycle { .. })
    ));
}

#[test]
fn traversal_artifact_is_rejected_without_rewriting_config() {
    let directory = tempdir().unwrap();
    let path = directory.path().join(PROJECT_FILE);
    let unsafe_config = VALID.replace("target/release/backend", "../secret");
    fs::write(&path, &unsafe_config).unwrap();
    assert!(matches!(
        load(directory.path()),
        Err(ConfigError::UnsafePath { .. })
    ));
    assert_eq!(fs::read_to_string(path).unwrap(), unsafe_config);
}

#[test]
fn project_config_rejects_embedded_credentials() {
    let unsafe_config = VALID.replace(
        "build:\n      - [cargo, build, --release]",
        "build: { program: cargo, args: [build], token: plaintext }",
    );
    assert!(matches!(
        load_text(&unsafe_config),
        Err(ConfigError::Security(_))
    ));
}

#[test]
fn roots_on_one_destination_must_not_overlap() {
    let overlap = VALID.replace(
        "/srv/shipforge/mall/production/worker",
        "/srv/shipforge/mall/production/backend/worker",
    );
    assert!(matches!(
        load_text(&overlap),
        Err(ConfigError::OverlappingRoots { .. })
    ));
}

#[test]
fn explicit_root_must_match_managed_resolved_root() {
    let mismatch = VALID.replace(
        "      backend:\n        to: dst_00000000000000000000000000000001",
        "      backend:\n        to: dst_00000000000000000000000000000001\n        root: /srv/other/backend",
    );
    assert!(matches!(
        load_text(&mismatch),
        Err(ConfigError::ManagedState(_))
    ));
}

#[test]
fn selecting_an_unconfigured_component_is_rejected() {
    let ProjectConfigState::Loaded(config) = load_text(VALID).unwrap() else {
        panic!("expected loaded config");
    };
    let environment = &config.environments["production"];
    let frontend = ComponentName::parse("frontend").unwrap();
    assert!(matches!(
        environment.activation_order("production", [&frontend]),
        Err(ConfigError::UnknownSelectedComponent { .. })
    ));
}
