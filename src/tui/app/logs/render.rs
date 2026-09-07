use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::Line,
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};
use std::fmt::Write as _;

use crate::{
    telemetry::log_record::LogEventKind,
    tui::presentation::{safe_text, step_label},
};

use super::{LogWorkspace, Mode};

impl LogWorkspace {
    pub(in crate::tui) fn help_text(&self) -> String {
        let scope = self.scope.as_ref().map_or_else(
            || "Deployment ID pending; no historical identity is guessed.".into(),
            |scope| {
                format!(
                    "Project: {}\nEnvironment: {}\nDeployment: {}",
                    scope.project, scope.environment, scope.deployment
                )
            },
        );
        let mut text = format!(
            "Log viewer · {}\n{scope}\n{}\n\nCurrent coverage: {}\nCurrent message: {}\n\n",
            if self.live {
                "live window"
            } else {
                "retained local files"
            },
            self.filter_label(),
            self.coverage_text(),
            safe_text(&self.status)
        );
        match &self.mode {
            Mode::ExportDirectory { browser, name, .. } => {
                let _ = writeln!(
                    text,
                    "Export directory (escaped path): {}\nAutomatic filename: {name}\nEnter opens a directory; Backspace goes to its parent; s selects; Esc discards.\n",
                    EscapedPath(&browser.directory)
                );
            }
            Mode::ExportPreview {
                preview, offset, ..
            } => {
                let chunk = super::preview_chunk(preview.text(), *offset);
                let _ = writeln!(
                    text,
                    "Exact export path (escaped): {}\nExport filename: {:?}\nPayload bytes {}..{} / {}\nOnly unmodified c saves; Esc discards. Existing files are never overwritten. Windows inherits directory permissions; Unix exports are owner-private.\n",
                    EscapedPath(preview.path()),
                    preview.path().file_name(),
                    offset,
                    offset + chunk.len(),
                    preview.byte_len()
                );
            }
            _ => {}
        }
        text.push_str("Up/Down or PgUp/PgDn selects rows; Enter opens the full record; Home/End selects first/latest; Space pauses/resumes live following.\n\np shows all steps, durations and coverage. / searches every retained local file. f selects a Component; t selects a step. Live f/t filters the live window; h searches retained files with the same filter; v returns to this exact deployment's live window and clears filters. n/b navigates retained pages.\n\ny requests copying only the selected row's recorded, redacted failed-command argv as JSON. It is diagnostic, not an executable command. The terminal can refuse the clipboard request; paste to verify.\n\ne exports all matching retained logs; s exports the entire recorded deployment summary, independently of the log filter. Choose a directory with arrows/Enter/Backspace, s selects it. In the preview, n/b selects a text chunk and Up/Down/PgUp/PgDn scrolls within it. Only unmodified c saves; Esc discards. Existing files are never overwritten.\n\nEsc closes an idle overlay or cancels/waits for its local worker. Ctrl+C also requests safe cancellation of the active deployment or rollback. F1/Esc closes this help.");
        text
    }

