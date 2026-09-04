//! Local, selection-first fallback when Component discovery cannot describe a build.

use std::{collections::BTreeMap, fmt::Write as _, path::PathBuf};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    Frame,
    layout::Rect,
    text::Line,
    widgets::{Block, Borders, Paragraph, Wrap},
};

use crate::{
    config::{
        ArtifactSpec, BuildCommand, ComponentSetup, ConfigError, EnvironmentSetup, ProjectSetup,
        TargetSetup, prepare_initialize,
    },
    domain::{ComponentName, DestinationKey},
    projects::{ComponentCandidate, DiscoveryConfidence, suggest_project_name},
    tui::presentation::{context_label, safe_text},
};

use super::ComponentSetupState;

const MAX_COMMANDS: usize = 64;
const MAX_ARGUMENTS: usize = 128;
const MAX_COMPONENTS: usize = 256;
const MAX_FIELD_BYTES: usize = 4096;

#[derive(Clone, Copy, Debug)]
enum Field {
    Name,
    WorkingDirectory,
    Artifact,
    Program(usize),
    Argument(usize, usize),
    NewProgram,
    NewArgument(usize),
}

impl Field {
    const fn label(self) -> &'static str {
        match self {
            Self::Name => "Component name (lowercase kebab-case)",
            Self::WorkingDirectory => {
                "Working directory, relative to the chosen Project (dot = root)"
            }
            Self::Artifact => "Artifact path, relative to the working directory",
            Self::Program(_) | Self::NewProgram => {
                "Executable program (one argv item; no shell string)"
            }
            Self::Argument(_, _) | Self::NewArgument(_) => {
                "One literal argument (spaces remain in this argument; empty is allowed)"
            }
        }
    }
}

#[derive(Clone, Debug)]
enum Page {
    Form {
        cursor: usize,
    },
    Commands {
        cursor: usize,
    },
    Command {
        index: usize,
        cursor: usize,
    },
    Text {
        field: Field,
        value: String,
        back: Box<Self>,
    },
    Preview {
        candidate: ComponentCandidate,
        scroll: u16,
    },
}

#[derive(Clone, Debug)]
pub(in crate::tui) struct ManualComponentScreen {
    origin: ComponentSetupState,
    name: String,
    working_directory: String,
    artifact: String,
    commands: Vec<BuildCommand>,
    page: Page,
    message: Option<&'static str>,
}

impl ManualComponentScreen {
    pub(super) fn new(origin: ComponentSetupState) -> Self {
        Self {
            origin,
            name: String::new(),
            working_directory: ".".into(),
            artifact: String::new(),
            commands: Vec::new(),
            page: Page::Form { cursor: 0 },
            message: None,
        }
    }

    pub(in crate::tui) fn context_label(&self) -> String {
        context_label(
            "Add a manual Component",
            Some(&suggest_project_name(&self.origin.root)),
            None,
            (!self.name.is_empty()).then_some(self.name.as_str()),
        )
    }

    pub(super) const fn requires_plain_confirmation(&self) -> bool {
        matches!(self.page, Page::Preview { .. })
    }

