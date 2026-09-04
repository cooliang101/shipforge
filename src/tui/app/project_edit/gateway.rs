use async_trait::async_trait;

use crate::application::{DeploymentSession, project_edit::ProjectEditService};

use super::{Arc, CancellationToken, PathBuf, ProjectEditPage, ProjectEditRequest};

#[async_trait(?Send)]
pub(in crate::tui::app) trait ProjectEditGateway:
    std::fmt::Debug + Send + Sync
{
    async fn run(
        &self,
        request: ProjectEditRequest,
        cancellation: &CancellationToken,
    ) -> Result<ProjectEditPage, String>;
}

#[derive(Debug)]
pub(in crate::tui::app) struct LocalProjectEditGateway {
    pub destinations: PathBuf,
    pub session: Arc<DeploymentSession>,
}

#[async_trait(?Send)]
impl ProjectEditGateway for LocalProjectEditGateway {
    async fn run(
        &self,
        request: ProjectEditRequest,
        cancellation: &CancellationToken,
    ) -> Result<ProjectEditPage, String> {
        let service = ProjectEditService::new(self.destinations.clone(), Arc::clone(&self.session));
        match request {
            ProjectEditRequest::Load(root) => service
                .load(&root, cancellation)
                .await
                .map(|draft| ProjectEditPage::Loaded(Arc::new(draft))),
            ProjectEditRequest::Discover(draft) => service
                .discover(&draft, cancellation)
                .await
                .map(|report| ProjectEditPage::Discovery {
                    report: Arc::new(report),
                    cursor: 0,
                }),
            ProjectEditRequest::Preview(draft) => service
                .preview((*draft).clone(), cancellation)
                .await
                .map(|preview| ProjectEditPage::Preview(Arc::new(preview))),
            ProjectEditRequest::Save(preview) => service
                .save((*preview).clone(), cancellation)
                .await
                .map(|config| ProjectEditPage::Saved(Arc::new(config))),
        }
        .map_err(|error| error.to_string())
    }
}