    pub(in crate::tui) fn render(&self, frame: &mut Frame<'_>, area: Rect) {
        frame.render_widget(Clear, area);
        let compact = area.height < 13;
        let chrome = if compact { 1 } else { 3 };
        let areas = Layout::vertical([
            Constraint::Length(chrome),
            Constraint::Min(0),
            Constraint::Length(chrome),
        ])
        .split(area);
        let source = if self.live {
            "LIVE WINDOW (not complete history)"
        } else {
            "RETAINED LOCAL LOGS"
        };
        let scope = self.scope.as_ref().map_or_else(
            || "Deployment ID pending".into(),
            |scope| format!("Deployment {}", scope.deployment),
        );
        let header = if compact {
            format!(
                "{} · {scope} · F1 context",
                if self.live { "LIVE" } else { "HISTORY" }
            )
        } else {
            format!(
                "{source} · {scope}\n{}\n{}",
                self.filter_label(),
                self.coverage_text()
            )
        };
        frame.render_widget(Paragraph::new(header), areas[0]);
        match &self.mode {
            Mode::Browse => self.render_rows(frame, areas[1]),
            Mode::Search(text) => frame.render_widget(panel(" Search retained files ", format!("Literal, case-insensitive text (up to 128 characters):\n> {}\n\nEnter scans every retained generation; Esc keeps the current display.\nLive output continues in the background.", safe_text(text))), areas[1]),
            Mode::Component { values, selected } => {
                let lines: Vec<_> = values.iter().map(|value| value.as_ref().map_or("All Components".into(), ToString::to_string)).collect();
                render_choices(frame, areas[1], " Component filter from recorded steps ", &lines, *selected);
            }
            Mode::Step { values, selected } => {
                let lines: Vec<_> = values.iter().map(|value| value.as_deref().map_or("All steps".into(), step_label)).collect();
                render_choices(frame, areas[1], " Step filter from recorded steps ", &lines, *selected);
            }
            Mode::Detail { scroll } => {
                let text = self.view.selected().map_or_else(|| "No selected row.".into(), |row| {
                    let evidence = match &row.event.kind {
                        LogEventKind::FailedCommand { command } => serde_json::to_string_pretty(command).unwrap_or_else(|_| "Command unavailable".into()),
                        LogEventKind::CommandUnavailable { .. } => "Failed-command snapshot unavailable; no command is reconstructed.".into(),
                        kind => format!("{kind:?}"),
                    };
                    format!("{}\nSource: {}\nElapsed: {:?} ms (None is unknown)\n\n{}\n\nRecorded evidence:\n{evidence}\n\nUp/Down/PgUp/PgDn scroll; y requests copying this row's recorded failed argv as JSON. Esc returns.", row_label(row), step_label(&row.event.namespace), row.elapsed_ms, row.event.message)
                });
                frame.render_widget(panel(" Selected record (diagnostic only) ", text).scroll((*scroll, 0)), areas[1]);
            }
            Mode::Progress { scroll } => self.render_progress(frame, areas[1], *scroll),
            Mode::ExportDirectory { browser, name, .. } => {
                render_export_directory(frame, areas[1], browser, name);
            }
            Mode::ExportPreview { preview, offset, scroll } => {
                render_export_preview(frame, areas[1], preview, *offset, *scroll);
            }
        }
        let keys = if compact {
            self.compact_keys()
        } else if self.task.is_some() {
            "Local worker active · Esc cancel and wait · Ctrl+C safely cancels deployment too"
        } else {
            "↑↓ rows · Enter detail · p progress · / search · f Component · t step · h history · v live · n/b pages · y copy · e logs · s summary · Esc close"
        };
        let footer = if compact {
            keys.into()
        } else {
            format!("{keys}\n{}", safe_text(&self.status))
        };
        frame.render_widget(Paragraph::new(footer).wrap(Wrap { trim: false }), areas[2]);
    }

    fn filter_label(&self) -> String {
        format!(
            "Component: {} · step: {} · text: {:?}",
            self.query
                .filter
                .component
                .as_ref()
                .map_or("all", |name| name.as_str()),
            self.query.filter.step.as_deref().unwrap_or("all"),
            if self.live {
                self.view.query()
            } else {
                &self.query.filter.text
            }
        )
    }

