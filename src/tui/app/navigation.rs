//! Session-only navigation and a non-authoritative local connection display cache.

use std::collections::BTreeMap;

use crate::{
    config::{DestinationSettings, ProjectConfig},
    domain::{DestinationKey, EnvironmentId, ProjectId},
    tui::presentation::{context_label, endpoint_label, safe_text},
};

use super::{App, Screen, environment_names, suggest_project_name};

#[derive(Debug, Default)]
pub(super) struct ProjectNavigation {
    // Only the current Project is retained. IDs, not names, survive a rename;
    // switching Project or recreating an Environment cannot borrow old scope.
    selected: Option<(ProjectId, EnvironmentId)>,
    destinations: BTreeMap<DestinationKey, String>,
    overview_scroll: u16,
}

impl App {
    pub(super) fn reset_overview_scroll(&mut self) {
        self.navigation.overview_scroll = 0;
    }

    pub(in crate::tui) fn overview_scroll(&self) -> u16 {
        self.navigation.overview_scroll
    }

    pub(super) fn scroll_overview(&mut self, key: super::KeyCode) {
        use super::KeyCode;
        self.navigation.overview_scroll = match key {
            KeyCode::Up => self.navigation.overview_scroll.saturating_sub(1),
            KeyCode::Down => self.navigation.overview_scroll.saturating_add(1),
            KeyCode::PageUp => self.navigation.overview_scroll.saturating_sub(10),
            KeyCode::PageDown => self.navigation.overview_scroll.saturating_add(10),
            KeyCode::Home => 0,
            _ => self.navigation.overview_scroll,
        };
    }

    pub(super) fn preferred_environment(&self, config: &ProjectConfig) -> Option<String> {
        self.navigation
            .selected
            .as_ref()
            .filter(|(project, _)| project == &config.project_id)
            .and_then(|(_, id)| {
                config
                    .environments
                    .iter()
                    .find(|(_, value)| &value.id == id)
            })
            .or_else(|| config.environments.first_key_value())
            .map(|(name, _)| name.clone())
    }

    pub(super) fn remember_environment(&mut self, config: &ProjectConfig, name: &str) {
        if let Some(environment) = config.environments.get(name) {
            self.navigation.selected = Some((config.project_id.clone(), environment.id.clone()));
        }
    }

    pub(super) fn move_overview_environment(&mut self, config: &ProjectConfig, forward: bool) {
        let current = self.preferred_environment(config);
        let names: Vec<_> = config.environments.keys().collect();
        let index = names
            .iter()
            .position(|name| Some(*name) == current.as_ref())
            .unwrap_or(0);
        let next = if forward {
            index.saturating_add(1).min(names.len().saturating_sub(1))
        } else {
            index.saturating_sub(1)
        };
        if let Some(name) = names.get(next) {
            self.remember_environment(config, name);
            self.reset_overview_scroll();
        }
    }

    pub(super) fn refresh_destination_labels(&mut self) {
        self.navigation.destinations.clear();
        // Listing is read-only and bounded by the existing application service;
        // it neither opens credentials nor contacts a server. A failed refresh
        // discards stale labels rather than presenting a former endpoint as current.
        if let Ok(connections) = self.connection_service().list_connections() {
            for connection in connections {
                let DestinationSettings::LinuxSsh {
                    user, host, port, ..
                } = &connection.current.settings;
                self.navigation.destinations.insert(
                    connection.key.clone(),
                    format!(
                        "{} · revision {} · {}",
                        endpoint_label(user, host, *port),
                        connection.current.revision.get(),
                        connection.key
                    ),
                );
            }
        }
    }

    pub(in crate::tui) fn destination_label(&self, key: &DestinationKey) -> String {
        self.navigation
            .destinations
            .get(key)
            .cloned()
            .unwrap_or_else(|| {
                format!("{key} · connection unavailable / unknown; inspect Connections")
            })
    }

