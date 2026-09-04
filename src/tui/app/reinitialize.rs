//! Explicit, local-only replacement of damaged managed Project identity.

use std::{fmt::Write as _, path::PathBuf, sync::Arc, time::Instant};

use crossterm::event::KeyCode;
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::Style,
    text::Span,
    widgets::{Block, Borders, Paragraph, Wrap},
};
use tokio_util::sync::CancellationToken;

use crate::{
    application::project_reinitialize::{ProjectReinitializePreview, ProjectReinitializeService},
    config::{ProjectConfig, ReinitializeConfirmation},
    projects::{ProjectRegistry, ProjectStatus, register_initialized_project},
    tui::presentation::{context_label, environment_label, safe_text},
};

use super::{App, BackgroundEvent, Screen, now_unix_ms};

#[derive(Clone, Debug)]
pub(in crate::tui) struct ReinitializeScreen {
    root: PathBuf,
    page: ReinitializePage,
    scroll: usize,
    horizontal: usize,
    document: Option<Arc<ReviewDocument>>,
}

/// Immutable complete text with byte offsets for logical lines. Only the visible
/// line window is rendered, so neither line count nor wrapping uses a u16 offset.
struct ReviewDocument {
    text: String,
    starts: Vec<usize>,
}

impl std::fmt::Debug for ReviewDocument {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ReviewDocument")
            .field("bytes", &self.text.len())
            .field("lines", &self.starts.len())
            .finish_non_exhaustive()
    }
}

impl ReviewDocument {
    fn new(text: String) -> Self {
        let mut starts = vec![0];
        starts.extend(
            text.match_indices('\n')
                .map(|(offset, _)| offset + 1)
                .filter(|offset| *offset < text.len()),
        );
        Self { text, starts }
    }