    fn compact_keys(&self) -> &'static str {
        if self.task.is_some() {
            return "Local worker active · Esc cancel/wait · F1 status · Ctrl+C safe cancel";
        }
        match self.mode {
            Mode::ExportPreview { .. } => {
                "↑↓ scroll · n/b chunks · c save · Esc discard · F1 full path/details"
            }
            Mode::ExportDirectory { .. } => {
                "↑↓ select · Enter open · s choose · Esc back · F1 full path/details"
            }
            Mode::Detail { .. } => "↑↓ scroll · y copy argv · Esc back · F1 context/status",
            Mode::Progress { .. } => "↑↓ scroll · Esc back · F1 context/status",
            Mode::Search(_) => "Type search · Enter retained search · Esc back · F1 context/status",
            Mode::Component { .. } | Mode::Step { .. } => {
                "↑↓ select · Enter apply filter · Esc back · F1 context/status"
            }
            Mode::Browse => {
                "↑↓ rows · Enter detail · p steps · / search · Esc close · F1 all keys/status"
            }
        }
    }

    fn render_progress(&self, frame: &mut Frame<'_>, area: Rect, scroll: u16) {
        let mut text = format!("{}\n\n", self.coverage_text());
        if self.live {
            if let Some(progress) = &self.progress {
                let _ = writeln!(
                    text,
                    "Elapsed: {} ms; execution finished: {}\nProducer gaps: {} rows; {} steps; {} rejected; {} late. Unavailable: {}\n",
                    progress.elapsed_ms,
                    progress.finished,
                    progress.dropped_rows,
                    progress.dropped_steps,
                    progress.rejected_events,
                    progress.late_events,
                    progress.poisoned
                );
                for step in &progress.steps {
                    let end = step.finished_elapsed_ms.or_else(|| {
                        (!progress.finished
                            && step.state == crate::telemetry::log_record::LogStepState::Started)
                            .then_some(progress.elapsed_ms)
                    });
                    let duration = duration_label(step.started_elapsed_ms, end);
                    let _ = writeln!(
                        text,
                        "{} / {}: {:?}; persistence {:?}; start {:?} ms; finish {:?} ms; duration {duration}",
                        step.scope.component,
                        step_label(&step.scope.step),
                        step.state,
                        step.persistence,
                        step.started_elapsed_ms,
                        step.finished_elapsed_ms
                    );
                }
            } else {
                text.push_str("No live progress evidence is available.\n");
            }
        } else {
            text.push_str("Original local journal steps; timestamps in epoch ms, missing values are unknown. These are not fresh remote observations.\n\n");
            for step in &self.recorded_steps {
                let duration = duration_label(step.started_at_ms, step.completed_at_ms);
                let _ = writeln!(
                    text,
                    "{} / {}: {:?}; start {:?}; completion {:?}; duration {duration}",
                    step.component,
                    step_label(&step.name),
                    step.status,
                    step.started_at_ms,
                    step.completed_at_ms
                );
            }
        }
        text.push_str("\nUp/Down/PgUp/PgDn scroll; Esc returns.");
        frame.render_widget(
            panel(" Step progress and coverage ", text).scroll((scroll, 0)),
            area,
        );
    }

    fn coverage_text(&self) -> String {
        if self.live {
            let gaps = self.progress.as_ref().map_or_else(
                || "producer state unavailable".into(),
                |progress| {
                    format!(
                        "producer gaps {}/{}/{}",
                        progress.dropped_rows, progress.dropped_steps, progress.rejected_events
                    )
                },
            );
            return format!(
                "{} · omitted {} · {gaps} · {}",
                if self.view.is_following() {
                    "Following"
                } else {
                    "Paused"
                },
                self.view.omitted(),
                if self.view.selection_evicted() {
                    "Selected row evicted; h loads retained logs"
                } else {
                    "p shows all steps/coverage"
                }
            );
        }
        self.coverage.as_ref().map_or_else(|| "No successful retained-file read yet.".into(), |coverage| format!("{:?} · {} matches · generations {:?} · complete within retained files: {} · issues: {:?}", coverage.status, self.total, coverage.available_generations, coverage.matches_are_complete, coverage.issues))
    }

    fn render_rows(&self, frame: &mut Frame<'_>, area: Rect) {
        let rows = self.view.matching();
        let selected = self.view.selected().map(|row| row.sequence);
        let index = rows
            .iter()
            .position(|row| Some(row.sequence) == selected)
            .unwrap_or(0);
        let count = usize::from(area.height.saturating_sub(2));
        let start = index.saturating_sub(count.saturating_sub(1));
        let lines: Vec<_> = rows
            .iter()
            .skip(start)
            .take(count)
            .map(|row| {
                let text = row.event.message.lines().next().unwrap_or("");
                let command = matches!(row.event.kind, LogEventKind::FailedCommand { .. });
                let value = format!(
                    "{} {}{} {}",
                    if Some(row.sequence) == selected {
                        ">"
                    } else {
                        " "
                    },
                    row_label(row),
                    if command { " [failed argv]" } else { "" },
                    text
                );
                let line = Line::from(safe_text(&value));
                if Some(row.sequence) == selected {
                    line.style(Style::default().add_modifier(Modifier::REVERSED))
                } else {
                    line
                }
            })
            .collect();
        let lines = if rows.is_empty() {
            vec![Line::from(
                "No rows in this display. Check source coverage/status; missing or failed reads are not an empty successful search.",
            )]
        } else {
            lines
        };
        frame.render_widget(
            Paragraph::new(lines).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Logs · Enter reads full selected record "),
            ),
            area,
        );
    }
}

/// Unlike `Path::display()`, this preserves escaped controls and non-UTF-8 bytes
/// when reviewing the complete destination path before confirming a local write.
struct EscapedPath<'a>(&'a std::path::Path);

