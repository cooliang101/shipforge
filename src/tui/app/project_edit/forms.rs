use crate::config::{ArtifactSpec, BuildCommand, EnvironmentRename};

use super::{
    App, Arc, BTreeMap, BTreeSet, ComponentName, ComponentSetup, EnvironmentSetup, KeyCode,
    PathBuf, ProjectEditPage, ProjectEditScreen, TargetSetup, move_cursor,
};

const MAX_COMPONENTS: usize = 256;
const MAX_ENVIRONMENTS: usize = 256;
const MAX_TARGETS: usize = 4096;

#[derive(Clone, Debug)]
pub(in crate::tui::app) struct ComponentForm {
    pub original: Option<ComponentName>,
    pub name: String,
    pub working_directory: String,
    pub artifact: String,
    pub commands: Vec<BuildCommand>,
}

impl ComponentForm {
    pub(super) fn blank() -> Self {
        Self {
            original: None,
            name: String::new(),
            working_directory: String::new(),
            artifact: String::new(),
            commands: Vec::new(),
        }
    }

    pub(super) fn new(
        original: Option<ComponentName>,
        name: String,
        setup: &ComponentSetup,
    ) -> Self {
        Self {
            original,
            name,
            working_directory: setup
                .working_directory
                .as_ref()
                .map_or_else(String::new, |path| path.display().to_string()),
            artifact: setup.artifact.path.display().to_string(),
            commands: setup.build.clone(),
        }
    }
}

#[derive(Clone, Debug)]
pub(in crate::tui::app) struct EnvironmentForm {
    pub original: Option<String>,
    pub name: String,
    pub targets: BTreeMap<ComponentName, TargetSetup>,
}

impl EnvironmentForm {
    pub(super) fn blank() -> Self {
        Self {
            original: None,
            name: String::new(),
            targets: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Debug)]
pub(in crate::tui::app) struct TargetForm {
    pub environment: EnvironmentForm,
    pub component: ComponentName,
    pub target: TargetSetup,
}

#[derive(Clone, Copy, Debug)]
pub(in crate::tui::app) enum TextField {
    Project,
    ComponentName,
    WorkingDirectory,
    Artifact,
    EnvironmentName,
    Root,
    Systemd,
    Health,
    Program(usize),
    Argument(usize, usize),
}

impl TextField {
    pub(super) const fn label(self) -> &'static str {
        match self {
            Self::Project => "Project name",
            Self::ComponentName => "New Component name",
            Self::WorkingDirectory => {
                "Working directory (relative to Project; empty = Component-named directory)"
            }
            Self::Artifact => "Artifact path (relative to Component working directory)",
            Self::EnvironmentName => "Environment name (existing identity is preserved on rename)",
            Self::Root => {
                "Remote root (empty = retain existing root; generate default only for a new target)"
            }
            Self::Systemd => "Systemd unit (empty = no service)",
            Self::Health => "Health URL (empty = no HTTP check; never enter credentials)",
            Self::Program(_) => "Executable program (one argv item, not a shell command)",
            Self::Argument(_, _) => {
                "One literal argument (spaces stay inside this single argument)"
            }
        }
    }
}

#[derive(Clone, Debug)]
pub(in crate::tui::app) struct TextEdit {
    pub field: TextField,
    pub value: String,
    pub back: Box<ProjectEditPage>,
    pub cancel: Option<Box<ProjectEditPage>>,
}

pub(super) fn text_page(field: TextField, value: String, back: ProjectEditPage) -> ProjectEditPage {
    ProjectEditPage::Text(TextEdit {
        field,
        value,
        back: Box::new(back),
        cancel: None,
    })
}

