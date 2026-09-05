//! Log interactions are an overlay: they never replace the active operation route.

mod render;
#[cfg(test)]
mod tests;
mod worker;

use std::{path::PathBuf, sync::Arc};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::{
    application::{
        history_query::{HistoryLogScope, LogCoverage, LogCursor, LogEntryContent, LogReadQuery},
        local_export::LocalExportPreview,
    },
    domain::ComponentName,
    telemetry::log_record::{LogEvent, LogEventKind, LogScope},
    tui::{
        clipboard::ClipboardOutcome,
        log_view::{LogRow, LogView},
    },
};

use super::{App, DirectoryBrowser};
use worker::{LogTask, Request, Response};

#[derive(Debug)]
pub(in crate::tui) struct LogWorkspace {
    scope: Option<HistoryLogScope>,
    live: bool,
    view: LogView,
    query: LogReadQuery,
    previous: Vec<Option<LogCursor>>,
    next: Option<LogCursor>,
    total: usize,
    coverage: Option<LogCoverage>,
    scopes: Vec<LogScope>,
    progress: Option<crate::tui::live_progress::LiveSnapshot>,
    recorded_steps: Vec<crate::history::StepRecord>,
    mode: Mode,
    task: Option<LogTask>,
    status: String,
    export_directory: PathBuf,
}

#[derive(Debug)]
enum Mode {
    Browse,
    Search(String),
    Component {
        values: Vec<Option<ComponentName>>,
        selected: usize,
    },
    Step {
        values: Vec<Option<String>>,
        selected: usize,
    },
    Detail {
        scroll: u16,
    },
    Progress {
        scroll: u16,
    },
    ExportDirectory {
        browser: DirectoryBrowser,
        payload: String,
        name: String,
    },
    ExportPreview {
        preview: LocalExportPreview,
        offset: usize,
        scroll: u16,
    },
}

impl LogWorkspace {
    fn handle_list_key(&mut self, key: KeyCode) -> bool {
        match key {
            KeyCode::Up => self.view.move_selection(false, 1),
            KeyCode::Down => self.view.move_selection(true, 1),
            KeyCode::PageUp => self.view.move_selection(false, 10),
            KeyCode::PageDown => self.view.move_selection(true, 10),
            KeyCode::Home => self.view.first(),
            KeyCode::End => self.view.follow_latest(),
            KeyCode::Char(' ') => {
                if self.view.is_following() {
                    self.view.move_selection(false, 0);
                } else {
                    self.view.follow_latest();
                }
            }
            KeyCode::Enter => self.mode = Mode::Detail { scroll: 0 },
            KeyCode::Char('p') => self.mode = Mode::Progress { scroll: 0 },
            KeyCode::Char('/') => self.mode = Mode::Search(self.query.filter.text.clone()),
            _ => return false,
        }
        true
    }

    fn open_filter_choice(&mut self, key: KeyCode) {
        if key == KeyCode::Char('f') {
            let names: std::collections::BTreeSet<_> = self
                .scopes
                .iter()
                .map(|scope| scope.component.clone())
                .collect();
            self.mode = Mode::Component {
                values: std::iter::once(None)
                    .chain(names.into_iter().map(Some))
                    .collect(),
                selected: 0,
            };
        } else {
            let steps: std::collections::BTreeSet<_> = self
                .scopes
                .iter()
                .filter(|scope| {
                    self.query
                        .filter
                        .component
                        .as_ref()
                        .is_none_or(|component| component == &scope.component)
                })
                .map(|scope| scope.step.clone())
                .collect();
            self.mode = Mode::Step {
                values: std::iter::once(None)
                    .chain(steps.into_iter().map(Some))
                    .collect(),
                selected: 0,
            };
        }
    }

    fn apply_filter(&mut self, history: PathBuf) {
        if self.live {
            if self.view.set_filter(&self.query.filter) {
                self.status = "Filter applied to the live window only. h searches retained files; export applies this filter to all retained files.".into();
            } else {
                self.status =
                    "The live filter exceeds its safe limits; no filter change was applied.".into();
            }
        } else {
            self.read(history, true);
        }
    }

    fn new(scope: Option<HistoryLogScope>, directory: PathBuf) -> Self {
        Self {
            scope,
            live: false,
            view: LogView::default(),
            query: LogReadQuery::default(),
            previous: Vec::new(),
            next: None,
            total: 0,
            coverage: None,
            scopes: Vec::new(),
            progress: None,
            recorded_steps: Vec::new(),
            mode: Mode::Browse,
            task: None,
            status: String::new(),
            export_directory: directory,
        }
    }

