use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    thread::JoinHandle,
};

use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::super::BackgroundEvent;
use super::{App, Page, RemoteSetupSelectionState, Screen};
use crate::application::{
    ConnectionManagementService, ManagementPaths, RemoteDirectoryCandidates, RemoteSetupCandidates,
};

#[derive(Clone, Debug)]
pub(in crate::tui::app) enum Request {
    Inspect,
    Browse(String),
}

#[derive(Debug)]
pub(in crate::tui::app) enum RemoteTargetResult {
    Inspected(RemoteSetupCandidates),
    Browsed(RemoteDirectoryCandidates),
}

#[derive(Debug)]
pub(in crate::tui::app) struct RemoteTargetTask {
    id: Uuid,
    origin: RemoteSetupSelectionState,
    cancellation: CancellationToken,
    worker: JoinHandle<()>,
    request: Request,
}

const FAILURE: &str = "Read-only target discovery failed or timed out. Check the saved connection, permissions and directory, then retry. Previous observations were not refreshed; no remote changes were made.";

impl App {
    pub(in crate::tui::app) fn start_remote_target(
        &mut self,
        mut origin: RemoteSetupSelectionState,
        request: Request,
    ) {
        if self.remote_target_task.is_some() || self.deployment_session.is_active() {
            self.message = Some("Wait for the active operation before inspecting a target.".into());
            return;
        }
        let Some(runtime) = self.runtime.clone() else {
            self.message =
                Some("Target discovery runtime is unavailable; return and retry.".into());
            return;
        };
        if matches!(request, Request::Inspect) {
            origin.root_state = None;
            origin.notices.clear();
        }
        let service = ConnectionManagementService::new(
            ManagementPaths {
                projects: self.registry_path.clone(),
                destinations: self.destination_registry_path.clone(),
                credentials: self.credential_registry_path.clone(),
                history: self
                    .destination_registry_path
                    .with_file_name("history.sqlite3"),
            },
            self.deployment_session.clone(),
            self.setup_service.clone(),
        );
        let id = Uuid::now_v7();
        let cancellation = CancellationToken::new();
        let worker_cancel = cancellation.clone();
        let sender = self.background_sender.clone();
        let summary = origin.destination.clone();
        let root = origin.root.clone();
        let worker_request = request.clone();
        let spawned = std::thread::Builder::new().name("shipforge-target-choices".into()).spawn(move || {
            let result = catch_unwind(AssertUnwindSafe(|| runtime.block_on(async {
                if worker_cancel.is_cancelled() { return Err(FAILURE.to_owned()); }
                let connection = service.list_connections().map_err(|_| FAILURE.to_owned())?.into_iter()
                    .find(|item| item.key == summary.key && item.current.revision == summary.revision
                        && item.current.settings.endpoint_label() == summary.endpoint)
                    .ok_or_else(|| "The selected connection changed or disappeared. Return and reload connections before retrying.".to_owned())?;
                match worker_request {
                    Request::Inspect => service.inspect_saved_target(&connection, &root, &worker_cancel).await.map(RemoteTargetResult::Inspected),
                    Request::Browse(path) => service.browse_saved_directories(&connection, &path, &worker_cancel).await.map(RemoteTargetResult::Browsed),
                }.map_err(|_| FAILURE.to_owned())
            }))).unwrap_or_else(|_| Err(FAILURE.to_owned()));
            let _ = sender.send(BackgroundEvent::RemoteTarget(id, result));
        });
        match spawned {
            Ok(worker) => {
                let mut screen = origin.clone();
                screen.page = Page::Loading { cancelling: false };
                self.remote_target_task = Some(RemoteTargetTask {
                    id,
                    origin,
                    cancellation,
                    worker,
                    request,
                });
                self.screen = Screen::RemoteSetupSelection(screen);
            }
            Err(_) => {
                self.message =
                    Some("Could not start read-only target discovery. Return and retry.".into());
            }
        }
    }

    pub(in crate::tui::app) fn cancel_remote_target(&mut self) {
        if let Some(task) = &self.remote_target_task {
            task.cancellation.cancel();
            if let Screen::RemoteSetupSelection(screen) = &mut self.screen {
                screen.page = Page::Loading { cancelling: true };
            }
        }
    }

    pub(in crate::tui::app) fn finish_remote_target(
        &mut self,
        id: Uuid,
        result: Result<RemoteTargetResult, String>,
    ) {
        if self
            .remote_target_task
            .as_ref()
            .is_none_or(|task| task.id != id)
        {
            return;
        }
        let task = self.remote_target_task.take().expect("matched task");
        let joined = task.worker.join();
        if !matches!(&self.screen, Screen::RemoteSetupSelection(screen) if matches!(screen.page, Page::Loading { .. }))
        {
            return;
        }
        let mut screen = task.origin;
        // Failed or cancelled browsing cannot revive cached children as fresh
        // evidence. Retain the requested path for an explicit, recoverable retry.
        if let Request::Browse(path) = task.request {
            screen.page = Page::Unavailable { retry_path: path };
        }
        if task.cancellation.is_cancelled() {
            self.message = Some("Target discovery cancelled. No new candidates were applied and no remote changes were made.".into());
        } else if joined.is_err() {
            self.message = Some(FAILURE.into());
        } else {
            match result {
                Ok(RemoteTargetResult::Inspected(candidates)) => {
                    if !screen.adopt_probe(candidates) {
                        self.message = Some(FAILURE.into());
                    }
                }
                Ok(RemoteTargetResult::Browsed(candidates)) => {
                    screen.page = Page::Directories {
                        candidates,
                        cursor: 0,
                    }
                }
                Err(error) => self.message = Some(error),
            }
        }
        self.screen = Screen::RemoteSetupSelection(screen);
    }
}