impl App {
    pub(super) fn project_form_key(
        &mut self,
        key: KeyCode,
        page: ProjectEditPage,
        screen: &mut ProjectEditScreen,
    ) {
        screen.page = match page {
            ProjectEditPage::Text(mut edit) => {
                match key {
                    KeyCode::Enter => return self.apply_text(edit, screen),
                    KeyCode::Esc => {
                        screen.page = *edit.cancel.unwrap_or(edit.back);
                        return;
                    }
                    KeyCode::Backspace => {
                        edit.value.pop();
                    }
                    KeyCode::Delete => edit.value.clear(),
                    KeyCode::Char(character)
                        if super::render::safe_character(character)
                            && edit.value.len() + character.len_utf8() <= 4096 =>
                    {
                        edit.value.push(character);
                    }
                    _ => {}
                }
                ProjectEditPage::Text(edit)
            }
            ProjectEditPage::Component { form, cursor } => {
                self.component_form_key(key, form, cursor, screen)
            }
            ProjectEditPage::Commands { form, cursor } => command_list_key(key, form, cursor),
            ProjectEditPage::Command {
                form,
                command,
                cursor,
            } => command_key(key, form, command, cursor),
            ProjectEditPage::Environment { form, cursor } => {
                self.environment_form_key(key, form, cursor, screen)
            }
            ProjectEditPage::Target { form, cursor } => target_key(key, form, cursor, screen),
            ProjectEditPage::Destination { form, cursor } => {
                destination_key(key, form, cursor, screen)
            }
            ProjectEditPage::Dependencies {
                form,
                names,
                selected,
                cursor,
            } => dependencies_key(key, form, names, selected, cursor),
            _ => ProjectEditPage::Home,
        };
    }

    fn apply_text(&mut self, edit: TextEdit, screen: &mut ProjectEditScreen) {
        let mut back = *edit.back;
        match (edit.field, &mut back) {
            (TextField::Project, _) => {
                if let Some(draft) = screen.draft.as_mut() {
                    Arc::make_mut(draft).setup.project = edit.value;
                    screen.dirty = true;
                }
            }
            (TextField::ComponentName, ProjectEditPage::Component { form, .. }) => {
                form.name = edit.value;
            }
            (TextField::WorkingDirectory, ProjectEditPage::Component { form, .. }) => {
                form.working_directory = edit.value;
            }
            (TextField::Artifact, ProjectEditPage::Component { form, .. }) => {
                form.artifact = edit.value;
            }
            (TextField::EnvironmentName, ProjectEditPage::Environment { form, .. }) => {
                form.name = edit.value;
            }
            (TextField::Root, ProjectEditPage::Target { form, .. }) => {
                form.target.root = optional(edit.value);
            }
            (TextField::Systemd, ProjectEditPage::Target { form, .. }) => {
                form.target.systemd = optional(edit.value);
            }
            (TextField::Health, ProjectEditPage::Target { form, .. }) => {
                form.target.health = optional(edit.value);
            }
            (TextField::Program(index), ProjectEditPage::Command { form, .. }) => {
                if let Some(command) = form.commands.get_mut(index) {
                    command.program = edit.value;
                    command.shell = false;
                }
            }
            (TextField::Argument(command, index), ProjectEditPage::Command { form, .. }) => {
                if let Some(argument) = form
                    .commands
                    .get_mut(command)
                    .and_then(|command| command.args.get_mut(index))
                {
                    *argument = edit.value;
                }
            }
            _ => self.message = Some("The edited field is no longer available.".into()),
        }
        screen.page = back;
    }

    fn component_form_key(
        &mut self,
        key: KeyCode,
        form: ComponentForm,
        mut cursor: usize,
        screen: &mut ProjectEditScreen,
    ) -> ProjectEditPage {
        move_cursor(key, &mut cursor, 5);
        if key == KeyCode::Esc {
            return ProjectEditPage::Components { cursor: 0 };
        }
        if key != KeyCode::Enter {
            return ProjectEditPage::Component { form, cursor };
        }
        match cursor {
            0 if form.original.is_none() => text_page(
                TextField::ComponentName,
                form.name.clone(),
                ProjectEditPage::Component { form, cursor },
            ),
            0 => {
                self.message = Some("Existing Component keys are retained. Add a new Component explicitly to use another name.".into());
                ProjectEditPage::Component { form, cursor }
            }
            1 => text_page(
                TextField::WorkingDirectory,
                form.working_directory.clone(),
                ProjectEditPage::Component { form, cursor },
            ),
            2 => text_page(
                TextField::Artifact,
                form.artifact.clone(),
                ProjectEditPage::Component { form, cursor },
            ),
            3 => ProjectEditPage::Commands { form, cursor: 0 },
            _ => match commit_component(screen, &form) {
                Ok(()) => ProjectEditPage::Components { cursor: 0 },
                Err(message) => {
                    self.message = Some(message.into());
                    ProjectEditPage::Component { form, cursor }
                }
            },
        }
    }