    fn start(&mut self, history: PathBuf, request: Request) {
        if self.task.is_some() {
            return;
        }
        match LogTask::spawn(history, request) {
            Ok(task) => {
                self.status = "Working locally; Esc cancels this read/export. Ctrl+C also cancels an active deployment safely.".into();
                self.task = Some(task);
            }
            Err(()) => {
                self.status =
                    "Could not start the local log operation. No export was written.".into();
            }
        }
    }

    fn read(&mut self, history: PathBuf, reset: bool) {
        let Some(scope) = self.scope.clone() else {
            self.status = "The deployment has not supplied a history ID. Retained logs are unavailable; no ID is guessed.".into();
            return;
        };
        if reset {
            self.query.cursor = None;
            self.previous.clear();
        }
        self.live = false;
        self.mode = Mode::Browse;
        self.view = LogView::default();
        self.coverage = None;
        self.next = None;
        self.total = 0;
        self.recorded_steps.clear();
        self.start(
            history,
            Request::Read {
                scope,
                query: Box::new(self.query.clone()),
            },
        );
    }

    fn accept(&mut self, response: Response) {
        match response {
            Response::Page { page, steps } => {
                let mut view = LogView::default();
                for (sequence, entry) in page.entries.into_iter().enumerate() {
                    let (event, elapsed_ms) = match entry.content {
                        LogEntryContent::Structured(record) => {
                            (record.event, Some(record.elapsed_ms))
                        }
                        LogEntryContent::Legacy(message) => (
                            LogEvent {
                                namespace: "legacy".into(),
                                message,
                                scope: None,
                                kind: LogEventKind::Output,
                            },
                            None,
                        ),
                    };
                    view.push(LogRow {
                        sequence: sequence as u64,
                        elapsed_ms,
                        event: Arc::new(event),
                    });
                }
                view.first();
                self.view = view;
                self.next = page.next_cursor;
                self.total = page.total_matches;
                let status = page.coverage.status;
                self.coverage = Some(page.coverage);
                self.scopes = steps
                    .iter()
                    .map(|step| LogScope {
                        component: step.component.clone(),
                        step: step.name.clone(),
                    })
                    .collect();
                self.recorded_steps = steps;
                self.status = match status {
                    crate::application::history_query::HistoricalLogStatus::Ready => "Loaded retained local files. Older deleted rotations are not recoverable. The filter applies across retained files, not just this page.",
                    crate::application::history_query::HistoricalLogStatus::NotIndexed => "No log index exists for this deployment. Logs are unavailable, not an empty successful search. Original recorded steps remain available with p; r retries.",
                    crate::application::history_query::HistoricalLogStatus::Missing => "Indexed log files are missing. Log contents are unknown, not an empty successful search. Original recorded steps remain available with p; r retries.",
                }.into();
            }
            Response::Payload {
                text,
                name,
                browser,
            } => {
                self.mode = Mode::ExportDirectory {
                    browser,
                    payload: text,
                    name,
                };
                self.status = "Choose a local directory, then review the exact payload. No file has been written.".into();
            }
            Response::Directory {
                browser,
                payload,
                name,
            } => {
                self.mode = Mode::ExportDirectory {
                    browser,
                    payload,
                    name,
                };
            }
            Response::Preview(preview) => {
                self.mode = Mode::ExportPreview {
                    preview,
                    offset: 0,
                    scroll: 0,
                };
                self.status = "Only unmodified c writes the previewed file. Existing files are never overwritten.".into();
            }
            Response::Published(outcome) => {
                self.mode = Mode::Browse;
                self.status = format!(
                    "Published: {}. {}",
                    outcome.path().display(),
                    outcome.warning().unwrap_or("Export complete.")
                );
            }
        }
    }
}

impl App {
    fn live_log_scope(&self) -> Option<HistoryLogScope> {
        let (project, environment) = self.live_environment.as_ref()?;
        let deployment = self.live_progress.as_ref()?.snapshot().deployment?;
        Some(HistoryLogScope {
            project: project.clone(),
            environment: environment.clone(),
            deployment,
        })
    }

    fn log_history_path(&self) -> PathBuf {
        self.destination_registry_path
            .with_file_name("history.sqlite3")
    }

