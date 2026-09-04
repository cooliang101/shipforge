use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Capability {
    LocalBuild,
    ProviderBuild,
    StagedDeployment,
    ExplicitActivation,
    Observe,
    Rollback,
    PreviewUrl,
    RemoteLogs,
    Retention,
    Cancellation,
    TrafficSplitting,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DriverCapabilities(BTreeSet<Capability>);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapabilityRejection {
    pub missing: BTreeSet<Capability>,
}

impl DriverCapabilities {
    #[must_use]
    pub fn new(capabilities: impl IntoIterator<Item = Capability>) -> Self {
        Self(capabilities.into_iter().collect())
    }

    /// Verifies that the Driver supports every requested capability.
    ///
    /// # Errors
    ///
    /// Returns all capabilities that are not supported by this Driver.
    pub fn require(
        &self,
        required: impl IntoIterator<Item = Capability>,
    ) -> Result<(), CapabilityRejection> {
        let missing = required
            .into_iter()
            .filter(|capability| !self.0.contains(capability))
            .collect::<BTreeSet<_>>();
        if missing.is_empty() {
            Ok(())
        } else {
            Err(CapabilityRejection { missing })
        }
    }

    #[must_use]
    pub fn contains(&self, capability: Capability) -> bool {
        self.0.contains(&capability)
    }

    #[must_use]
    pub fn is_subset_of(&self, other: &Self) -> bool {
        self.0.is_subset(&other.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_check_reports_every_missing_capability() {
        let capabilities = DriverCapabilities::new([Capability::StagedDeployment]);
        let rejection = capabilities
            .require([Capability::ExplicitActivation, Capability::Rollback])
            .expect_err("requirements should be rejected");
        assert_eq!(
            rejection.missing,
            BTreeSet::from([Capability::ExplicitActivation, Capability::Rollback])
        );
    }
}