    fn window(&self, line: usize, horizontal: usize, height: u16) -> String {
        let Some(offset) = self.starts.get(line) else {
            return String::new();
        };
        self.text[*offset..]
            .lines()
            .take(usize::from(height))
            .map(|line| {
                // Ratatui's own grapheme iterator preserves combined Unicode text.
                // Pan by whole graphemes rather than cutting a UTF-8 byte or cluster.
                Span::raw(line)
                    .styled_graphemes(Style::default())
                    .skip(horizontal)
                    .map(|grapheme| grapheme.symbol)
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[derive(Clone, Debug)]
enum ReinitializePage {
    Intro,
    Preview(Arc<ProjectReinitializePreview>),
    Working {
        preview: Option<Arc<ProjectReinitializePreview>>,
        saving: bool,
        started: Instant,
        cancelling: bool,
    },
}

#[derive(Debug)]
pub(super) struct ReinitializeTask {
    id: uuid::Uuid,
    origin: ReinitializeScreen,
    cancellation: CancellationToken,
    worker: Option<std::thread::JoinHandle<()>>,
}

#[derive(Debug)]
pub(super) enum ReinitializeResult {
    Preview(Arc<ProjectReinitializePreview>),
    Saved {
        root: PathBuf,
        config: Arc<ProjectConfig>,
        recent: Option<Vec<ProjectStatus>>,
    },
}

#[derive(Clone, Debug)]
enum ReinitializeRequest {
    Preview(PathBuf),
    Save(Arc<ProjectReinitializePreview>),
}

impl App {
    pub(super) fn open_reinitialize(&mut self, root: PathBuf) {
        if self.reinitialize_busy() {
            self.message = Some("Another operation is active; wait for completion.".into());
            return;
        }
        self.invalidate_attention();
        self.screen = Screen::Reinitialize(ReinitializeScreen {
            root,
            page: ReinitializePage::Intro,
            scroll: 0,
            horizontal: 0,
            document: None,
        });
    }

    pub(super) fn handle_reinitialize(&mut self, key: KeyCode, mut screen: ReinitializeScreen) {
        if self.reinitialize_task.is_some() {
            if key == KeyCode::Esc {
                self.cancel_reinitialize();
            }
            return;
        }
        if key == KeyCode::Esc {
            self.show_projects();
            return;
        }
        let request = match &screen.page {
            ReinitializePage::Intro if matches!(key, KeyCode::Enter | KeyCode::Char('r')) => {
                Some(ReinitializeRequest::Preview(screen.root.clone()))
            }
            ReinitializePage::Preview(preview) if key == KeyCode::Char('c') => {
                Some(ReinitializeRequest::Save(Arc::clone(preview)))
            }
            ReinitializePage::Preview(_) if key == KeyCode::Char('r') => {
                Some(ReinitializeRequest::Preview(screen.root.clone()))
            }
            _ => None,
        };
        screen.scroll = match key {
            KeyCode::Up => screen.scroll.saturating_sub(1),
            KeyCode::Down => screen.scroll.saturating_add(1),
            KeyCode::PageUp => screen.scroll.saturating_sub(10),
            KeyCode::PageDown => screen.scroll.saturating_add(10),
            KeyCode::Home => 0,
            KeyCode::End => screen
                .document
                .as_ref()
                .map_or(0, |document| document.starts.len().saturating_sub(1)),
            _ => screen.scroll,
        };
        if let Some(document) = &screen.document {
            screen.scroll = screen.scroll.min(document.starts.len().saturating_sub(1));
            screen.horizontal = match key {
                KeyCode::Left => screen.horizontal.saturating_sub(1),
                KeyCode::Right => screen.horizontal.saturating_add(1),
                KeyCode::Char('[') => screen.horizontal.saturating_sub(32),
                KeyCode::Char(']') => screen.horizontal.saturating_add(32),
                KeyCode::Char('0') | KeyCode::Home | KeyCode::End => 0,
                _ => screen.horizontal,
            }
            .min(document.text.len());
        }
        if let Some(request) = request {
            self.start_reinitialize(screen, request);
        } else {
            self.screen = Screen::Reinitialize(screen);
        }
    }

    fn reinitialize_busy(&self) -> bool {
        self.reinitialize_task.is_some()
            || self.project_edit_task.is_some()
            || self.management_task.is_some()
            || self.connections_task.is_some()
            || self.deployment_session.is_active()
    }

    fn start_reinitialize(&mut self, origin: ReinitializeScreen, request: ReinitializeRequest) {
        if self.reinitialize_busy() {
            self.message = Some("Another operation is active; wait for completion.".into());
            return;
        }
        let Some(runtime) = self.runtime.clone() else {
            self.message = Some("Background runtime is unavailable; nothing was saved.".into());
            return;
        };
        let id = uuid::Uuid::now_v7();
        let cancellation = CancellationToken::new();
        let worker_cancel = cancellation.clone();
        let service = ProjectReinitializeService::new(
            self.destination_registry_path.clone(),
            Arc::clone(&self.deployment_session),
        );
        let registry = self.registry_path.clone();
        let sender = self.background_sender.clone();
        let saving = matches!(request, ReinitializeRequest::Save(_));
        let preview = match &request {
            ReinitializeRequest::Save(preview) => Some(Arc::clone(preview)),
            ReinitializeRequest::Preview(_) => None,
        };
        let spawned = std::thread::Builder::new()
            .name("shipforge-reinitialize".into())
            .spawn(move || {
                let result = catch_local_worker_failure(|| {
                    runtime.block_on(run_request(&service, &registry, request, &worker_cancel))
                });
                let _ = sender.send(BackgroundEvent::Reinitialize(id, result));
            });
        match spawned {
            Ok(worker) => {
                self.invalidate_attention();
                let mut loading = origin.clone();
                loading.page = ReinitializePage::Working {
                    preview,
                    saving,
                    started: Instant::now(),
                    cancelling: false,
                };
                loading.scroll = 0;
                self.screen = Screen::Reinitialize(loading);
                self.reinitialize_task = Some(ReinitializeTask {
                    id,
                    origin,
                    cancellation,
                    worker: Some(worker),
                });
            }
            Err(_) => {
                self.message =
                    Some("Could not start Project reinitialization; nothing was saved.".into());
            }
        }
    }

    pub(super) fn cancel_reinitialize(&mut self) {
        if let Some(task) = &self.reinitialize_task {
            task.cancellation.cancel();
            if let Screen::Reinitialize(ReinitializeScreen {
                page: ReinitializePage::Working { cancelling, .. },
                ..
            }) = &mut self.screen
            {
                *cancelling = true;
            }
        }
    }

    pub(super) fn finish_reinitialize(
        &mut self,
        id: uuid::Uuid,
        result: Result<ReinitializeResult, String>,
    ) {
        if self
            .reinitialize_task
            .as_ref()
            .is_none_or(|task| task.id != id)
        {
            return;
        }
        let Some(mut task) = self.reinitialize_task.take() else {
            return;
        };
        // Consume the final event and then join the worker before navigation or exit.
        if let Some(worker) = task.worker.take() {
            let _ = worker.join();
        }
        match result {
            Ok(ReinitializeResult::Saved {
                root,
                config,
                recent,
            }) => {
                let registered = recent.is_some();
                if let Some(recent) = recent {
                    self.recent = recent;
                    self.recent_unavailable = false;
                } else {
                    self.recent_unavailable = true;
                    for status in &mut self.recent {
                        status.available = false;
                    }
                }
                self.show_overview(root, (*config).clone());
                self.message = Some(if registered {
                    "Saved shipforge.yaml with NEW Project and Environment identities. No deployment was performed."
                } else {
                    "Saved shipforge.yaml with NEW identities, but recent-Project registration or refresh failed. Open the directory again; no deployment was performed."
                }.into());
            }
            Ok(ReinitializeResult::Preview(preview)) if !task.cancellation.is_cancelled() => {
                let document = Arc::new(ReviewDocument::new(preview_text(&preview)));
                self.screen = Screen::Reinitialize(ReinitializeScreen {
                    root: preview.root().to_owned(),
                    page: ReinitializePage::Preview(preview),
                    scroll: 0,
                    horizontal: 0,
                    document: Some(document),
                });
            }
            Ok(ReinitializeResult::Preview(_)) => {
                self.show_projects();
                self.message =
                    Some("Project reinitialization cancelled; nothing was saved.".into());
            }
            Err(error) => {
                if task.cancellation.is_cancelled() {
                    self.show_projects();
                } else {
                    // A failed save cannot reuse a potentially stale preview. Retry
                    // prepares fresh local evidence and requires confirmation again.
                    self.screen = Screen::Reinitialize(ReinitializeScreen {
                        page: ReinitializePage::Intro,
                        scroll: 0,
                        horizontal: 0,
                        document: None,
                        ..task.origin
                    });
                }
                self.message = Some(safe_text(&error));
            }
        }
    }
}

async fn run_request(
    service: &ProjectReinitializeService,
    registry: &std::path::Path,
    request: ReinitializeRequest,
    cancellation: &CancellationToken,
) -> Result<ReinitializeResult, String> {
    match request {
        ReinitializeRequest::Preview(root) => service
            .preview(&root, cancellation)
            .await
            .map(|preview| ReinitializeResult::Preview(Arc::new(preview)))
            .map_err(|error| error.to_string()),
        ReinitializeRequest::Save(preview) => {
            let root = preview.root().to_owned();
            let config = service
                .save(
                    (*preview).clone(),
                    ReinitializeConfirmation::confirmed(),
                    cancellation,
                )
                .await
                .map_err(|error| error.to_string())?;
            // Registration is auxiliary. Preserve the known save even if this
            // optional follow-up fails or cancellation arrives after the commit.
            let recent = catch_local_worker_failure(|| {
                register_initialized_project(registry, &root, now_unix_ms())
                    .map_err(|_| "Recent-Project registration failed.".to_owned())?;
                ProjectRegistry::load(registry)
                    .map(|registry| registry.statuses())
                    .map_err(|_| "Recent-Project refresh failed.".to_owned())
            })
            .ok();
            Ok(ReinitializeResult::Saved {
                root,
                config: Arc::new(config),
                recent,
            })
        }
    }
}

fn catch_local_worker_failure<T>(
    operation: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(operation)).unwrap_or_else(|_| {
        Err("Local Project reinitialization stopped unexpectedly; the local YAML save outcome is unconfirmed. Reload this directory before retrying. No remote operation was requested.".into())
    })
}

impl ReinitializeScreen {
    pub(super) fn requires_plain_confirmation(&self) -> bool {
        matches!(self.page, ReinitializePage::Preview(_))
    }

    pub(in crate::tui) fn help(&self) -> &'static str {
        match self.page {
            ReinitializePage::Intro => "Esc return   Enter/r check and preview (no save)",
            ReinitializePage::Preview(_) => {
                "Esc discard c confirm NEW Project ↑↓ PgUp/Dn Home/End ←→ pan [/] jump 0 reset"
            }
            ReinitializePage::Working {
                cancelling: true, ..
            } => "Cancellation requested; waiting for the final local save outcome",
            ReinitializePage::Working { .. } => {
                "Esc cancel and wait; do not close while local work is active"
            }
        }
    }

    pub(in crate::tui) fn context_label(&self) -> String {
        let (page, preview) = match &self.page {
            ReinitializePage::Intro => ("Project / Reinitialize local identity", None),
            ReinitializePage::Preview(preview) => ("Project / Confirm NEW identity", Some(preview)),
            ReinitializePage::Working { preview, .. } => {
                ("Project / Reinitialize working", preview.as_ref())
            }
        };
        context_label(
            page,
            preview.map(|preview| preview.config().project.as_str()),
            None,
            None,
        )
    }

    pub(in crate::tui) fn render(&self, frame: &mut Frame<'_>, area: Rect) {
        let [warning, body] =
            Layout::vertical([Constraint::Length(3), Constraint::Min(0)]).areas(area);
        frame.render_widget(Paragraph::new("NEW Project identity replaces the local YAML only. Existing deployments and history are not adopted. No remote operation is performed.").wrap(Wrap { trim: false }), warning);
        if let Some(document) = &self.document
            && matches!(self.page, ReinitializePage::Preview(_))
        {
            let block = Block::default().borders(Borders::ALL).title(format!(
                "Exact YAML — row {}/{} · pan {} · F1 keys",
                self.scroll.saturating_add(1),
                document.starts.len(),
                self.horizontal,
            ));
            let inner = block.inner(body);
            let text = document.window(self.scroll, self.horizontal, inner.height);
            frame.render_widget(Paragraph::new(text).block(block), body);
            return;
        }
        let text = match &self.page {
            ReinitializePage::Intro => format!(
                "Directory: {}\n\nOnly a present shipforge.yaml with missing or damaged _shipforge can be reinitialized. Human configuration must remain valid.\n\nEnter/r checks the file and connections, then shows exact YAML with fresh IDs. It does not save. Existing reliable roots are retained; ambiguous roots are rejected. A fully missing managed section uses new-Project defaults where root is omitted.\n\nMissing YAML uses ordinary new-Project setup. Esc returns without changes.",
                safe_text(&self.root.to_string_lossy())
            ),
            ReinitializePage::Preview(preview) => preview_text(preview),
            ReinitializePage::Working {
                saving,
                started,
                cancelling,
                ..
            } => format!(
                "{}\nElapsed: {}s\n{}",
                if *saving {
                    "Saving the exact confirmed YAML, then registering this Project locally."
                } else {
                    "Checking local YAML and connection evidence; nothing has been saved."
                },
                started.elapsed().as_secs(),
                if *cancelling {
                    "Cancellation requested. Waiting for the final outcome; a completed save will still be shown."
                } else {
                    "Esc requests cancellation and waits for completion."
                }
            ),
        };
        frame.render_widget(
            Paragraph::new(text)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title("Reinitialize as NEW Project — local only"),
                )
                .wrap(Wrap { trim: false })
                .scroll((u16::try_from(self.scroll).unwrap_or(u16::MAX), 0)),
            body,
        );
    }
}

fn preview_text(preview: &ProjectReinitializePreview) -> String {
    let config = preview.config();
    let mut text = format!(
        "Project: {}\nNEW Project ID: {}\nFile: {}\n",
        safe_text(&config.project),
        config.project_id,
        safe_text(
            &preview
                .root()
                .join(crate::config::PROJECT_FILE)
                .to_string_lossy()
        )
    );
    for (name, environment) in &config.environments {
        let _ = writeln!(
            text,
            "\nEnvironment {} — NEW ID {}",
            environment_label(name),
            environment.id
        );
        for (component, target) in &environment.components {
            let destination = preview
                .destinations()
                .iter()
                .find(|destination| destination.key == target.destination)
                .map_or_else(
                    || target.destination.to_string(),
                    |destination| {
                        format!("{} · {}", destination.key, safe_text(&destination.endpoint))
                    },
                );
            let _ = writeln!(
                text,
                "  {component}: {destination}\n  root: {}",
                safe_text(&target.root)
            );
        }
    }
    text.push_str("\nPress plain c to replace local YAML with the exact preview below. New identities do not adopt old deployments. Connection labels are not connectivity or health checks.\n\n");
    // The service rejects unsafe display characters before constructing this
    // immutable preview. Do not truncate or sanitize exact YAML: a long valid
    // argument must be reviewable through logical-line scrolling and panning.
    text.push_str(preview.yaml());
    text
}

#[cfg(test)]
mod tests;