    pub(super) fn open_live_logs(&mut self) {
        let mut workspace =
            LogWorkspace::new(self.live_log_scope(), self.initial_directory.clone());
        workspace.live = true;
        workspace.view = self.live_logs.clone();
        workspace.status = "Live window only. h loads retained history; / searches retained files. Browsing does not cancel the deployment.".into();
        self.log_workspace = Some(workspace);
        self.poll_live_logs();
    }

    pub(super) fn open_history_logs(&mut self, scope: HistoryLogScope, root: PathBuf) {
        let mut workspace = LogWorkspace::new(Some(scope), root);
        workspace.read(self.log_history_path(), true);
        self.log_workspace = Some(workspace);
    }

    pub(super) fn poll_live_logs(&mut self) -> bool {
        let Some(progress) = &self.live_progress else {
            return false;
        };
        let update = progress.drain();
        let changed = !update.rows.is_empty();
        let scope = self.live_log_scope();
        if let Some(workspace) = &mut self.log_workspace
            && workspace.live
        {
            workspace.scope = scope;
            workspace.scopes = update
                .snapshot
                .steps
                .iter()
                .map(|step| step.scope.clone())
                .collect();
            workspace.progress = Some(update.snapshot.clone());
        }
        for row in update.rows {
            if let Some(workspace) = &mut self.log_workspace
                && workspace.live
            {
                workspace.view.push(row.clone());
            }
            self.live_logs.push(row);
        }
        changed
    }

    pub(super) fn poll_log_workspace(&mut self) -> bool {
        let Some(workspace) = &mut self.log_workspace else {
            return false;
        };
        if !workspace.task.as_ref().is_some_and(LogTask::is_finished) {
            return false;
        }
        let Some(task) = workspace.task.take() else {
            return false;
        };
        match task.join() {
            Ok(response) => workspace.accept(response),
            Err(message) => {
                workspace.mode = Mode::Browse;
                workspace.status =
                    format!("{message} No successful refresh is claimed. r starts a fresh read.");
            }
        }
        true
    }

    pub(super) fn log_workspace_busy(&self) -> bool {
        self.log_workspace
            .as_ref()
            .is_some_and(|workspace| workspace.task.is_some())
    }

    pub(super) fn cancel_log_operation(&self) {
        if let Some(task) = self
            .log_workspace
            .as_ref()
            .and_then(|workspace| workspace.task.as_ref())
        {
            task.cancel();
        }
    }

    pub(in crate::tui) fn take_clipboard_request(&mut self) -> Option<String> {
        self.pending_clipboard.take()
    }

    pub(in crate::tui) fn finish_clipboard_request(&mut self, result: ClipboardOutcome) {
        let message = match result {
            ClipboardOutcome::Sent => {
                "Clipboard request sent. Paste to verify; the terminal may reject OSC 52. Redacted argv is diagnostic, not a runnable command."
            }
            ClipboardOutcome::Unsupported => {
                "This terminal transport does not support clipboard requests. Use export instead."
            }
            ClipboardOutcome::Rejected => {
                "Clipboard payload was rejected by the size/control checks. Nothing was copied; use export."
            }
            ClipboardOutcome::Failed => {
                "Clipboard write was incomplete or failed. It was not retried; do not assume the clipboard changed."
            }
        };
        self.message = Some(message.into());
        if let Some(workspace) = &mut self.log_workspace {
            workspace.status = message.into();
        }
    }

    pub(super) fn handle_log_key(&mut self, key: KeyEvent) {
        if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('c') {
            self.request_deployment_cancellation();
            return;
        }
        let Some(mut workspace) = self.log_workspace.take() else {
            return;
        };
        // Only search text accepts Shift. No modified shortcut authorizes output.
        let text_mode = matches!(workspace.mode, Mode::Search(_));
        if (!text_mode && !key.modifiers.is_empty())
            || !key.modifiers.difference(KeyModifiers::SHIFT).is_empty()
        {
            self.log_workspace = Some(workspace);
            return;
        }
        if let Some(task) = &workspace.task {
            if key.code == KeyCode::Esc {
                task.cancel();
                workspace.status =
                    "Cancellation requested; waiting for the local worker before closing.".into();
            }
            self.log_workspace = Some(workspace);
            return;
        }
        let mode = std::mem::replace(&mut workspace.mode, Mode::Browse);
        if !self.handle_log_mode(&mut workspace, mode, key.code) {
            self.log_workspace = Some(workspace);
        }
    }

