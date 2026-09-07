use ratatui::{
    Frame,
    layout::Rect,
    text::Line,
    widgets::{Block, Borders, Paragraph},
};

use super::{Page, RemoteSetupSelectionState, TextField};
use crate::{
    application::SetupRootState,
    tui::{presentation::context_label, safe_text, selected_line},
};

impl RemoteSetupSelectionState {
    pub(in crate::tui) fn context_label(&self) -> String {
        context_label(
            "Target choices (draft only)",
            Some(&self.project),
            Some(&self.environment),
            Some(self.component.as_str()),
        )
    }

    pub(in crate::tui) fn help(&self) -> &'static str {
        match &self.page {
            Page::Services => {
                "Esc back · Enter apply · c commands · m unit · r root · b browse · v inspect"
            }
            Page::CommandEditor(editor) => editor.help(),
            Page::Directories { candidates, .. }
                if candidates.directory == "/" && candidates.directories.is_empty() =>
            {
                "g path · f refresh · Esc back · / is browsing only"
            }
            Page::Directories { candidates, .. } if candidates.directory == "/" => {
                "↑↓ choose · Enter open · g path · f refresh · Esc back · / browsing only"
            }
            Page::Directories { candidates, .. } if candidates.directories.is_empty() => {
                "s select current · Backspace parent · g path · f refresh · Esc back"
            }
            Page::Directories { .. } => {
                "↑↓ choose · Enter open · Backspace parent · s current · g path · f refresh · Esc back"
            }
            Page::Unavailable { .. } => "f retry this directory · g another path · Esc back",
            Page::Text { .. } => {
                "Type value · Delete clear · ←→ scroll · Home/End · Enter use · Esc back"
            }
            Page::Loading { cancelling: false } => {
                "Read-only discovery · Esc/Ctrl+C cancel and wait · F1 help"
            }
            Page::Loading { cancelling: true } => {
                "Cancellation requested; waiting for discovery and disconnect to finish"
            }
        }
    }

    pub(in crate::tui) fn render(&self, frame: &mut Frame<'_>, area: Rect) {
        if area.width < 80 || area.height < 7 {
            frame.render_widget(
                Paragraph::new("Resize to at least 80x10; Esc back/cancel."),
                area,
            );
            return;
        }
        let Content {
            title,
            mut fixed,
            rows,
            selected,
        } = match &self.page {
            Page::Services => self.service_content(),
            Page::CommandEditor(editor) => {
                editor.render(frame, area);
                return;
            }
            Page::Directories { candidates, cursor } => directory_content(candidates, *cursor),
            Page::Unavailable { retry_path } => Content::fixed(
                " Remote directory unavailable ",
                vec![
                    Line::from(format!("Requested directory: {}", safe_text(retry_path))),
                    Line::from(
                        "Directory contents are unknown: the last read failed or was cancelled.",
                    ),
                    Line::from(
                        "No cached entries are selectable; this does not mean the directory is empty.",
                    ),
                    Line::from(
                        "Press f to retry, g to enter another path, or Esc to return to target settings.",
                    ),
                ],
            ),
            Page::Text {
                field,
                value,
                offset,
            } => text_content(*field, value, *offset),
            Page::Loading { cancelling } => Content::fixed(
                " Read-only target discovery ",
                vec![
                    Line::from(if *cancelling {
                        "Cancelling; waiting for the tracked worker to finish."
                    } else {
                        "Reading remote candidates using the saved SSH host-key pin…"
                    }),
                    Line::from("No paths are created and no services are changed."),
                ],
            ),
        };
        let height = usize::from(area.height.saturating_sub(2));
        // Context is fixed; only candidate rows scroll. In particular, s always
        // selects the visible Directory header, never the highlighted child.
        // Optional help/notes yield to the focused choice on short terminals.
        // At the supported minimum, Connection + Root + Observation still fit.
        fixed.truncate(height.saturating_sub(usize::from(!rows.is_empty())));
        let available = height.saturating_sub(fixed.len());
        let first = selected.map_or(0, |row| row.saturating_sub(available.saturating_sub(1)));
        fixed.extend(rows.into_iter().skip(first).take(available));
        frame.render_widget(
            Paragraph::new(fixed).block(
                Block::default()
                    .title(crate::tui::i18n::tr(title))
                    .borders(Borders::ALL),
            ),
            area,
        );
    }

    fn service_content(&self) -> Content {
        let state = match self.root_state {
            None => "not inspected for this root",
            Some(SetupRootState::Missing) => {
                "missing when inspected; deployment checks parent access"
            }
            Some(SetupRootState::WritableDirectory) => {
                "writable when inspected; not deployment/health proof"
            }
            Some(SetupRootState::ReadOnlyDirectory) => "read-only when inspected",
            Some(SetupRootState::NotDirectory) => "not a directory when inspected",
        };
        let mut fixed = vec![
            Line::from(format!(
                "Connection: {}",
                safe_text(&self.destination.endpoint)
            )),
            Line::from(format!("Root: {}", safe_text(&self.root))),
            Line::from(format!("Observation: {state}")),
            Line::from("Optional service (v discovers candidates; m enters a unit):"),
        ];
        if let Some(note) = self.notices.first() {
            fixed.push(Line::from(format!("Discovery note: {}", safe_text(note))));
        }
        let mut rows = vec![selected_line(self.cursor == 0, "Do not manage a service")];
        rows.extend(
            self.systemd_units
                .iter()
                .enumerate()
                .map(|(i, unit)| selected_line(self.cursor == i + 1, &safe_text(unit))),
        );
        rows.push(selected_line(
            self.cursor == self.systemd_units.len() + 1,
            if self.custom_service.is_some() {
                "Custom remote commands (c to edit)"
            } else {
                "Configure custom remote commands..."
            },
        ));
        if self.systemd_units.is_empty() {
            rows.push(Line::from(
                "No service candidates loaded. Service management is optional.",
            ));
        }
        Content {
            title: " Target settings · no remote writes ",
            fixed,
            rows,
            selected: Some(self.cursor),
        }
    }
}

