//! One tracked first-time SSH setup worker at a time, including Agent discovery.

use std::{thread::JoinHandle, time::Duration};

use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{
    App, BackgroundEvent, DestinationSetupRequest, DestinationSetupService, EndpointProbeRequest,
    HostKeyFingerprint, KeyCode, LocalIdentityCandidate, NewSshDestinationState,
    RemoteSetupCandidates, Screen, apply_agent_identities, catch_worker_failure,
};

const IDENTITY_TIMEOUT: Duration = Duration::from_secs(10);
const HOST_KEY_TIMEOUT: Duration = Duration::from_secs(12);
const AUTHENTICATION_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SetupKind {
    Identities,
    HostKey,
    Authentication,
}

impl SetupKind {
    const fn failure(self) -> &'static str {
        match self {
            Self::Identities => {
                "SSH Agent discovery failed or timed out. Check the Agent or choose a key file; no identity was saved."
            }
            Self::HostKey => {
                "Host Key capture failed or timed out. Check the host and port, then retry; no key was trusted."
            }
            Self::Authentication => {
                "SSH authentication or read-only discovery failed or timed out. Check the identity and permissions, then retry; no connection was saved."
            }
        }
    }

    fn event(self, id: Uuid, result: Result<SetupResult, String>) -> BackgroundEvent {
        match self {
            Self::Identities => BackgroundEvent::AgentIdentities(
                id,
                result.and_then(|result| match result {
                    SetupResult::Identities(identities) => Ok(identities),
                    _ => Err(self.failure().into()),
                }),
            ),
            Self::HostKey => BackgroundEvent::HostKey(
                id,
                result.and_then(|result| match result {
                    SetupResult::HostKey(fingerprint) => Ok(fingerprint),
                    _ => Err(self.failure().into()),
                }),
            ),
            Self::Authentication => BackgroundEvent::Authentication(
                id,
                result.and_then(|result| match result {
                    SetupResult::Authenticated(candidates) => Ok(candidates),
                    _ => Err(self.failure().into()),
                }),
            ),
        }
    }
}

enum SetupRequest {
    Identities,
    HostKey(EndpointProbeRequest),
    Authentication(DestinationSetupRequest),
}

impl SetupRequest {
    const fn kind(&self) -> SetupKind {
        match self {
            Self::Identities => SetupKind::Identities,
            Self::HostKey(_) => SetupKind::HostKey,
            Self::Authentication(_) => SetupKind::Authentication,
        }
    }
}

enum SetupResult {
    Identities(Vec<LocalIdentityCandidate>),
    HostKey(String),
    Authenticated(RemoteSetupCandidates),
}

#[derive(Debug)]
pub(super) struct SetupTask {
    id: Uuid,
    kind: SetupKind,
    cancellation: CancellationToken,
    worker: Option<JoinHandle<()>>,
}

impl SetupTask {
    #[cfg(test)]
    pub(super) fn fixture(id: Uuid, kind: SetupKind, cancellation: CancellationToken) -> Self {
        Self {
            id,
            kind,
            cancellation,
            worker: None,
        }
    }
}

impl App {
    pub(in crate::tui) fn setup_busy(&self) -> bool {
        self.setup_task.is_some()
    }

    pub(in crate::tui) fn setup_cancelling(&self) -> bool {
        self.setup_task
            .as_ref()
            .is_some_and(|task| task.cancellation.is_cancelled())
    }

    fn start_setup_worker(&mut self, request: SetupRequest) -> Option<(Uuid, CancellationToken)> {
        if self.setup_task.is_some() {
            self.message =
                Some("Wait for the current SSH setup operation to finish before retrying.".into());
            return None;
        }
        let Some(runtime) = self.runtime.clone() else {
            self.message =
                Some("SSH setup runtime is unavailable. No operation was started.".into());
            return None;
        };
        let id = Uuid::now_v7();
        let kind = request.kind();
        let cancellation = CancellationToken::new();
        let worker_cancellation = cancellation.clone();
        let service = self.setup_service.clone();
        let sender = self.background_sender.clone();
        let spawned = std::thread::Builder::new()
            .name("shipforge-setup".into())
            .spawn(move || {
                let result = catch_worker_failure(|| {
                    runtime.block_on(run(&service, request, &worker_cancellation))
                });
                let _ = sender.send(kind.event(id, result));
            });
        if let Ok(worker) = spawned {
            self.setup_task = Some(SetupTask {
                id,
                kind,
                cancellation: cancellation.clone(),
                worker: Some(worker),
            });
            Some((id, cancellation))
        } else {
            self.message =
                Some("Could not start the SSH setup worker. No operation was started.".into());
            None
        }
    }

