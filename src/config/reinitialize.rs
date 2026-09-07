//! Read-only extraction of existing user intent for explicit new-Project initialization.

use std::{collections::BTreeMap, path::Path};

use serde_yaml_ng::Value;

use crate::domain::ComponentName;

use super::{
    ArtifactSpec, BuildCommand, ComponentSetup, ConfigError, EnvironmentSetup, PROJECT_FILE,
    ProjectSetup, RawProjectConfig, TargetSetup, parse_contents, validate_root,
};

/// A valid Project is deliberately not eligible. No identities or roots are read
/// from history, caches, or a remote endpoint.
pub(crate) fn reinitialize_setup(
    root: &Path,
    contents: &str,
) -> Result<Option<ProjectSetup>, ConfigError> {
    crate::telemetry::detect_sensitive_config(contents)?;
    if parse_contents(root, contents).is_ok() {
        return Ok(None);
    }
    let yaml_error = |source| ConfigError::Yaml {
        path: root.join(PROJECT_FILE),
        source,
    };
    let mut value: Value = serde_yaml_ng::from_str(contents).map_err(yaml_error)?;
    let managed = value
        .as_mapping_mut()
        .and_then(|mapping| mapping.remove(Value::String("_shipforge".into())))
        .filter(|value| !value.is_null());
    // Removing only this field retains deny_unknown_fields and all typed human
    // fields. A malformed build, destination, schema or extra field cannot be
    // accepted merely because its system-maintained identity was also damaged.
    let raw: RawProjectConfig = serde_yaml_ng::from_value(value).map_err(yaml_error)?;
    if !matches!(raw.schema_version, 1 | 2) {
        return Err(ConfigError::SchemaVersion(raw.schema_version));
    }
    let mut environments = BTreeMap::new();
    for (environment, configured) in raw.environments {
        let mut components = BTreeMap::new();
        for (name, target) in configured.components {
            if (raw.schema_version == 2 && target.systemd.is_some())
                || (raw.schema_version == 1 && target.service.is_some())
            {
                return Err(ConfigError::Service {
                    environment: environment.clone(),
                    component: name.clone(),
                    message: "Service fields do not match the schema version.",
                });
            }
            let retained = retained_root(
                managed.as_ref(),
                &environment,
                &name,
                target.root.as_deref(),
            )?;
            components.insert(
                name,
                TargetSetup {
                    destination: target.destination,
                    root: target.root.or_else(|| retained.map(str::to_owned)),
                    service: target
                        .service
                        .or_else(|| target.systemd.map(super::ServiceConfig::systemd)),
                    health: target.health,
                    after: target.after,
                },
            );
        }
        environments.insert(environment, EnvironmentSetup { components });
    }
    Ok(Some(ProjectSetup {
        project: raw.project,
        components: raw
            .components
            .into_iter()
            .map(|(name, component)| {
                (
                    name,
                    ComponentSetup {
                        working_directory: component.working_directory,
                        build: component
                            .build
                            .into_iter()
                            .map(BuildCommand::from_argv)
                            .collect(),
                        artifact: ArtifactSpec {
                            path: component.artifact,
                        },
                    },
                )
            })
            .collect(),
        environments,
    }))
}

fn retained_root<'a>(
    managed: Option<&'a Value>,
    environment: &str,
    component: &ComponentName,
    configured: Option<&str>,
) -> Result<Option<&'a str>, ConfigError> {
    let retained = managed.and_then(|managed| {
        managed
            .get("environments")?
            .get(environment)?
            .get("components")?
            .get(component.as_str())?
            .get("resolvedRoot")
    });
    let retained = retained
        .map(|value| {
            value.as_str().ok_or_else(|| {
                ConfigError::ManagedState("existing resolvedRoot is not a path".into())
            })
        })
        .transpose()?;
    if let Some(root) = retained {
        validate_root(environment, component, root)?;
        if configured.is_some_and(|configured| configured != root) {
            return Err(ConfigError::ManagedState(
                "configured and system-maintained roots disagree".into(),
            ));
        }
    } else if managed.is_some() && configured.is_none() {
        return Err(ConfigError::ManagedState(
            "a damaged system-maintained section has no reliable root".into(),
        ));
    }
    Ok(retained)
}
