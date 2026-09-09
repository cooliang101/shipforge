//! Draft-only service argv editor shared by initial setup and project editing.

use crossterm::event::KeyCode;
use ratatui::{
    Frame,
    layout::Rect,
    text::Line,
    widgets::{Block, Borders, Paragraph},
};

use crate::config::{ServiceAction, ServiceCheck, ServiceConfig};
use crate::tui::{safe_text, selected_line};

const LABELS: [&str; 5] = [
    "First start (required)",
    "Update (empty reuses start)",
    "Restore (empty reuses update)",
    "Stop when undeployed (required)",
    "Read-only health command (optional)",
];

pub(super) enum EditResult {
    Editing,
    Discard,
    Apply(ServiceConfig),
}

#[derive(Clone, Debug)]
enum Page {
    Fields(usize),
    Action {
        stage: usize,
        cursor: usize,
    },
    Arguments {
        stage: usize,
        command: usize,
        cursor: usize,
    },
    Text {
        stage: usize,
        command: usize,
        argument: usize,
        value: String,
        created: bool,
    },
}

#[derive(Clone)]
pub(super) struct ServiceEditor {
    actions: [ServiceAction; 5],
    systemd_check: Option<String>,
    page: Page,
    message: Option<&'static str>,
    text_from_end: usize,
}

impl std::fmt::Debug for ServiceEditor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ServiceEditor([REDACTED draft])")
    }
}

impl ServiceEditor {
    pub(super) fn new(service: Option<ServiceConfig>) -> Self {
        let mut editor = Self {
            actions: std::array::from_fn(|_| Vec::new()),
            systemd_check: None,
            page: Page::Fields(0),
            message: None,
            text_from_end: 0,
        };
        if let Some(service) = service {
            editor.actions[0] = service.start;
            editor.actions[1] = service.update;
            editor.actions[2] = service.restore;
            editor.actions[3] = service.stop;
            match service.check {
                Some(ServiceCheck::Command { argv }) => editor.actions[4].push(argv),
                Some(ServiceCheck::Systemd { unit }) => editor.systemd_check = Some(unit),
                None => {}
            }
        }
        editor
    }

    pub(super) fn is_text(&self) -> bool {
        matches!(self.page, Page::Text { .. })
    }