    pub(in crate::tui) const fn help(&self) -> &'static str {
        match &self.page {
            Page::Form { .. } => "Esc cancel  Up/Down choose  Enter edit / preview",
            Page::Commands { .. } if self.commands.is_empty() => "Esc fields  a add command",
            Page::Commands { .. } => {
                "Esc fields  Up/Down choose  Enter edit  a add command  d remove"
            }
            Page::Command { .. } => {
                "Esc commands  Up/Down choose  Enter edit  a add argument  d remove arg"
            }
            Page::Text { .. } => {
                "Esc cancel value  Enter apply value  Backspace erase  Delete clear"
            }
            Page::Preview { .. } => {
                "Esc fields  Up/Down PgUp/PgDn scroll  c add to setup draft only"
            }
        }
    }

    /// Returns the setup draft only when cancelling or confirming the preview.
    /// This editor never reads files, writes configuration, or runs build code.
    pub(super) fn handle_key(&mut self, key: KeyEvent) -> Option<ComponentSetupState> {
        if !key.modifiers.difference(KeyModifiers::SHIFT).is_empty()
            || (!key.modifiers.is_empty() && self.requires_plain_confirmation())
        {
            return None;
        }
        self.message = None;
        let page = self.page.clone();
        match page {
            Page::Form { mut cursor } => {
                move_cursor(key.code, &mut cursor, 5);
                self.page = Page::Form { cursor };
                match key.code {
                    KeyCode::Esc => return Some(self.origin.clone()),
                    KeyCode::Enter => self.open_form_field(cursor),
                    _ => {}
                }
            }
            Page::Commands { mut cursor } => {
                move_cursor(key.code, &mut cursor, self.commands.len());
                self.page = Page::Commands { cursor };
                self.command_list_key(key.code, cursor);
            }
            Page::Command { index, mut cursor } => {
                let count = self
                    .commands
                    .get(index)
                    .map_or(0, |build| build.args.len() + 1);
                move_cursor(key.code, &mut cursor, count);
                self.page = Page::Command { index, cursor };
                self.command_key(key.code, index, cursor);
            }
            Page::Text {
                field,
                mut value,
                back,
            } => match key.code {
                KeyCode::Esc => self.page = *back,
                KeyCode::Enter => self.apply_value(field, value, *back),
                _ => {
                    edit_value(key.code, &mut value);
                    self.page = Page::Text { field, value, back };
                }
            },
            Page::Preview {
                candidate,
                mut scroll,
            } => match key.code {
                KeyCode::Esc => self.page = Page::Form { cursor: 4 },
                KeyCode::Char('c') => return Some(self.add_candidate(candidate)),
                _ => {
                    scroll = match key.code {
                        KeyCode::Up => scroll.saturating_sub(1),
                        KeyCode::Down => scroll.saturating_add(1),
                        KeyCode::PageUp => scroll.saturating_sub(10),
                        KeyCode::PageDown => scroll.saturating_add(10),
                        KeyCode::Home => 0,
                        _ => scroll,
                    };
                    self.page = Page::Preview { candidate, scroll };
                }
            },
        }
        None
    }

    fn open_form_field(&mut self, cursor: usize) {
        match cursor {
            0 => self.open_text(Field::Name, self.name.clone()),
            1 => self.open_text(Field::WorkingDirectory, self.working_directory.clone()),
            2 => self.open_text(Field::Artifact, self.artifact.clone()),
            3 => self.page = Page::Commands { cursor: 0 },
            _ => match self.prepare_candidate() {
                Ok(candidate) => {
                    self.page = Page::Preview {
                        candidate,
                        scroll: 0,
                    }
                }
                Err(message) => self.message = Some(message),
            },
        }
    }

    fn open_text(&mut self, field: Field, value: String) {
        self.page = Page::Text {
            field,
            value,
            back: Box::new(self.page.clone()),
        };
    }

    fn command_list_key(&mut self, key: KeyCode, cursor: usize) {
        match key {
            KeyCode::Esc => self.page = Page::Form { cursor: 3 },
            KeyCode::Char('a') if self.commands.len() < MAX_COMMANDS => {
                self.open_text(Field::NewProgram, String::new());
            }
            KeyCode::Char('a') => self.message = Some("At most 64 build commands may be entered."),
            KeyCode::Char('d') if cursor < self.commands.len() => {
                self.commands.remove(cursor);
                self.page = Page::Commands {
                    cursor: cursor.saturating_sub(1),
                };
            }
            KeyCode::Enter if cursor < self.commands.len() => {
                self.page = Page::Command {
                    index: cursor,
                    cursor: 0,
                };
            }
            _ => {}
        }
    }

    fn command_key(&mut self, key: KeyCode, index: usize, cursor: usize) {
        let Some(build) = self.commands.get(index) else {
            self.page = Page::Commands { cursor: 0 };
            return;
        };
        match key {
            KeyCode::Esc => self.page = Page::Commands { cursor: index },
            KeyCode::Char('a') if build.args.len() < MAX_ARGUMENTS => {
                self.open_text(Field::NewArgument(index), String::new());
            }
            KeyCode::Char('a') => {
                self.message = Some("At most 128 literal arguments may be entered.");
            }
            KeyCode::Char('d') if cursor > 0 && cursor <= build.args.len() => {
                self.commands[index].args.remove(cursor - 1);
                self.page = Page::Command {
                    index,
                    cursor: cursor.saturating_sub(1),
                };
            }
            KeyCode::Enter if cursor == 0 => {
                self.open_text(Field::Program(index), build.program.clone());
            }
            KeyCode::Enter if cursor <= build.args.len() => {
                self.open_text(
                    Field::Argument(index, cursor - 1),
                    build.args[cursor - 1].clone(),
                );
            }
            _ => {}
        }
    }

    fn apply_value(&mut self, field: Field, value: String, back: Page) {
        self.page = back;
        match field {
            Field::Name => self.name = value,
            Field::WorkingDirectory => self.working_directory = value,
            Field::Artifact => self.artifact = value,
            Field::Program(index) => {
                if let Some(build) = self.commands.get_mut(index) {
                    build.program = value;
                }
            }
            Field::Argument(index, argument) => {
                if let Some(value_slot) = self
                    .commands
                    .get_mut(index)
                    .and_then(|build| build.args.get_mut(argument))
                {
                    *value_slot = value;
                }
            }
            Field::NewProgram => {
                let index = self.commands.len();
                self.commands
                    .push(BuildCommand::argv(value, std::iter::empty::<String>()));
                self.page = Page::Command { index, cursor: 0 };
            }
            Field::NewArgument(index) => {
                if let Some(build) = self.commands.get_mut(index) {
                    build.args.push(value);
                    self.page = Page::Command {
                        index,
                        cursor: build.args.len(),
                    };
                }
            }
        }
    }

    fn prepare_candidate(&self) -> Result<ComponentCandidate, &'static str> {
        let name = ComponentName::parse(&self.name)
            .map_err(|_| "Use a lowercase kebab-case Component name.")?;
        if self
            .origin
            .report
            .components
            .iter()
            .any(|candidate| candidate.name == name)
        {
            return Err("This Component name already exists. Choose a different name.");
        }
        if self.origin.report.components.len() >= MAX_COMPONENTS {
            return Err("The setup has reached its limit of 256 Components.");
        }
        if self.commands.is_empty()
            || self
                .commands
                .iter()
                .any(|command| command.program.trim().is_empty() || command.shell)
        {
            return Err(
                "Add at least one executable argv command. Shell strings are not supported.",
            );
        }
        let setup = ComponentSetup {
            working_directory: Some(PathBuf::from(&self.working_directory)),
            artifact: ArtifactSpec {
                path: PathBuf::from(&self.artifact),
            },
            build: self.commands.clone(),
        };
        // Reuse configuration path, argv and sensitive-value validation. This
        // disposable in-memory target is never registered, shown or persisted.
        prepare_initialize(ProjectSetup {
            project: suggest_project_name(&self.origin.root),
            components: BTreeMap::from([(name.clone(), setup.clone())]),
            environments: BTreeMap::from([(
                "validation".into(),
                EnvironmentSetup {
                    components: BTreeMap::from([(
                        name.clone(),
                        TargetSetup {
                            destination: DestinationKey::new(),
                            root: None,
                            systemd: None,
                            health: None,
                            after: Vec::new(),
                        },
                    )]),
                },
            )]),
        })
        .map_err(|error| match error {
            ConfigError::UnsafePath {
                field: "working directory",
                ..
            } => "Working directory must be a nonempty relative path inside the chosen Project.",
            ConfigError::UnsafePath { .. } => {
                "Artifact must be a nonempty relative path inside the working directory."
            }
            ConfigError::EmptyBuild(_) => "Add a nonempty executable program for every command.",
            _ => {
                "Component validation failed. Check the paths and argv values; never enter secrets."
            }
        })?;
        Ok(ComponentCandidate {
            name,
            setup,
            source: self.origin.root.clone(),
            confidence: DiscoveryConfidence::Low,
        })
    }

    fn add_candidate(&self, candidate: ComponentCandidate) -> ComponentSetupState {
        let mut setup = self.origin.clone();
        setup.selected.insert(candidate.name.clone());
        setup.report.notices.push(format!(
            "{}: manually configured; build output has not been verified",
            candidate.name
        ));
        setup.report.components.push(candidate);
        setup.cursor = setup.report.components.len() - 1;
        setup
    }

    pub(in crate::tui) fn render(&self, frame: &mut Frame<'_>, area: Rect) {
        let area = if let Some(message) = self.message {
            let height = area.height.min(2);
            frame.render_widget(
                Paragraph::new(format!("Error: {message}")).wrap(Wrap { trim: false }),
                Rect::new(area.x, area.y, area.width, height),
            );
            Rect::new(
                area.x,
                area.y.saturating_add(height),
                area.width,
                area.height.saturating_sub(height),
            )
        } else {
            area
        };
        let (title, lines, cursor, scroll, wrap) = self.content(area.width);
        let scroll = cursor.map_or(scroll, |row| {
            let visible = usize::from(area.height.saturating_sub(2)).max(1);
            u16::try_from(row.saturating_sub(visible.saturating_sub(1))).unwrap_or(u16::MAX)
        });
        let paragraph = Paragraph::new(lines)
            .block(Block::default().title(title).borders(Borders::ALL))
            .scroll((scroll, 0));
        frame.render_widget(
            if wrap {
                paragraph.wrap(Wrap { trim: false })
            } else {
                paragraph
            },
            area,
        );
    }

    fn content(&self, width: u16) -> (&'static str, Vec<Line<'static>>, Option<usize>, u16, bool) {
        match &self.page {
            Page::Form { cursor } => (
                " Manual Component fields ",
                self.form_lines(*cursor),
                Some(cursor + 3),
                0,
                false,
            ),
            Page::Commands { cursor } => (
                " Build commands (argv only) ",
                self.command_lines(*cursor),
                Some(cursor + 2),
                0,
                false,
            ),
            Page::Command { index, cursor } => {
                let mut rows = Vec::new();
                if let Some(build) = self.commands.get(*index) {
                    rows.push(format!("Program: {}", safe_text(&build.program)));
                    rows.extend(build.args.iter().enumerate().map(|(index, argument)| {
                        format!("Argument {}: {:?}", index + 1, safe_text(argument))
                    }));
                }
                (
                    " Edit one command · literal argv ",
                    mark_rows(rows, *cursor),
                    Some(*cursor),
                    0,
                    false,
                )
            }
            Page::Text { field, value, .. } => (
                " Enter one value ",
                text_lines(*field, value, width),
                None,
                0,
                true,
            ),
            Page::Preview { candidate, scroll } => (
                " Confirm manual Component ",
                self.preview_lines(candidate),
                None,
                *scroll,
                true,
            ),
        }
    }

    fn form_lines(&self, cursor: usize) -> Vec<Line<'static>> {
        let rows = vec![
            format!("Name: {}", safe_text(&self.name)),
            format!("Working directory: {}", safe_text(&self.working_directory)),
            format!("Artifact: {}", safe_text(&self.artifact)),
            format!("Build commands: {}", self.commands.len()),
            "Validate and preview Component".into(),
        ];
        let mut lines = vec![
            Line::from(format!(
                "Project directory: {}",
                safe_text(&self.origin.root.display().to_string())
            )),
            Line::from("Manual fallback. No files are written and no commands are run."),
            Line::from(""),
        ];
        lines.extend(mark_rows(rows, cursor));
        lines
    }

    fn command_lines(&self, cursor: usize) -> Vec<Line<'static>> {
        let mut lines = vec![
            Line::from("Commands run in the chosen working directory, in the listed order."),
            Line::from(""),
        ];
        if self.commands.is_empty() {
            lines.push(Line::from(
                "No commands yet. Press a to enter an executable program.",
            ));
        }
        lines.extend(mark_rows(
            self.commands
                .iter()
                .enumerate()
                .map(|(index, command)| {
                    format!(
                        "{}. {} ({} literal arguments)",
                        index + 1,
                        safe_text(&command.program),
                        command.args.len()
                    )
                })
                .collect(),
            cursor,
        ));
        lines
    }

    fn preview_lines(&self, candidate: &ComponentCandidate) -> Vec<Line<'static>> {
        let mut text = format!(
            "ADD COMPONENT TO SETUP DRAFT ONLY\nProject directory: {}\nName: {}\nWorking directory: {}\nArtifact: {}\n\n",
            safe_text(&self.origin.root.display().to_string()),
            candidate.name,
            safe_text(&self.working_directory),
            safe_text(&self.artifact)
        );
        for (index, command) in candidate.setup.build.iter().enumerate() {
            let _ = writeln!(
                text,
                "Command {} program: {:?}",
                index + 1,
                safe_text(&command.program)
            );
            for (index, argument) in command.args.iter().enumerate() {
                let _ = writeln!(text, "  Argument {}: {:?}", index + 1, safe_text(argument));
            }
        }
        text.push_str("\nNo build ran; paths/output will be checked during deployment.\nOnly plain c adds this candidate. A separate full YAML preview and confirmation is still required before saving.");
        text.lines()
            .map(|line| Line::from(line.to_owned()))
            .collect()
    }
}