    pub(super) fn launch_setup_identities(&mut self) {
        if !matches!(self.screen, Screen::NewSshDestination(_)) {
            return;
        }
        let Some((id, _)) = self.start_setup_worker(SetupRequest::Identities) else {
            if let Screen::NewSshDestination(draft) = &mut self.screen {
                draft.agent_status = "SSH Agent: discovery unavailable; choose a key file".into();
            }
            return;
        };
        if let Screen::NewSshDestination(draft) = &mut self.screen {
            draft.identity_request = Some(id);
            draft.agent_status =
                "SSH Agent: checking (up to 10 seconds); Esc requests cancellation".into();
        }
    }

    pub(super) fn launch_setup_host_key(
        &mut self,
        draft: &NewSshDestinationState,
        request: EndpointProbeRequest,
    ) {
        if let Some((request_id, _)) = self.start_setup_worker(SetupRequest::HostKey(request)) {
            self.screen = Screen::HostKeyPending {
                request_id,
                draft: draft.clone(),
                cancellation_requested: false,
            };
        }
    }

    pub(super) fn launch_setup_authentication(
        &mut self,
        draft: &NewSshDestinationState,
        fingerprint: &HostKeyFingerprint,
        request: DestinationSetupRequest,
    ) {
        if let Some((request_id, _)) =
            self.start_setup_worker(SetupRequest::Authentication(request))
        {
            self.screen = Screen::SshAuthenticationPending {
                request_id,
                draft: draft.clone(),
                fingerprint: fingerprint.clone(),
                cancellation_requested: false,
            };
        }
    }

    pub(super) fn cancel_setup_on_escape(&mut self, key: KeyCode) {
        if key == KeyCode::Esc {
            self.cancel_setup();
        }
    }

    pub(super) fn cancel_setup(&mut self) {
        let Some(task) = &self.setup_task else {
            return;
        };
        task.cancellation.cancel();
        match &mut self.screen {
            Screen::HostKeyPending {
                cancellation_requested,
                ..
            }
            | Screen::SshAuthenticationPending {
                cancellation_requested,
                ..
            } => *cancellation_requested = true,
            Screen::NewSshDestination(draft) | Screen::KeyBrowser { draft, .. } => {
                draft.agent_status =
                    "SSH Agent: cancellation requested; waiting for the worker".into();
            }
            _ => {}
        }
        self.message = Some("SSH setup cancellation requested; waiting for the worker. No connection will be saved.".into());
    }

    fn finish_setup_worker(&mut self, id: Uuid, kind: SetupKind) -> Option<bool> {
        if self
            .setup_task
            .as_ref()
            .is_none_or(|task| task.id != id || task.kind != kind)
        {
            return None;
        }
        let task = self.setup_task.take()?;
        // The completion event is sent after all gateway work. Join before any
        // navigation or persistence so even the final thread tail has finished.
        let worker_failed = task.worker.is_some_and(|worker| worker.join().is_err());
        Some(task.cancellation.is_cancelled() || worker_failed)
    }

    pub(super) fn finish_setup_identities(
        &mut self,
        id: Uuid,
        result: Result<Vec<LocalIdentityCandidate>, String>,
    ) {
        let Some(cancelled) = self.finish_setup_worker(id, SetupKind::Identities) else {
            return;
        };
        let (Screen::NewSshDestination(draft) | Screen::KeyBrowser { draft, .. }) =
            &mut self.screen
        else {
            return;
        };
        if draft.identity_request != Some(id) {
            return;
        }
        draft.identity_request = None;
        if cancelled {
            draft.agent_status =
                "SSH Agent: discovery cancelled; choose a key file or retry setup".into();
        } else {
            apply_agent_identities(
                draft,
                result.map_err(|_| SetupKind::Identities.failure().to_owned()),
            );
        }
    }