    pub(super) fn help(&self) -> &'static str {
        match self.page {
            Page::Fields(_) => {
                "↑↓ choose · Enter edit/apply · p password sudo · x clear · Esc discard"
            }
            Page::Action { .. } => "↑↓ command · a add · Enter edit · d remove · Esc fields",
            Page::Arguments { .. } => {
                "↑↓ argument · Enter edit · a append argument · d remove argument · Esc commands"
            }
            Page::Text { .. } => {
                "Enter apply · Esc cancel · ←→/Home/End view · Backspace erase · Delete clear"
            }
        }
    }

    fn config(&self) -> Result<ServiceConfig, &'static str> {
        let check = self.actions[4]
            .first()
            .cloned()
            .map(|argv| ServiceCheck::Command { argv })
            .or_else(|| {
                self.systemd_check
                    .clone()
                    .map(|unit| ServiceCheck::Systemd { unit })
            });
        let service = ServiceConfig {
            start: self.actions[0].clone(),
            update: self.actions[1].clone(),
            restore: self.actions[2].clone(),
            stop: self.actions[3].clone(),
            check,
        };
        service.validate()?;
        Ok(service)
    }

    #[allow(clippy::too_many_lines)] // Explicit, bounded keyboard state transitions.
    pub(super) fn handle(&mut self, key: KeyCode) -> EditResult {
        self.message = None;
        if !self.is_text() {
            self.text_from_end = 0;
        }
        match self.page.clone() {
            Page::Fields(mut cursor) => {
                move_cursor(key, &mut cursor, 6);
                self.page = Page::Fields(cursor);
                match key {
                    KeyCode::Esc => return EditResult::Discard,
                    KeyCode::Char('p') if cursor < 5 => {
                        for argv in &mut self.actions[cursor] {
                            toggle_password_sudo(argv);
                        }
                        self.message = Some(
                            "Password sudo uses the saved SSH login password; no server permissions are changed.",
                        );
                    }
                    KeyCode::Char('x') if cursor < 5 => {
                        self.actions[cursor].clear();
                        if cursor == 4 {
                            self.systemd_check = None;
                        }
                    }
                    KeyCode::Enter if cursor < 5 => {
                        self.page = Page::Action {
                            stage: cursor,
                            cursor: 0,
                        }
                    }
                    KeyCode::Enter => match self.config() {
                        Ok(service) => return EditResult::Apply(service),
                        Err(message) => self.message = Some(message),
                    },
                    _ => {}
                }
            }
            Page::Action { stage, mut cursor } => {
                move_cursor(key, &mut cursor, self.actions[stage].len());
                self.page = Page::Action { stage, cursor };
                match key {
                    KeyCode::Esc => self.page = Page::Fields(stage),
                    KeyCode::Char('a')
                        if self.actions[stage].len() < if stage == 4 { 1 } else { 16 } =>
                    {
                        self.actions[stage].push(vec![String::new()]);
                        self.page = Page::Text {
                            stage,
                            command: self.actions[stage].len() - 1,
                            argument: 0,
                            value: String::new(),
                            created: true,
                        };
                    }
                    KeyCode::Enter if cursor < self.actions[stage].len() => {
                        self.page = Page::Arguments {
                            stage,
                            command: cursor,
                            cursor: 0,
                        }
                    }
                    KeyCode::Char('d') if cursor < self.actions[stage].len() => {
                        self.actions[stage].remove(cursor);
                        self.page = Page::Action {
                            stage,
                            cursor: cursor.saturating_sub(1),
                        };
                    }
                    _ => {}
                }
            }
            Page::Arguments {
                stage,
                command,
                mut cursor,
            } => {
                move_cursor(key, &mut cursor, self.actions[stage][command].len());
                self.page = Page::Arguments {
                    stage,
                    command,
                    cursor,
                };
                match key {
                    KeyCode::Esc => {
                        self.page = Page::Action {
                            stage,
                            cursor: command,
                        }
                    }
                    KeyCode::Enter => {
                        self.page = Page::Text {
                            stage,
                            command,
                            argument: cursor,
                            value: self.actions[stage][command][cursor].clone(),
                            created: false,
                        }
                    }
                    KeyCode::Char('a') if self.actions[stage][command].len() < 128 => {
                        self.actions[stage][command].push(String::new());
                        self.page = Page::Text {
                            stage,
                            command,
                            argument: self.actions[stage][command].len() - 1,
                            value: String::new(),
                            created: true,
                        };
                    }
                    KeyCode::Char('d') if cursor > 0 => {
                        self.actions[stage][command].remove(cursor);
                        self.page = Page::Arguments {
                            stage,
                            command,
                            cursor: cursor - 1,
                        };
                    }
                    _ => {}
                }
            }
            Page::Text {
                stage,
                command,
                argument,
                mut value,
                created,
            } => {
                match key {
                    KeyCode::Enter => {
                        self.actions[stage][command][argument] = value;
                        if stage == 4 {
                            self.systemd_check = None;
                        }
                        self.page = Page::Arguments {
                            stage,
                            command,
                            cursor: argument,
                        };
                        return EditResult::Editing;
                    }
                    KeyCode::Esc => {
                        if created {
                            if argument == 0 {
                                self.actions[stage].remove(command);
                                self.page = Page::Action {
                                    stage,
                                    cursor: command.saturating_sub(1),
                                };
                            } else {
                                self.actions[stage][command].remove(argument);
                                self.page = Page::Arguments {
                                    stage,
                                    command,
                                    cursor: argument - 1,
                                };
                            }
                            return EditResult::Editing;
                        }
                        self.page = Page::Arguments {
                            stage,
                            command,
                            cursor: argument,
                        };
                        return EditResult::Editing;
                    }
                    KeyCode::Backspace => {
                        value.pop();
                        self.text_from_end = 0;
                    }
                    KeyCode::Delete => {
                        value.clear();
                        self.text_from_end = 0;
                    }
                    KeyCode::Left => {
                        self.text_from_end = self
                            .text_from_end
                            .saturating_add(8)
                            .min(value.chars().count());
                    }
                    KeyCode::Right => self.text_from_end = self.text_from_end.saturating_sub(8),
                    KeyCode::Home => self.text_from_end = value.chars().count(),
                    KeyCode::End => self.text_from_end = 0,
                    KeyCode::Char(c)
                        if super::safe_char(c) && value.len() + c.len_utf8() <= 4096 =>
                    {
                        value.push(c);
                        self.text_from_end = 0;
                    }
                    _ => {}
                }
                self.page = Page::Text {
                    stage,
                    command,
                    argument,
                    value,
                    created,
                };
            }
        }
        EditResult::Editing
    }

    pub(super) fn render(&self, frame: &mut Frame<'_>, area: Rect) {
        let mut fixed = vec![
            Line::from("Draft only; commands run in the selected/restored version directory."),
            Line::from(self.message.unwrap_or(
                "No dependency installation. Do not enter secrets or shell command strings.",
            )),
        ];
        let (rows, selected): (Vec<Line<'static>>, usize) = match &self.page {
            Page::Fields(cursor) => {
                let mut rows = LABELS
                    .iter()
                    .enumerate()
                    .map(|(i, label)| {
                        selected_line(
                            *cursor == i,
                            &format!(
                                "{label}: {} command(s){}",
                                self.actions[i].len(),
                                if i == 4 && self.systemd_check.is_some() {
                                    " (systemd stability retained; x clears)"
                                } else {
                                    ""
                                }
                            ),
                        )
                    })
                    .collect::<Vec<_>>();
                rows.push(selected_line(
                    *cursor == 5,
                    "Apply service commands to target draft",
                ));
                (rows, *cursor)
            }
            Page::Action { stage, cursor } => {
                fixed.push(Line::from(LABELS[*stage]));
                let rows = if self.actions[*stage].is_empty() {
                    vec![Line::from("No commands. Press a to add a program.")]
                } else {
                    self.actions[*stage]
                        .iter()
                        .enumerate()
                        .map(|(i, argv)| {
                            selected_line(*cursor == i, &safe_text(&format!("{argv:?}")))
                        })
                        .collect()
                };
                (rows, *cursor)
            }
            Page::Arguments {
                stage,
                command,
                cursor,
            } => (
                self.actions[*stage][*command]
                    .iter()
                    .enumerate()
                    .map(|(i, value)| {
                        selected_line(
                            *cursor == i,
                            &format!(
                                "{}: {}",
                                if i == 0 { "Program" } else { "Argument" },
                                safe_text(value)
                            ),
                        )
                    })
                    .collect(),
                *cursor,
            ),
            Page::Text {
                argument, value, ..
            } => {
                fixed.push(Line::from(if *argument == 0 {
                    "Executable program:"
                } else {
                    "One literal argument (spaces stay in this argument):"
                }));
                let width = usize::from(area.width.saturating_sub(4));
                let start = value
                    .chars()
                    .count()
                    .saturating_sub(width)
                    .saturating_sub(self.text_from_end);
                let visible = value.chars().skip(start).take(width).collect::<String>();
                (vec![Line::from(safe_text(&visible))], 0)
            }
        };
        let capacity = usize::from(area.height.saturating_sub(2));
        fixed.truncate(capacity.saturating_sub(1));
        let available = capacity.saturating_sub(fixed.len());
        let first = selected.saturating_sub(available.saturating_sub(1));
        fixed.extend(rows.into_iter().skip(first).take(available));
        frame.render_widget(
            Paragraph::new(fixed).block(
                Block::default()
                    .title(" Custom remote service commands ")
                    .borders(Borders::ALL),
            ),
            area,
        );
    }
}