    /// Returns true only for an explicit close of an idle browsing overlay.
    fn handle_log_mode(&mut self, workspace: &mut LogWorkspace, mode: Mode, key: KeyCode) -> bool {
        let history = self.log_history_path();
        match mode {
            Mode::Browse => return self.handle_log_browse(workspace, key, history),
            Mode::Search(mut text) => match key {
                KeyCode::Esc => {}
                KeyCode::Enter => {
                    workspace.query.filter.text = text;
                    workspace.read(history, true);
                }
                KeyCode::Backspace => {
                    text.pop();
                    workspace.mode = Mode::Search(text);
                }
                KeyCode::Char(value)
                    if !value.is_control()
                        && text.chars().count() < 128
                        && text.len() + value.len_utf8() <= 512 =>
                {
                    text.push(value);
                    workspace.mode = Mode::Search(text);
                }
                _ => workspace.mode = Mode::Search(text),
            },
            Mode::Component {
                values,
                mut selected,
            } => {
                move_choice(key, &mut selected, values.len());
                if key == KeyCode::Enter {
                    workspace.query.filter.component = values.get(selected).cloned().flatten();
                    workspace.query.filter.step = None;
                    workspace.apply_filter(history);
                } else if key != KeyCode::Esc {
                    workspace.mode = Mode::Component { values, selected };
                }
            }
            Mode::Step {
                values,
                mut selected,
            } => {
                move_choice(key, &mut selected, values.len());
                if key == KeyCode::Enter {
                    workspace.query.filter.step = values.get(selected).cloned().flatten();
                    workspace.apply_filter(history);
                } else if key != KeyCode::Esc {
                    workspace.mode = Mode::Step { values, selected };
                }
            }
            Mode::Detail { mut scroll } => {
                scroll_text(key, &mut scroll);
                if key == KeyCode::Char('y') {
                    self.copy_failed_command(workspace);
                }
                if !matches!(key, KeyCode::Esc | KeyCode::Enter) {
                    workspace.mode = Mode::Detail { scroll };
                }
            }
            Mode::Progress { mut scroll } => {
                scroll_text(key, &mut scroll);
                if !matches!(key, KeyCode::Esc | KeyCode::Enter) {
                    workspace.mode = Mode::Progress { scroll };
                }
            }
            Mode::ExportDirectory {
                browser,
                payload,
                name,
            } => {
                handle_export_directory(workspace, key, browser, payload, name, history);
            }
            Mode::ExportPreview {
                preview,
                mut offset,
                mut scroll,
            } => {
                scroll_text(key, &mut scroll);
                let old_offset = offset;
                offset = preview_offset(preview.text(), offset, key);
                if old_offset != offset {
                    scroll = 0;
                }
                if key == KeyCode::Char('c') {
                    workspace.start(history, Request::Save(preview));
                } else if key != KeyCode::Esc {
                    workspace.mode = Mode::ExportPreview {
                        preview,
                        offset,
                        scroll,
                    };
                }
            }
        }
        false
    }

    fn handle_log_browse(
        &mut self,
        workspace: &mut LogWorkspace,
        key: KeyCode,
        history: PathBuf,
    ) -> bool {
        if workspace.handle_list_key(key) {
            return false;
        }
        match key {
            KeyCode::Esc => return true,
            KeyCode::Char('h' | 'r') => workspace.read(history, true),
            KeyCode::Char('v') => {
                if workspace.scope.is_some() && workspace.scope == self.live_log_scope() {
                    workspace.live = true;
                    workspace.view = self.live_logs.clone();
                    workspace.query = LogReadQuery::default();
                    workspace.coverage = None;
                    workspace.status =
                        "Returned to the bounded live window; retained-file filters are cleared."
                            .into();
                } else {
                    workspace.status = "No live window belongs to this exact deployment.".into();
                }
            }
            KeyCode::Char('n') if workspace.next.is_some() && !workspace.live => {
                if workspace.previous.len() < 128 {
                    workspace.previous.push(workspace.query.cursor.clone());
                    workspace.query.cursor.clone_from(&workspace.next);
                    workspace.read(history, false);
                } else {
                    workspace.status = "Page navigation limit reached. Refine the search/filter or export retained matches.".into();
                }
            }
            KeyCode::Char('b') if !workspace.live => {
                if let Some(cursor) = workspace.previous.pop() {
                    workspace.query.cursor = cursor;
                    workspace.read(history, false);
                }
            }
            KeyCode::Char('f' | 't') => workspace.open_filter_choice(key),
            KeyCode::Char('y') => self.copy_failed_command(workspace),
            KeyCode::Char('e' | 's') => {
                if let Some(scope) = workspace.scope.clone() {
                    workspace.start(
                        history,
                        Request::Payload {
                            scope,
                            filter: workspace.query.filter.clone(),
                            summary: key == KeyCode::Char('s'),
                            directory: workspace.export_directory.clone(),
                        },
                    );
                } else {
                    workspace.status = "No persisted deployment ID is available for export.".into();
                }
            }
            _ => {}
        }
        false
    }

