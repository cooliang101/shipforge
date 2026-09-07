//! Search only focuses current local candidates; it never forwards Enter.

use crate::tui::picker::{ChoiceSet, Picker, PickerAction};

use super::{
    App, KeyCode, KeyEvent, Screen, SshField, deployment_components, environment_names,
    selected_components,
};

impl App {
    pub(super) fn handle_search_key(&mut self, key: KeyEvent) -> bool {
        if let Some(mut picker) = self.picker.take() {
            match picker.handle_key(key) {
                PickerAction::Cancel => {}
                PickerAction::Continue => self.picker = Some(picker),
                PickerAction::Focus(index) => {
                    if self
                        .search_choices()
                        .is_some_and(|choices| picker.source_matches(&choices))
                    {
                        self.focus_search_choice(index);
                    } else {
                        self.message = Some("Choices changed while searching. Press F4 to reload; the original selection was not changed.".into());
                    }
                }
            }
            return true;
        }
        if key.code != KeyCode::F(4) || !key.modifiers.is_empty() {
            return false;
        }
        if let Some(choices) = self.search_choices() {
            match Picker::new(choices) {
                Ok(picker) => self.picker = Some(picker),
                Err(message) => self.message = Some(message.into()),
            }
        } else {
            self.message =
                Some("No candidate search on this page. Use F1 for available actions.".into());
        }
        true
    }

    fn search_choices(&self) -> Option<ChoiceSet> {
        if self.management_task.is_some()
            || self.connections_task.is_some()
            || self.project_edit_task.is_some()
            || self.reinitialize_task.is_some()
            || self.remote_target_task.is_some()
            || self.setup_task.is_some()
        {
            return None;
        }
        match &self.screen {
            Screen::Connections(screen) => screen.search_choices(),
            Screen::ProjectEdit(screen) => screen.search_choices(),
            Screen::Management(screen) => screen.search_choices(),
            Screen::RemoteSetupSelection(screen) => screen.search_choices(),
            Screen::Projects
            | Screen::Browser(_)
            | Screen::KeyBrowser { .. }
            | Screen::Overview { .. } => self.local_search_choices(),
            Screen::DeploySelection(_) => self.deployment_search_choices(),
            Screen::SetupComponents(_) | Screen::SetupDestinations(_) => {
                self.setup_search_choices()
            }
            Screen::NewSshDestination(_) => self.ssh_search_choices(),
            _ => None,
        }
    }

    fn local_search_choices(&self) -> Option<ChoiceSet> {
        let (title, items, selected, empty_hint) = match &self.screen {
            Screen::Projects => {
                let mut items: Vec<_> = self
                    .recent
                    .iter()
                    .enumerate()
                    .map(|(index, status)| {
                        (
                            index,
                            format!(
                                "{}{}",
                                crate::tui::presentation::path_label(&status.project.root),
                                if status.available {
                                    ""
                                } else {
                                    " [unavailable]"
                                }
                            ),
                        )
                    })
                    .collect();
                items.push((self.recent.len(), "Browse directories".into()));
                (
                    "Projects",
                    items,
                    self.selected_recent,
                    "No saved projects. Esc returns to directory browsing.",
                )
            }
            Screen::Browser(browser) => (
                "project directories",
                browser
                    .children
                    .iter()
                    .enumerate()
                    .map(|(index, path)| (index, crate::tui::presentation::path_label(path)))
                    .collect(),
                browser.selected,
                "No child directories. Esc returns; use Backspace for parent or s to select this root.",
            ),
            Screen::KeyBrowser { browser, .. } => (
                "SSH key files",
                browser
                    .entries
                    .iter()
                    .enumerate()
                    .map(|(index, path)| (index, crate::tui::presentation::path_label(path)))
                    .collect(),
                browser.selected,
                "No entries. Esc returns; use Backspace for the parent directory.",
            ),
            Screen::Overview { config, .. } => {
                let selected = config
                    .environments
                    .keys()
                    .position(|name| Some(name) == self.preferred_environment(config).as_ref())
                    .unwrap_or(0);
                (
                    "Environments",
                    config
                        .environments
                        .keys()
                        .enumerate()
                        .map(|(i, name)| (i, format!("Environment: {name}")))
                        .collect(),
                    selected,
                    "No Environments. Esc returns; edit the project to add one.",
                )
            }
            _ => return None,
        };
        Some(ChoiceSet {
            title,
            items,
            selected,
            empty_hint,
        })
    }

    fn deployment_search_choices(&self) -> Option<ChoiceSet> {
        let (title, items, selected, empty_hint) = match &self.screen {
            Screen::DeploySelection(selection) => {
                let environments = environment_names(selection);
                let mut items: Vec<_> = environments
                    .iter()
                    .enumerate()
                    .map(|(i, name)| (i, format!("Environment: {name}")))
                    .collect();
                items.extend(deployment_components(selection).iter().enumerate().map(
                    |(i, name)| {
                        (
                            environments.len() + i,
                            format!(
                                "Component: {} {name}",
                                if selection.selected.contains(name) {
                                    "[x]"
                                } else {
                                    "[ ]"
                                }
                            ),
                        )
                    },
                ));
                (
                    "Environment / Component",
                    items,
                    environments.len() + selection.component_cursor,
                    "No deployment candidates. Esc returns to project configuration.",
                )
            }
            _ => return None,
        };
        Some(ChoiceSet {
            title,
            items,
            selected,
            empty_hint,
        })
    }