struct Content {
    title: &'static str,
    fixed: Vec<Line<'static>>,
    rows: Vec<Line<'static>>,
    selected: Option<usize>,
}

impl Content {
    fn fixed(title: &'static str, fixed: Vec<Line<'static>>) -> Self {
        Self {
            title,
            fixed,
            rows: Vec::new(),
            selected: None,
        }
    }
}

fn directory_content(candidates: &super::RemoteDirectoryCandidates, cursor: usize) -> Content {
    let fixed = vec![
        Line::from(format!("Directory: {}", safe_text(&candidates.directory))),
        Line::from(if candidates.directory == "/" {
            "Read-only listing; / cannot be selected as a deployment root."
        } else {
            "Read-only listing; s selects this directory, not the highlighted child."
        }),
    ];
    let mut rows: Vec<_> = candidates
        .directories
        .iter()
        .enumerate()
        .map(|(i, path)| selected_line(cursor == i, &safe_text(path)))
        .collect();
    if candidates.directories.is_empty() {
        rows.push(Line::from(
            "No child directories. Press g for another path or Esc to return.",
        ));
    }
    Content {
        title: " Browse remote directories ",
        fixed,
        rows,
        selected: Some(cursor),
    }
}

fn text_content(field: TextField, value: &str, offset: usize) -> Content {
    let label = match field {
        TextField::Root => "Component deployment root (absolute, not /)",
        TextField::Service => "Optional unit, e.g. worker.service (empty = none)",
        TextField::Browse => "Existing absolute directory to browse (/ is allowed)",
    };
    Content::fixed(
        " Edit target draft ",
        vec![
            Line::from(label),
            Line::from("No command is executed by typing a path or service."),
            Line::from(format!(
                "Column {} · {} characters (←→/Home/End to view)",
                offset + 1,
                value.chars().count()
            )),
            Line::from(value.chars().skip(offset).collect::<String>()),
        ],
    )
}