impl std::fmt::Display for EscapedPath<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(self.0, formatter)
    }
}

fn row_label(row: &crate::tui::log_view::LogRow) -> String {
    row.event.scope.as_ref().map_or_else(
        || "[unscoped]".into(),
        |scope| format!("[{} / {}]", scope.component, step_label(&scope.step)),
    )
}

fn duration_label(start: Option<u64>, end: Option<u64>) -> String {
    start
        .zip(end)
        .and_then(|(start, end)| end.checked_sub(start))
        .map_or_else(|| "unknown".into(), |duration| format!("{duration} ms"))
}

fn panel(title: &'static str, text: String) -> Paragraph<'static> {
    // These bodies have already passed the log/export field sanitizer. The
    // single-line label helper would erase LF and truncate inspectable evidence.
    Paragraph::new(text).wrap(Wrap { trim: false }).block(
        Block::default()
            .borders(Borders::ALL)
            .title(crate::tui::i18n::tr(title)),
    )
}

fn render_choices(
    frame: &mut Frame<'_>,
    area: Rect,
    title: &'static str,
    values: &[String],
    selected: usize,
) {
    let count = usize::from(area.height.saturating_sub(2));
    let start = selected.saturating_sub(count.saturating_sub(1));
    let lines: Vec<_> = values
        .iter()
        .enumerate()
        .skip(start)
        .take(count)
        .map(|(index, value)| {
            let line = Line::from(format!(
                "{} {}",
                if index == selected { ">" } else { " " },
                safe_text(value)
            ));
            if index == selected {
                line.style(Style::default().add_modifier(Modifier::REVERSED))
            } else {
                line
            }
        })
        .collect();
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .title(crate::tui::i18n::tr(title)),
        ),
        area,
    );
}

fn render_export_directory(
    frame: &mut Frame<'_>,
    area: Rect,
    browser: &super::DirectoryBrowser,
    name: &str,
) {
    let lines: Vec<_> = browser
        .children
        .iter()
        .map(|path| {
            path.file_name().map_or_else(
                || crate::tui::presentation::path_label(path),
                |name| name.to_string_lossy().into_owned(),
            )
        })
        .collect();
    let compact = area.height < 7;
    let inner = Layout::vertical([
        Constraint::Length(if compact { 1 } else { 3 }),
        Constraint::Min(0),
    ])
    .split(area);
    let text = if compact {
        "Choose export directory · F1 full path/filename".into()
    } else {
        format!(
            "Directory: {}\nAutomatic filename: {name}\nEnter enters directory; Backspace parent; s selects; Esc cancels.",
            safe_text(&crate::tui::presentation::path_label(&browser.directory))
        )
    };
    frame.render_widget(Paragraph::new(text), inner[0]);
    render_choices(
        frame,
        inner[1],
        " Local export directory ",
        &lines,
        browser.selected,
    );
}

fn render_export_preview(
    frame: &mut Frame<'_>,
    area: Rect,
    preview: &crate::application::local_export::LocalExportPreview,
    offset: usize,
    scroll: u16,
) {
    let chunk = super::preview_chunk(preview.text(), offset);
    let compact = area.height < 8;
    let parts = Layout::vertical([
        Constraint::Length(if compact { 1 } else { 4 }),
        Constraint::Min(0),
    ])
    .split(area);
    let range = format!(
        "Bytes {}..{} / {}",
        offset,
        offset + chunk.len(),
        preview.byte_len()
    );
    let header = if compact {
        format!("{range} · F1 full path and confirmation details")
    } else {
        format!(
            "Path: {}\n{range} · n/b chunk · ↑↓/PgUp/PgDn scroll\nUnmodified c confirms; Esc discards. No overwrite.\nF1 full path/details; Windows inherits directory permissions.",
            safe_text(&crate::tui::presentation::path_label(preview.path()))
        )
    };
    frame.render_widget(Paragraph::new(header), parts[0]);
    frame.render_widget(
        panel(" Exact export payload chunk ", chunk.into()).scroll((scroll, 0)),
        parts[1],
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_does_not_invent_missing_or_reversed_time_evidence() {
        assert_eq!(duration_label(Some(10), Some(42)), "32 ms");
        assert_eq!(duration_label(None, Some(42)), "unknown");
        assert_eq!(duration_label(Some(10), None), "unknown");
        assert_eq!(duration_label(Some(42), Some(10)), "unknown");
    }
}