    pub(super) fn finish_setup_host_key(&mut self, id: Uuid, result: Result<String, String>) {
        let Some(cancelled) = self.finish_setup_worker(id, SetupKind::HostKey) else {
            return;
        };
        let Screen::HostKeyPending {
            request_id, draft, ..
        } = self.screen.clone()
        else {
            return;
        };
        if request_id != id {
            return;
        }
        if cancelled {
            self.screen = Screen::NewSshDestination(draft);
            self.message = Some(
                "Host Key capture cancelled. No key was trusted; review the form before retrying."
                    .into(),
            );
            return;
        }
        if let Some(fingerprint) = result
            .ok()
            .and_then(|value| HostKeyFingerprint::parse(value).ok())
        {
            self.screen = Screen::HostKeyConfirm { draft, fingerprint };
        } else {
            self.screen = Screen::NewSshDestination(draft);
            self.message = Some(SetupKind::HostKey.failure().into());
        }
    }

    pub(super) fn finish_setup_authentication(
        &mut self,
        id: Uuid,
        result: Result<RemoteSetupCandidates, String>,
    ) {
        let Some(cancelled) = self.finish_setup_worker(id, SetupKind::Authentication) else {
            return;
        };
        let Screen::SshAuthenticationPending {
            request_id,
            draft,
            fingerprint,
            ..
        } = self.screen.clone()
        else {
            return;
        };
        if request_id != id {
            return;
        }
        if cancelled {
            self.screen = Screen::HostKeyConfirm { draft, fingerprint };
            self.message = Some("SSH authentication cancelled. No connection was saved; retry or return to the form.".into());
            return;
        }
        if let Ok(candidates) = result {
            match self.commit_ssh_destination(&draft, &fingerprint) {
                Ok(setup) => self.show_setup_candidates(setup, candidates),
                Err(error) => {
                    self.screen = Screen::HostKeyConfirm { draft, fingerprint };
                    self.message = Some(error);
                }
            }
        } else {
            self.screen = Screen::HostKeyConfirm { draft, fingerprint };
            self.message = Some(SetupKind::Authentication.failure().into());
        }
    }

    fn show_setup_candidates(
        &mut self,
        setup: super::DestinationSetupState,
        candidates: RemoteSetupCandidates,
    ) {
        self.open_initial_remote_target(setup, Some(candidates));
    }
}

async fn run(
    service: &DestinationSetupService,
    request: SetupRequest,
    cancellation: &CancellationToken,
) -> Result<SetupResult, String> {
    let timeout = match request.kind() {
        SetupKind::Identities => IDENTITY_TIMEOUT,
        SetupKind::HostKey => HOST_KEY_TIMEOUT,
        SetupKind::Authentication => AUTHENTICATION_TIMEOUT,
    };
    run_with_deadline(service, request, cancellation, timeout).await
}

async fn run_with_deadline(
    service: &DestinationSetupService,
    request: SetupRequest,
    cancellation: &CancellationToken,
    timeout: Duration,
) -> Result<SetupResult, String> {
    let kind = request.kind();
    if cancellation.is_cancelled() {
        return Err("SSH setup cancelled before starting.".into());
    }
    match request {
        SetupRequest::Identities => {
            tokio::select! {
                biased;
                () = cancellation.cancelled() => Err("SSH Agent discovery cancelled.".into()),
                result = tokio::time::timeout(timeout, service.discover_local_identities(cancellation)) => {
                    result.ok().and_then(Result::ok).map(SetupResult::Identities).ok_or_else(|| kind.failure().into())
                }
            }
        }
        SetupRequest::HostKey(request) => tokio::time::timeout(
            timeout,
            service.capture_endpoint_identity(&request, Duration::from_secs(10), cancellation),
        )
        .await
        .ok()
        .and_then(Result::ok)
        .map(SetupResult::HostKey)
        .ok_or_else(|| kind.failure().into()),
        SetupRequest::Authentication(request) => tokio::time::timeout(
            timeout,
            service.authenticate_and_probe(
                &request,
                Duration::from_secs(15),
                Duration::from_secs(10),
                cancellation,
            ),
        )
        .await
        .ok()
        .and_then(Result::ok)
        .map(SetupResult::Authenticated)
        .ok_or_else(|| kind.failure().into()),
    }
}

#[cfg(test)]
mod tests;