fn text_lines(field: Field, value: &str, width: u16) -> Vec<Line<'static>> {
    let value = safe_text(value);
    let visible = usize::from(width.saturating_sub(6)).max(1);
    let skipped = value.chars().count().saturating_sub(visible);
    let tail: String = value.chars().skip(skipped).collect();
    let prefix = if skipped == 0 { "> " } else { "> ..." };
    vec![
        Line::from(field.label()),
        Line::from(""),
        Line::from(format!("{prefix}{tail}")),
        Line::from(""),
        Line::from("Only the value tail is shown when it is long. Delete clears it."),
        Line::from("No IDs, secrets, Shell strings or JSON argv are needed."),
    ]
}

fn edit_value(key: KeyCode, value: &mut String) {
    match key {
        KeyCode::Backspace => {
            value.pop();
        }
        KeyCode::Delete => value.clear(),
        KeyCode::Char(character)
            if !character.is_control() && value.len() + character.len_utf8() <= MAX_FIELD_BYTES =>
        {
            let text = character.to_string();
            if safe_text(&text) == text {
                value.push(character);
            }
        }
        _ => {}
    }
}

fn move_cursor(key: KeyCode, cursor: &mut usize, count: usize) {
    *cursor = match key {
        KeyCode::Up => cursor.saturating_sub(1),
        KeyCode::Down => cursor.saturating_add(1).min(count.saturating_sub(1)),
        KeyCode::Home => 0,
        KeyCode::End => count.saturating_sub(1),
        _ => *cursor,
    };
}

