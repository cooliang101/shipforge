use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::{ComponentName, ReleaseVersion};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComponentObservation {
    pub current: Option<ReleaseVersion>,
    pub previous_healthy: Option<ReleaseVersion>,
}

pub type EnvironmentObservation = BTreeMap<ComponentName, ComponentObservation>;

#[must_use]
pub fn protected_releases<'a>(
    observation: &'a EnvironmentObservation,
    in_progress: impl IntoIterator<Item = &'a ReleaseVersion>,
) -> BTreeSet<ReleaseVersion> {
    observation
        .values()
        .flat_map(|component| {
            component
                .current
                .iter()
                .chain(component.previous_healthy.iter())
        })
        .chain(in_progress)
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version(value: &str) -> ReleaseVersion {
        ReleaseVersion::parse(value).expect("test version must be valid")
    }

    #[test]
    fn retention_protects_current_previous_healthy_and_in_progress() {
        let observation = BTreeMap::from([(
            ComponentName::parse("api").unwrap(),
            ComponentObservation {
                current: Some(version("v3")),
                previous_healthy: Some(version("v2")),
            },
        )]);
        let uploading = version("v4");
        assert_eq!(
            protected_releases(&observation, [&uploading]),
            BTreeSet::from([version("v2"), version("v3"), version("v4")])
        );
    }
}
