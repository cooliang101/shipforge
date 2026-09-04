use std::collections::{BTreeMap, BTreeSet};

use crate::domain::ComponentName;

use super::{ConfigError, TargetConfig};

pub(super) fn activation_order<'a>(
    environment: &str,
    targets: &BTreeMap<ComponentName, TargetConfig>,
    selected: impl IntoIterator<Item = &'a ComponentName>,
) -> Result<Vec<ComponentName>, ConfigError> {
    let selected = selected.into_iter().cloned().collect::<BTreeSet<_>>();
    for component in &selected {
        if !targets.contains_key(component) {
            return Err(ConfigError::UnknownSelectedComponent {
                environment: environment.into(),
                component: component.clone(),
            });
        }
    }

    let mut remaining_dependencies = selected
        .iter()
        .map(|component| {
            let dependencies = targets[component]
                .after
                .iter()
                .filter(|dependency| selected.contains(*dependency))
                .cloned()
                .collect::<BTreeSet<_>>();
            (component.clone(), dependencies)
        })
        .collect::<BTreeMap<_, _>>();
    let mut ordered = Vec::with_capacity(selected.len());

    while !remaining_dependencies.is_empty() {
        let ready = remaining_dependencies
            .iter()
            .filter(|(_, dependencies)| dependencies.is_empty())
            .map(|(component, _)| component.clone())
            .collect::<Vec<_>>();
        if ready.is_empty() {
            return Err(ConfigError::DependencyCycle {
                environment: environment.into(),
            });
        }
        for component in ready {
            remaining_dependencies.remove(&component);
            for dependencies in remaining_dependencies.values_mut() {
                dependencies.remove(&component);
            }
            ordered.push(component);
        }
    }
    Ok(ordered)
}