    fn environment_form_key(
        &mut self,
        key: KeyCode,
        mut form: EnvironmentForm,
        mut cursor: usize,
        screen: &mut ProjectEditScreen,
    ) -> ProjectEditPage {
        let names = screen
            .draft
            .as_ref()
            .map(|draft| draft.setup.components.keys().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        move_cursor(key, &mut cursor, names.len() + 2);
        if key == KeyCode::Esc {
            return ProjectEditPage::Environments { cursor: 0 };
        }
        if key == KeyCode::Enter && cursor == 0 {
            return text_page(
                TextField::EnvironmentName,
                form.name.clone(),
                ProjectEditPage::Environment { form, cursor },
            );
        }
        if key == KeyCode::Enter && cursor == names.len() + 1 {
            return match commit_environment(screen, &form) {
                Ok(()) => ProjectEditPage::Environments { cursor: 0 },
                Err(message) => {
                    self.message = Some(message.into());
                    ProjectEditPage::Environment { form, cursor }
                }
            };
        }
        if let Some(name) = cursor.checked_sub(1).and_then(|index| names.get(index)) {
            if key == KeyCode::Char(' ') && form.targets.remove(name).is_some() {
                for target in form.targets.values_mut() {
                    target.after.retain(|dependency| dependency != name);
                }
            } else if matches!(key, KeyCode::Char(' ') | KeyCode::Enter) {
                if !form.targets.contains_key(name) {
                    let destination = screen
                        .draft
                        .as_ref()
                        .and_then(|draft| draft.destinations().first());
                    let Some(destination) = destination else {
                        self.message = Some("Create a saved connection in connection management before assigning this Component.".into());
                        return ProjectEditPage::Environment { form, cursor };
                    };
                    form.targets.insert(
                        name.clone(),
                        TargetSetup {
                            destination: destination.key.clone(),
                            root: None,
                            systemd: None,
                            health: None,
                            after: Vec::new(),
                        },
                    );
                }
                if key == KeyCode::Enter
                    && let Some(target) = form.targets.get(name).cloned()
                {
                    return ProjectEditPage::Target {
                        form: TargetForm {
                            environment: form,
                            component: name.clone(),
                            target,
                        },
                        cursor: 0,
                    };
                }
            }
        }
        ProjectEditPage::Environment { form, cursor }
    }
}

fn command_list_key(key: KeyCode, mut form: ComponentForm, mut cursor: usize) -> ProjectEditPage {
    move_cursor(key, &mut cursor, form.commands.len());
    match key {
        KeyCode::Esc => ProjectEditPage::Component { form, cursor: 3 },
        KeyCode::Char('a') if form.commands.len() < 64 => {
            let cancel = ProjectEditPage::Commands {
                form: form.clone(),
                cursor,
            };
            let command = form.commands.len();
            form.commands
                .push(BuildCommand::argv("", std::iter::empty::<String>()));
            append_text_page(
                TextField::Program(command),
                String::new(),
                ProjectEditPage::Command {
                    form,
                    command,
                    cursor: 0,
                },
                cancel,
            )
        }
        KeyCode::Char('d') if cursor < form.commands.len() => {
            form.commands.remove(cursor);
            ProjectEditPage::Commands {
                form,
                cursor: cursor.saturating_sub(1),
            }
        }
        KeyCode::Enter if cursor < form.commands.len() => ProjectEditPage::Command {
            form,
            command: cursor,
            cursor: 0,
        },
        _ => ProjectEditPage::Commands { form, cursor },
    }
}

fn command_key(
    key: KeyCode,
    mut form: ComponentForm,
    command: usize,
    mut cursor: usize,
) -> ProjectEditPage {
    let Some(build) = form.commands.get(command) else {
        return ProjectEditPage::Commands { form, cursor: 0 };
    };
    move_cursor(key, &mut cursor, build.args.len() + 1);
    match key {
        KeyCode::Esc => ProjectEditPage::Commands {
            form,
            cursor: command,
        },
        KeyCode::Char('a') if build.args.len() < 128 => {
            let argument = build.args.len();
            let cancel = ProjectEditPage::Command {
                form: form.clone(),
                command,
                cursor,
            };
            form.commands[command].args.push(String::new());
            append_text_page(
                TextField::Argument(command, argument),
                String::new(),
                ProjectEditPage::Command {
                    form,
                    command,
                    cursor: argument + 1,
                },
                cancel,
            )
        }
        KeyCode::Char('d') if cursor > 0 && cursor <= build.args.len() => {
            form.commands[command].args.remove(cursor - 1);
            ProjectEditPage::Command {
                form,
                command,
                cursor: cursor.saturating_sub(1),
            }
        }
        KeyCode::Enter => {
            let (field, value) = if cursor == 0 {
                (TextField::Program(command), build.program.clone())
            } else {
                (
                    TextField::Argument(command, cursor - 1),
                    build.args[cursor - 1].clone(),
                )
            };
            text_page(
                field,
                value,
                ProjectEditPage::Command {
                    form,
                    command,
                    cursor,
                },
            )
        }
        _ => ProjectEditPage::Command {
            form,
            command,
            cursor,
        },
    }
}

fn target_key(
    key: KeyCode,
    form: TargetForm,
    mut cursor: usize,
    screen: &ProjectEditScreen,
) -> ProjectEditPage {
    move_cursor(key, &mut cursor, 6);
    if key == KeyCode::Esc {
        return ProjectEditPage::Environment {
            form: form.environment,
            cursor: 0,
        };
    }
    if key != KeyCode::Enter {
        return ProjectEditPage::Target { form, cursor };
    }
    match cursor {
        0 => {
            let index = screen
                .draft
                .as_ref()
                .and_then(|draft| {
                    draft
                        .destinations()
                        .iter()
                        .position(|destination| destination.key == form.target.destination)
                })
                .unwrap_or(0);
            ProjectEditPage::Destination {
                form,
                cursor: index,
            }
        }
        1 => text_page(
            TextField::Root,
            form.target.root.clone().unwrap_or_default(),
            ProjectEditPage::Target { form, cursor },
        ),
        2 => text_page(
            TextField::Systemd,
            form.target.systemd.clone().unwrap_or_default(),
            ProjectEditPage::Target { form, cursor },
        ),
        3 => text_page(
            TextField::Health,
            form.target.health.clone().unwrap_or_default(),
            ProjectEditPage::Target { form, cursor },
        ),
        4 => {
            let names = form
                .environment
                .targets
                .keys()
                .filter(|name| *name != &form.component)
                .cloned()
                .collect();
            let selected = form.target.after.iter().cloned().collect();
            ProjectEditPage::Dependencies {
                form,
                names,
                selected,
                cursor: 0,
            }
        }
        _ => {
            let mut environment = form.environment;
            environment.targets.insert(form.component, form.target);
            ProjectEditPage::Environment {
                form: environment,
                cursor: 0,
            }
        }
    }
}

fn destination_key(
    key: KeyCode,
    mut form: TargetForm,
    mut cursor: usize,
    screen: &ProjectEditScreen,
) -> ProjectEditPage {
    let choices = screen
        .draft
        .as_ref()
        .map(|draft| draft.destinations())
        .unwrap_or_default();
    move_cursor(key, &mut cursor, choices.len());
    if key == KeyCode::Enter
        && let Some(choice) = choices.get(cursor)
    {
        form.target.destination = choice.key.clone();
        return ProjectEditPage::Target { form, cursor: 0 };
    }
    if key == KeyCode::Esc {
        ProjectEditPage::Target { form, cursor: 0 }
    } else {
        ProjectEditPage::Destination { form, cursor }
    }
}

fn dependencies_key(
    key: KeyCode,
    mut form: TargetForm,
    names: Vec<ComponentName>,
    mut selected: BTreeSet<ComponentName>,
    mut cursor: usize,
) -> ProjectEditPage {
    move_cursor(key, &mut cursor, names.len());
    if key == KeyCode::Char(' ')
        && let Some(name) = names.get(cursor)
        && !selected.remove(name)
    {
        selected.insert(name.clone());
    }
    if key == KeyCode::Enter {
        form.target.after = selected.into_iter().collect();
        return ProjectEditPage::Target { form, cursor: 4 };
    }
    if key == KeyCode::Esc {
        ProjectEditPage::Target { form, cursor: 4 }
    } else {
        ProjectEditPage::Dependencies {
            form,
            names,
            selected,
            cursor,
        }
    }
}

fn commit_component(
    screen: &mut ProjectEditScreen,
    form: &ComponentForm,
) -> Result<(), &'static str> {
    let name = ComponentName::parse(&form.name)
        .map_err(|_| "Use a lowercase kebab-case Component name.")?;
    if form.commands.is_empty()
        || form
            .commands
            .iter()
            .any(|command| command.program.is_empty() || command.shell)
    {
        return Err("Add at least one executable argv command. Shell strings are not supported.");
    }
    if form.artifact.is_empty() {
        return Err("Choose the build-output artifact path.");
    }
    let draft = screen
        .draft
        .as_mut()
        .ok_or("Project draft is unavailable.")?;
    if form.original.is_none() && draft.setup.components.contains_key(&name) {
        return Err("This Component already exists; edit it from the Component list.");
    }
    if !draft.setup.components.contains_key(&name) && draft.setup.components.len() >= MAX_COMPONENTS
    {
        return Err("The draft has reached its limit of 256 Components.");
    }
    if form
        .original
        .as_ref()
        .is_some_and(|original| original != &name)
    {
        return Err("Existing Component keys cannot be silently renamed.");
    }
    Arc::make_mut(draft).setup.components.insert(
        name,
        ComponentSetup {
            working_directory: (!form.working_directory.is_empty())
                .then(|| PathBuf::from(&form.working_directory)),
            build: form.commands.clone(),
            artifact: ArtifactSpec {
                path: PathBuf::from(&form.artifact),
            },
        },
    );
    screen.dirty = true;
    Ok(())
}