    fn setup_search_choices(&self) -> Option<ChoiceSet> {
        let (title, items, selected, empty_hint) = match &self.screen {
            Screen::SetupComponents(setup) => (
                "Component candidates",
                setup
                    .report
                    .components
                    .iter()
                    .enumerate()
                    .map(|(i, candidate)| {
                        (
                            i,
                            format!(
                                "{} {} · {}",
                                if setup.selected.contains(&candidate.name) {
                                    "[x]"
                                } else {
                                    "[ ]"
                                },
                                candidate.name,
                                crate::tui::presentation::path_label(
                                    &candidate.setup.artifact.path
                                )
                            ),
                        )
                    })
                    .collect(),
                setup.cursor,
                "No Components were discovered. Esc returns; use a to add one manually.",
            ),
            Screen::SetupDestinations(setup) => {
                let components = selected_components(setup);
                let mut items: Vec<_> = components
                    .iter()
                    .enumerate()
                    .map(|(i, name)| (i, format!("Component: {name}")))
                    .collect();
                items.extend(
                    setup
                        .destinations
                        .iter()
                        .enumerate()
                        .map(|(i, destination)| {
                            (
                                components.len() + i,
                                format!(
                                    "Connection: {} · {}",
                                    destination.key, destination.endpoint
                                ),
                            )
                        }),
                );
                (
                    "Component / connection",
                    items,
                    components.len() + setup.destination_cursor,
                    "No candidates. Esc returns; use a to add an SSH connection.",
                )
            }
            _ => return None,
        };
        Some(ChoiceSet {
            title,
            items,
            selected,
            empty_hint,
        })
    }

    fn ssh_search_choices(&self) -> Option<ChoiceSet> {
        let (title, items, selected, empty_hint) = match &self.screen {
            Screen::NewSshDestination(draft) => match draft.field {
                SshField::Host => (
                    "SSH config hosts",
                    draft
                        .connections
                        .iter()
                        .enumerate()
                        .map(|(i, host)| {
                            (
                                i,
                                format!(
                                    "{} · {}",
                                    host.host,
                                    host.hostname.as_deref().unwrap_or(&host.host)
                                ),
                            )
                        })
                        .collect(),
                    draft.connection_cursor,
                    "No simple SSH config hosts were found. Esc returns to enter the host manually.",
                ),
                SshField::Credential => (
                    "SSH identities",
                    draft
                        .credentials
                        .iter()
                        .enumerate()
                        .map(|(i, credential)| (i, credential.label().to_owned()))
                        .collect(),
                    draft.credential_cursor,
                    "No identities are available. Esc returns; use F3 to choose a key file or start SSH Agent.",
                ),
                _ => return None,
            },
            _ => return None,
        };
        Some(ChoiceSet {
            title,
            items,
            selected,
            empty_hint,
        })
    }

    fn focus_search_choice(&mut self, index: usize) {
        match self.screen.clone() {
            Screen::Management(_) => self.focus_management_search_choice(index),
            Screen::Connections(mut screen) => {
                if screen.select_search_choice(index) {
                    self.screen = Screen::Connections(screen);
                }
            }
            Screen::ProjectEdit(mut screen) => {
                if screen.select_search_choice(index) {
                    self.screen = Screen::ProjectEdit(screen);
                }
            }
            Screen::Projects => self.selected_recent = index.min(self.recent.len()),
            Screen::Browser(mut browser) if index < browser.children.len() => {
                browser.selected = index;
                self.screen = Screen::Browser(browser);
            }
            Screen::KeyBrowser { draft, mut browser } if index < browser.entries.len() => {
                browser.selected = index;
                self.screen = Screen::KeyBrowser { draft, browser };
            }
            Screen::Overview { config, .. } => {
                if let Some(name) = config.environments.keys().nth(index) {
                    self.remember_environment(&config, name);
                    self.reset_overview_scroll();
                }
            }
            Screen::DeploySelection(mut selection) => {
                let environments = environment_names(&selection);
                if index < environments.len() {
                    if index != selection.environment_cursor {
                        selection.environment_cursor = index;
                        selection.component_cursor = 0;
                        selection.selected =
                            deployment_components(&selection).into_iter().collect();
                    }
                    self.remember_environment(&selection.config, &environments[index]);
                } else if index - environments.len() < deployment_components(&selection).len() {
                    selection.component_cursor = index - environments.len();
                }
                self.screen = Screen::DeploySelection(selection);
            }
            Screen::SetupComponents(mut setup) if index < setup.report.components.len() => {
                setup.cursor = index;
                self.screen = Screen::SetupComponents(setup);
            }
            Screen::SetupDestinations(mut setup) => {
                let count = selected_components(&setup).len();
                if index < count {
                    setup.component_cursor = index;
                } else if index - count < setup.destinations.len() {
                    setup.destination_cursor = index - count;
                }
                self.screen = Screen::SetupDestinations(setup);
            }
            Screen::NewSshDestination(mut draft) => {
                match draft.field {
                    SshField::Host => draft.apply_connection(index),
                    SshField::Credential if index < draft.credentials.len() => {
                        draft.credential_cursor = index;
                    }
                    _ => {}
                }
                self.screen = Screen::NewSshDestination(draft);
            }
            Screen::RemoteSetupSelection(mut selection) => {
                selection.focus_search_choice(index);
                self.screen = Screen::RemoteSetupSelection(selection);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests;