    fn copy_failed_command(&mut self, workspace: &mut LogWorkspace) {
        let Some(command) = workspace.view.failed_command() else {
            workspace.status = "This row has no complete recorded failed-command argv. Old logs and unavailable snapshots cannot be reconstructed from current YAML.".into();
            return;
        };
        // JSON preserves argv boundaries on every host; never construct an executable shell string.
        match serde_json::to_string_pretty(command) {
            Ok(text) => {
                self.pending_clipboard = Some(text);
                workspace.status = "Preparing one explicit terminal clipboard request.".into();
            }
            Err(_) => {
                workspace.status =
                    "The recorded command could not be encoded. Nothing was copied.".into();
            }
        }
    }
}

fn handle_export_directory(
    workspace: &mut LogWorkspace,
    key: KeyCode,
    mut browser: DirectoryBrowser,
    payload: String,
    name: String,
    history: PathBuf,
) {
    match key {
        KeyCode::Up => browser.selected = browser.selected.saturating_sub(1),
        KeyCode::Down => {
            browser.selected = browser
                .selected
                .saturating_add(1)
                .min(browser.children.len().saturating_sub(1));
        }
        KeyCode::Char('s') => {
            workspace.export_directory.clone_from(&browser.directory);
            workspace.start(
                history,
                Request::Prepare {
                    directory: browser.directory,
                    name,
                    payload,
                },
            );
            return;
        }
        KeyCode::Enter | KeyCode::Backspace => {
            let target = if key == KeyCode::Enter {
                browser.selected_child()
            } else {
                browser.directory.parent()
            };
            if let Some(directory) = target {
                workspace.start(
                    history,
                    Request::Directory {
                        directory: directory.to_path_buf(),
                        payload,
                        name,
                    },
                );
                return;
            }
        }
        KeyCode::Esc => return,
        _ => {}
    }
    workspace.mode = Mode::ExportDirectory {
        browser,
        payload,
        name,
    };
}

fn move_choice(key: KeyCode, selected: &mut usize, len: usize) {
    match key {
        KeyCode::Up => *selected = selected.saturating_sub(1),
        KeyCode::Down => *selected = selected.saturating_add(1).min(len.saturating_sub(1)),
        KeyCode::PageUp => *selected = selected.saturating_sub(10),
        KeyCode::PageDown => *selected = selected.saturating_add(10).min(len.saturating_sub(1)),
        KeyCode::Home => *selected = 0,
        KeyCode::End => *selected = len.saturating_sub(1),
        _ => {}
    }
}

fn scroll_text(key: KeyCode, scroll: &mut u16) {
    *scroll = match key {
        KeyCode::Up => scroll.saturating_sub(1),
        KeyCode::Down => scroll.saturating_add(1),
        KeyCode::PageUp => scroll.saturating_sub(10),
        KeyCode::PageDown => scroll.saturating_add(10),
        KeyCode::Home => 0,
        _ => *scroll,
    };
}

const PREVIEW_CHUNK_BYTES: usize = 4096;

fn preview_offset(text: &str, offset: usize, key: KeyCode) -> usize {
    let mut next = match key {
        KeyCode::Char('n') => offset
            .saturating_add(PREVIEW_CHUNK_BYTES)
            .min(text.len().saturating_sub(1)),
        KeyCode::Char('b') => offset.saturating_sub(PREVIEW_CHUNK_BYTES),
        KeyCode::Home => 0,
        KeyCode::End => text.len().saturating_sub(PREVIEW_CHUNK_BYTES),
        _ => offset,
    };
    while !text.is_char_boundary(next) {
        next -= 1;
    }
    next
}

fn preview_chunk(text: &str, offset: usize) -> &str {
    let mut end = offset.saturating_add(PREVIEW_CHUNK_BYTES).min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[offset..end]
}