fn commit_environment(
    screen: &mut ProjectEditScreen,
    form: &EnvironmentForm,
) -> Result<(), &'static str> {
    ComponentName::parse(&form.name).map_err(|_| "Use a lowercase kebab-case Environment name.")?;
    if form.targets.is_empty() {
        return Err("Select at least one Component for this Environment.");
    }
    let draft = screen
        .draft
        .as_mut()
        .ok_or("Project draft is unavailable.")?;
    if form.original.as_ref() != Some(&form.name)
        && draft.setup.environments.contains_key(&form.name)
    {
        return Err("An Environment with this name already exists.");
    }
    let other_environments = draft
        .setup
        .environments
        .iter()
        .filter(|(name, _)| form.original.as_ref() != Some(*name));
    if other_environments.clone().count() >= MAX_ENVIRONMENTS {
        return Err("The draft has reached its limit of 256 Environments.");
    }
    let targets = other_environments
        .map(|(_, environment)| environment.components.len())
        .sum::<usize>();
    if targets + form.targets.len() > MAX_TARGETS {
        return Err("The draft has reached its limit of 4096 Component targets.");
    }
    let draft = Arc::make_mut(draft);
    if let Some(original) = &form.original {
        draft.setup.environments.remove(original);
        if original != &form.name {
            if let Some(rename) = draft
                .environment_renames
                .iter_mut()
                .find(|rename| &rename.to == original)
            {
                rename.to.clone_from(&form.name);
            } else if draft.original().environments.contains_key(original) {
                draft.environment_renames.push(EnvironmentRename {
                    from: original.clone(),
                    to: form.name.clone(),
                });
            }
            draft
                .environment_renames
                .retain(|rename| rename.from != rename.to);
        }
    }
    draft.setup.environments.insert(
        form.name.clone(),
        EnvironmentSetup {
            components: form.targets.clone(),
        },
    );
    screen.dirty = true;
    Ok(())
}

fn optional(value: String) -> Option<String> {
    if value.is_empty() { None } else { Some(value) }
}

fn append_text_page(
    field: TextField,
    value: String,
    back: ProjectEditPage,
    cancel: ProjectEditPage,
) -> ProjectEditPage {
    ProjectEditPage::Text(TextEdit {
        field,
        value,
        back: Box::new(back),
        cancel: Some(Box::new(cancel)),
    })
}
