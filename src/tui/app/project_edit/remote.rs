use crate::{
    config::{DestinationSummary, default_remote_root},
    domain::ComponentName,
};

use super::{ProjectEditPage, ProjectEditScreen};

impl ProjectEditScreen {
    pub(in crate::tui::app) fn remote_target_context(
        &self,
    ) -> Option<(
        DestinationSummary,
        ComponentName,
        String,
        String,
        String,
        Option<crate::config::ServiceConfig>,
    )> {
        let ProjectEditPage::Target { form, .. } = &self.page else {
            return None;
        };
        let draft = self.draft.as_ref()?;
        let destination = draft
            .destinations()
            .iter()
            .find(|item| item.key == form.target.destination)?
            .clone();
        let root = form
            .target
            .root
            .clone()
            .or_else(|| {
                draft
                    .original()
                    .environments
                    .get(
                        form.environment
                            .original
                            .as_deref()
                            .unwrap_or(&form.environment.name),
                    )
                    .and_then(|environment| environment.components.get(&form.component))
                    .map(|target| target.root.clone())
            })
            .unwrap_or_else(|| {
                default_remote_root(
                    &draft.setup.project,
                    &form.environment.name,
                    &form.component,
                )
            });
        Some((
            destination,
            form.component.clone(),
            draft.setup.project.clone(),
            form.environment.name.clone(),
            root,
            form.target.service.clone(),
        ))
    }

    pub(in crate::tui::app) fn apply_remote_target(
        &mut self,
        root: String,
        service: Option<crate::config::ServiceConfig>,
    ) {
        if let ProjectEditPage::Target { form, .. } = &mut self.page {
            form.target.root = Some(root);
            form.target.service = service;
            // The enclosing form still needs its existing Apply action; YAML save is separate.
        }
    }
}
