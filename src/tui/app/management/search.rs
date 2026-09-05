use crate::tui::picker::ChoiceSet;

use super::{App, Arc, ManagementPage, ManagementScreen, Screen};

#[cfg(test)]
mod tests;

impl ManagementScreen {
    pub(in crate::tui) fn search_choices(&self) -> Option<ChoiceSet> {
        let (title, items, selected) = match &self.page {
            ManagementPage::Home if self.scope.historical_environment.is_none() => (
                "Environments",
                self.scope
                    .config
                    .environments
                    .keys()
                    .enumerate()
                    .map(|(i, name)| (i, name.clone()))
                    .collect(),
                self.scope
                    .config
                    .environments
                    .keys()
                    .position(|name| name == &self.scope.environment)
                    .unwrap_or(0),
            ),
            ManagementPage::Environments { page, cursor, .. } => (
                "historical Environment IDs (current page only)",
                page.items
                    .iter()
                    .enumerate()
                    .map(|(i, id)| (i, id.to_string()))
                    .collect(),
                *cursor,
            ),
            ManagementPage::InspectSelection { names, cursor, .. } => (
                "Components to inspect",
                names
                    .iter()
                    .enumerate()
                    .map(|(i, name)| (i, name.to_string()))
                    .collect(),
                *cursor,
            ),
            ManagementPage::RollbackSelection {
                details, cursor, ..
            } => (
                "Components to roll back",
                details
                    .snapshots
                    .iter()
                    .enumerate()
                    .map(|(i, snapshot)| (i, snapshot.release.component.to_string()))
                    .collect(),
                *cursor,
            ),
            ManagementPage::RollbackTargets {
                candidates, cursor, ..
            } => (
                "rollback Components",
                candidates
                    .components
                    .iter()
                    .enumerate()
                    .map(|(i, component)| (i, component.component.to_string()))
                    .collect(),
                *cursor,
            ),
            _ => return None,
        };
        Some(ChoiceSet {
            title,
            items,
            selected,
            empty_hint: "No candidates on this page. Esc returns; use the page's navigation or configuration actions.",
        })
    }
}

impl App {
    pub(in crate::tui::app) fn focus_management_search_choice(&mut self, index: usize) {
        let Screen::Management(mut screen) = self.screen.clone() else {
            return;
        };
        if !screen
            .search_choices()
            .is_some_and(|choices| choices.items.iter().any(|(i, _)| *i == index))
        {
            return;
        }
        match &mut screen.page {
            ManagementPage::Home => {
                if let Some(name) = screen.scope.config.environments.keys().nth(index).cloned() {
                    Arc::make_mut(&mut screen.scope).environment = name;
                    self.remember_environment(&screen.scope.config, &screen.scope.environment);
                }
            }
            ManagementPage::Environments { cursor, .. }
            | ManagementPage::InspectSelection { cursor, .. }
            | ManagementPage::RollbackSelection { cursor, .. }
            | ManagementPage::RollbackTargets { cursor, .. } => *cursor = index,
            _ => return,
        }
        screen.view = super::viewport::Viewport::default();
        self.screen = Screen::Management(screen);
    }
}