fn mark_rows(rows: Vec<String>, cursor: usize) -> Vec<Line<'static>> {
    rows.into_iter()
        .enumerate()
        .map(|(index, row)| {
            Line::from(format!("{} {row}", if index == cursor { ">" } else { " " }))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use ratatui::{Terminal, backend::TestBackend};

    use crate::projects::DiscoveryReport;

    use super::*;

    fn empty_setup(root: PathBuf) -> ComponentSetupState {
        ComponentSetupState {
            root,
            report: DiscoveryReport::default(),
            selected: BTreeSet::new(),
            cursor: 0,
        }
    }

    fn press(screen: &mut ManualComponentScreen, code: KeyCode) -> Option<ComponentSetupState> {
        screen.handle_key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn type_value(screen: &mut ManualComponentScreen, value: &str) {
        press(screen, KeyCode::Delete);
        for character in value.chars() {
            press(screen, KeyCode::Char(character));
        }
        press(screen, KeyCode::Enter);
    }

    fn fill_valid_form(screen: &mut ManualComponentScreen) {
        press(screen, KeyCode::Enter);
        type_value(screen, "worker");
        press(screen, KeyCode::Down);
        press(screen, KeyCode::Down);
        press(screen, KeyCode::Enter);
        type_value(screen, "dist/worker");
        press(screen, KeyCode::Down);
        press(screen, KeyCode::Enter);
        press(screen, KeyCode::Char('a'));
        type_value(screen, "cargo");
        press(screen, KeyCode::Char('a'));
        type_value(screen, "build");
        press(screen, KeyCode::Char('a'));
        type_value(screen, "a literal argument with spaces");
        press(screen, KeyCode::Esc);
        press(screen, KeyCode::Esc);
        press(screen, KeyCode::Down);
    }

    #[test]
    fn keyboard_manual_component_requires_preview_and_plain_confirmation_without_writes() {
        let directory = tempfile::tempdir().unwrap();
        let mut screen = ManualComponentScreen::new(empty_setup(directory.path().to_owned()));
        fill_valid_form(&mut screen);
        assert!(press(&mut screen, KeyCode::Enter).is_none());
        assert!(matches!(screen.page, Page::Preview { .. }));
        assert!(screen.origin.report.components.is_empty());
        assert!(screen.requires_plain_confirmation());
        for modifiers in [
            KeyModifiers::CONTROL,
            KeyModifiers::ALT,
            KeyModifiers::SHIFT,
        ] {
            assert!(
                screen
                    .handle_key(KeyEvent::new(KeyCode::Char('c'), modifiers))
                    .is_none()
            );
        }
        assert!(press(&mut screen, KeyCode::Enter).is_none());
        let result = press(&mut screen, KeyCode::Char('c')).unwrap();
        let candidate = &result.report.components[0];
        assert_eq!(candidate.name.as_str(), "worker");
        assert_eq!(candidate.source, directory.path());
        assert_eq!(candidate.setup.working_directory, Some(PathBuf::from(".")));
        assert_eq!(
            candidate.setup.build[0],
            BuildCommand::argv("cargo", ["build", "a literal argument with spaces"])
        );
        assert_eq!(candidate.setup.artifact.path, PathBuf::from("dist/worker"));
        assert!(result.selected.contains(&candidate.name));
        assert_eq!(result.cursor, 0);
        assert!(result.report.notices[0].contains("manually configured"));
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
    }

    #[test]
    fn invalid_component_is_recoverable_and_does_not_echo_inputs() {
        let mut screen = ManualComponentScreen::new(empty_setup(PathBuf::from("chosen-project")));
        fill_valid_form(&mut screen);
        screen.working_directory = "../outside".into();
        press(&mut screen, KeyCode::Enter);
        assert!(matches!(screen.page, Page::Form { .. }));
        assert!(screen.message.unwrap().contains("Working directory"));
        screen.working_directory = ".".into();
        screen.artifact = "../secret-sentinel".into();
        press(&mut screen, KeyCode::Enter);
        assert!(screen.message.unwrap().contains("Artifact"));
        assert!(!screen.message.unwrap().contains("secret-sentinel"));
        screen.artifact = "dist".into();
        screen.commands[0].program.clear();
        press(&mut screen, KeyCode::Enter);
        assert!(screen.message.unwrap().contains("argv"));
        screen.commands[0].program = "cargo".into();
        press(&mut screen, KeyCode::Enter);
        assert!(matches!(screen.page, Page::Preview { .. }));
    }

    #[test]
    fn invalid_component_keeps_the_recovery_message_visible_above_scrolled_fields() {
        let mut screen = ManualComponentScreen::new(empty_setup(PathBuf::from("chosen-project")));
        fill_valid_form(&mut screen);
        screen.working_directory = "../outside".into();
        press(&mut screen, KeyCode::Enter);
        let mut terminal = Terminal::new(TestBackend::new(80, 10)).unwrap();
        terminal
            .draw(|frame| screen.render(frame, frame.area()))
            .unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        assert!(text.contains("Error: Working directory"));
        assert!(text.contains("> Validate and preview Component"));
    }

    #[test]
    fn cancel_value_new_command_argument_and_component_preserves_origin() {
        let mut screen = ManualComponentScreen::new(empty_setup(PathBuf::from("chosen-project")));
        fill_valid_form(&mut screen);
        press(&mut screen, KeyCode::Up);
        press(&mut screen, KeyCode::Enter);
        press(&mut screen, KeyCode::Char('a'));
        press(&mut screen, KeyCode::Char('x'));
        press(&mut screen, KeyCode::Esc);
        assert_eq!(screen.commands.len(), 1);
        press(&mut screen, KeyCode::Enter);
        press(&mut screen, KeyCode::Char('a'));
        press(&mut screen, KeyCode::Char('x'));
        press(&mut screen, KeyCode::Esc);
        assert_eq!(screen.commands[0].args.len(), 2);
        press(&mut screen, KeyCode::Esc);
        press(&mut screen, KeyCode::Esc);
        let result = press(&mut screen, KeyCode::Esc).unwrap();
        assert!(result.report.components.is_empty());
        assert!(result.selected.is_empty());
    }

    #[test]
    fn duplicate_component_and_shell_command_cannot_be_added() {
        let mut screen = ManualComponentScreen::new(empty_setup(PathBuf::from("chosen-project")));
        fill_valid_form(&mut screen);
        screen
            .origin
            .report
            .components
            .push(screen.prepare_candidate().unwrap());
        press(&mut screen, KeyCode::Enter);
        assert!(screen.message.unwrap().contains("already exists"));
        screen.origin.report.components.clear();
        screen.commands[0].shell = true;
        press(&mut screen, KeyCode::Enter);
        assert!(screen.message.unwrap().contains("Shell strings"));
        assert!(screen.origin.report.components.is_empty());
    }

    #[test]
    fn many_arguments_keep_selected_row_visible_without_color() {
        let mut screen = ManualComponentScreen::new(empty_setup(PathBuf::from("chosen-project")));
        screen.commands.push(BuildCommand::argv(
            "cargo",
            (0..100).map(|i| format!("value-{i}")),
        ));
        screen.page = Page::Command {
            index: 0,
            cursor: 0,
        };
        press(&mut screen, KeyCode::End);
        let mut terminal = Terminal::new(TestBackend::new(80, 10)).unwrap();
        terminal
            .draw(|frame| screen.render(frame, frame.area()))
            .unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        assert!(text.contains("> Argument 100: \"value-99\""));
        assert!(screen.help().contains("Esc"));
    }

    #[test]
    fn manual_text_is_bounded_and_rejects_control_or_directional_characters() {
        let mut screen = ManualComponentScreen::new(empty_setup(PathBuf::from("chosen-project")));
        press(&mut screen, KeyCode::Enter);
        for character in ['\u{1b}', '\u{202e}', '\u{200b}', '\n'] {
            press(&mut screen, KeyCode::Char(character));
        }
        for _ in 0..5000 {
            press(&mut screen, KeyCode::Char('x'));
        }
        let Page::Text { value, .. } = &screen.page else {
            panic!("expected text field");
        };
        assert_eq!(value.len(), MAX_FIELD_BYTES);
        assert!(value.chars().all(|character| character == 'x'));
        press(&mut screen, KeyCode::Esc);
        assert!(screen.name.is_empty());
    }

    #[test]
    fn empty_command_list_help_only_offers_available_actions() {
        let mut screen = ManualComponentScreen::new(empty_setup(PathBuf::from("chosen-project")));
        screen.page = Page::Commands { cursor: 0 };
        assert_eq!(screen.help(), "Esc fields  a add command");
        press(&mut screen, KeyCode::Enter);
        assert!(matches!(screen.page, Page::Commands { .. }));
        press(&mut screen, KeyCode::Char('d'));
        assert!(screen.commands.is_empty());
        press(&mut screen, KeyCode::Char('a'));
        type_value(&mut screen, "cargo");
        press(&mut screen, KeyCode::Esc);
        assert!(screen.help().contains("Enter edit"));
    }
}