    pub(in crate::tui) fn context_label(&self) -> String {
        match &self.screen {
            Screen::Management(screen) => screen.context_label(),
            Screen::Connections(screen) => self.connections_context_label(screen),
            Screen::ProjectEdit(screen) => self.project_edit_context_label(screen),
            Screen::Projects => "Projects".into(),
            Screen::Browser(_) => "Projects / Choose directory".into(),
            Screen::Overview { config, .. } => self.project_context("Overview", config),
            Screen::DeploySelection(selection) | Screen::DeploymentPlanning { selection, .. } => {
                context_label(
                    if matches!(self.screen, Screen::DeploySelection(_)) {
                        "Deploy / Select Components"
                    } else {
                        "Deploy / Check"
                    },
                    Some(&selection.config.project),
                    environment_names(selection)
                        .get(selection.environment_cursor)
                        .map(String::as_str),
                    None,
                )
            }
            Screen::DeploymentReview { plan, .. } => context_label(
                "Deploy / Confirm",
                Some(&plan.selection.config.project),
                Some(&plan.selection.environment),
                None,
            ),
            Screen::DeploymentRunning { config, .. } => {
                self.project_context("Deploy / Progress", config)
            }
            Screen::DeploymentFinished { config, .. } => {
                self.project_context("Deploy / Result", config)
            }
            Screen::SetupComponents(setup) => context_label(
                "Setup / Components",
                Some(&suggest_project_name(&setup.root)),
                None,
                None,
            ),
            Screen::SetupDestinations(setup)
            | Screen::SetupReview {
                destinations: setup,
                ..
            } => {
                let component = super::selected_components(setup)
                    .get(setup.component_cursor)
                    .map(ToString::to_string);
                context_label(
                    if matches!(self.screen, Screen::SetupReview { .. }) {
                        "Setup / Confirm YAML (local only)"
                    } else {
                        "Setup / Assign connection"
                    },
                    Some(&suggest_project_name(&setup.components.root)),
                    Some("production"),
                    component.as_deref(),
                )
            }
            Screen::RemoteSetupSelection(selection) => context_label(
                "Setup / Service",
                Some(&suggest_project_name(
                    &selection.destinations.components.root,
                )),
                Some("production"),
                Some(selection.component.as_str()),
            ),
            Screen::NewSshDestination(draft)
            | Screen::HostKeyPending { draft, .. }
            | Screen::HostKeyConfirm { draft, .. }
            | Screen::SshAuthenticationPending { draft, .. }
            | Screen::KeyBrowser { draft, .. } => context_label(
                match &self.screen {
                    Screen::HostKeyPending { .. } => "Setup / Capture Host Key",
                    Screen::HostKeyConfirm { .. } => "Setup / Confirm Host Key",
                    Screen::SshAuthenticationPending { .. } => "Setup / Authenticate",
                    Screen::KeyBrowser { .. } => "Setup / Choose identity",
                    _ => "Setup / Connection",
                },
                Some(&suggest_project_name(&draft.destinations.components.root)),
                Some("production"),
                None,
            ),
        }
    }

    fn project_context(&self, page: &str, config: &ProjectConfig) -> String {
        context_label(
            page,
            Some(&config.project),
            self.preferred_environment(config).as_deref(),
            None,
        )
    }

    pub(in crate::tui) fn overview_targets(&self, config: &ProjectConfig) -> String {
        let Some(environment) = self.preferred_environment(config) else {
            return "No Environment is available; edit configuration before deploying.".into();
        };
        let mut text = format!(
            "Environment: {}\n",
            crate::tui::presentation::environment_label(&environment)
        );
        for (name, target) in &config.environments[&environment].components {
            use std::fmt::Write as _;
            let _ = writeln!(
                text,
                "{name} → {}\n  Root: {}",
                self.destination_label(&target.destination),
                safe_text(&target.root)
            );
        }
        text.push_str("\n[d] Select Components and preview deployment\n[m] History / read-only inspection / rollback planning\n[e] Edit local configuration (no deployment)\nConnection labels are local hints, not connectivity or health checks.");
        text
    }
}

#[cfg(test)]
mod tests;