fn toggle_password_sudo(argv: &mut Vec<String>) {
    let is_sudo = argv
        .first()
        .is_some_and(|value| matches!(value.as_str(), "sudo" | "/usr/bin/sudo" | "/bin/sudo"));
    if is_sudo
        && argv.get(1).is_some_and(|value| value == "-S")
        && argv.get(2).is_some_and(|value| value == "--")
    {
        argv.drain(..3);
    } else if is_sudo && argv.get(1).is_some_and(|value| value == "-n") {
        argv.splice(1..2, ["-S".into(), "--".into()]);
    } else if !is_sudo && !argv.is_empty() {
        argv.splice(..0, ["/usr/bin/sudo".into(), "-S".into(), "--".into()]);
    }
}

fn move_cursor(key: KeyCode, cursor: &mut usize, count: usize) {
    match key {
        KeyCode::Up => *cursor = cursor.saturating_sub(1),
        KeyCode::Down => *cursor = cursor.saturating_add(1).min(count.saturating_sub(1)),
        KeyCode::Home => *cursor = 0,
        KeyCode::End => *cursor = count.saturating_sub(1),
        _ => {}
    }
}

impl ServiceEditor {
    pub(in crate::tui::app) fn actions_menu(&self) -> Option<crate::tui::app::actions::Actions> {
        use crate::tui::app::actions::Actions;
        use crate::tui::i18n::choose as t;
        Some(match self.page {
            Page::Action { stage, cursor } => Actions::new(
                &[
                    (t("Add command", "添加命令"), 'a'),
                    (t("Remove selected command", "移除选中命令"), 'd'),
                ],
                self.actions[stage].len(),
                cursor,
            ),
            Page::Arguments {
                stage,
                command,
                cursor,
            } => Actions::new(
                &[
                    (t("Add argument", "添加参数"), 'a'),
                    (t("Remove selected argument", "移除选中参数"), 'd'),
                ],
                self.actions[stage][command].len(),
                cursor,
            ),
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_sudo_toggle_preserves_check_and_other_actions() {
        let service = ServiceConfig::systemd("fixture.service");
        let mut editor = ServiceEditor::new(Some(service.clone()));
        editor.handle(KeyCode::Char('p'));
        let updated = editor.config().unwrap();
        assert_eq!(&updated.start[0][..3], &["/usr/bin/sudo", "-S", "--"]);
        assert_eq!(updated.stop, service.stop);
        assert_eq!(updated.check, service.check);
        editor.handle(KeyCode::Char('p'));
        assert_eq!(editor.config().unwrap(), service);
        let mut old = vec![
            "/usr/bin/sudo".into(),
            "-n".into(),
            "systemctl".into(),
            "stop".into(),
        ];
        toggle_password_sudo(&mut old);
        assert_eq!(old, ["/usr/bin/sudo", "-S", "--", "systemctl", "stop"]);
    }

    fn type_value(editor: &mut ServiceEditor, value: &str) {
        for c in value.chars() {
            editor.handle(KeyCode::Char(c));
        }
        editor.handle(KeyCode::Enter);
    }

    fn add_command(editor: &mut ServiceEditor, stage: usize, argv: &[&str]) {
        editor.page = Page::Fields(stage);
        editor.handle(KeyCode::Enter);
        editor.handle(KeyCode::Char('a'));
        type_value(editor, argv[0]);
        for value in &argv[1..] {
            editor.handle(KeyCode::Char('a'));
            type_value(editor, value);
        }
        editor.handle(KeyCode::Esc);
        editor.handle(KeyCode::Esc);
    }

    #[test]
    fn keyboard_builds_literal_argv_defaults_and_optional_check() {
        let mut editor = ServiceEditor::new(None);
        add_command(&mut editor, 0, &["pm2", "start", "ecosystem config.cjs"]);
        add_command(&mut editor, 3, &["pm2", "delete", "api"]);
        add_command(&mut editor, 4, &["node", "health.cjs"]);
        editor.handle(KeyCode::End);
        let EditResult::Apply(service) = editor.handle(KeyCode::Enter) else {
            panic!("valid draft");
        };
        assert_eq!(service.start[0][2], "ecosystem config.cjs");
        assert_eq!(service.restore_commands(), &service.start);
        assert!(matches!(service.check, Some(ServiceCheck::Command { .. })));
        assert!(!format!("{editor:?}").contains("ecosystem"));
    }

    #[test]
    fn cancelled_added_commands_and_arguments_do_not_change_existing_plan() {
        let service = ServiceConfig::systemd("api.service");
        let mut editor = ServiceEditor::new(Some(service.clone()));
        editor.handle(KeyCode::Enter);
        editor.handle(KeyCode::Char('a'));
        editor.handle(KeyCode::Char('x'));
        editor.handle(KeyCode::Esc);
        assert_eq!(editor.config().unwrap(), service);
        editor.handle(KeyCode::Enter);
        editor.handle(KeyCode::Char('a'));
        editor.handle(KeyCode::Esc);
        assert_eq!(editor.config().unwrap(), service);
        editor.page = Page::Fields(0);
        assert!(matches!(editor.handle(KeyCode::Esc), EditResult::Discard));
    }

    #[test]
    fn long_literal_value_can_be_reviewed_from_both_ends_without_editing() {
        let mut editor = ServiceEditor::new(None);
        let value = format!("BEGIN-{}-END", "x".repeat(200));
        add_command(&mut editor, 0, &["node", &value]);
        editor.page = Page::Arguments {
            stage: 0,
            command: 0,
            cursor: 1,
        };
        editor.handle(KeyCode::Enter);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(60, 10)).unwrap();
        editor.handle(KeyCode::Home);
        terminal
            .draw(|frame| editor.render(frame, frame.area()))
            .unwrap();
        let rendered = format!("{:?}", terminal.backend());
        assert!(rendered.contains("BEGIN-"));
        editor.handle(KeyCode::End);
        terminal
            .draw(|frame| editor.render(frame, frame.area()))
            .unwrap();
        assert!(format!("{:?}", terminal.backend()).contains("-END"));
        editor.handle(KeyCode::Esc);
        assert_eq!(editor.actions[0][0][1], value);
    }

    #[test]
    fn missing_stop_is_not_applied_and_tiny_views_do_not_panic() {
        let mut editor = ServiceEditor::new(None);
        add_command(&mut editor, 0, &["pm2", "start", "app.cjs"]);
        editor.handle(KeyCode::End);
        assert!(matches!(editor.handle(KeyCode::Enter), EditResult::Editing));
        assert!(editor.message.is_some());
        for (width, height) in [(80, 10), (20, 3), (0, 0)] {
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| editor.render(frame, frame.area()))
                .unwrap();
        }
    }
}
