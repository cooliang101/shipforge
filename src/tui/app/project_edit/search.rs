//! Search maps to existing rows without applying fields, toggling targets or saving YAML.

use crate::tui::{
    picker::ChoiceSet,
    presentation::{environment_label, safe_text},
};

use super::{ProjectEditPage, ProjectEditScreen};

type SearchRows = (&'static str, Vec<String>, usize, &'static str);

impl ProjectEditScreen {
    pub(in crate::tui) fn search_choices(&self) -> Option<ChoiceSet> {
        let (title, labels, selected, empty_hint) = self
            .collection_search_rows()
            .or_else(|| self.environment_search_rows())
            .or_else(|| self.component_search_rows())?;
        Some(ChoiceSet {
            title,
            items: labels.into_iter().enumerate().collect(),
            selected,
            empty_hint,
        })
    }

    fn collection_search_rows(&self) -> Option<SearchRows> {
        Some(match &self.page {
            ProjectEditPage::Components { cursor } => (
                "Find Component",
                self.draft
                    .as_ref()?
                    .setup
                    .components
                    .keys()
                    .map(ToString::to_string)
                    .collect(),
                *cursor,
                "Esc returns to Components. Press f to discover candidates or a to add one.",
            ),
            ProjectEditPage::Environments { cursor } => (
                "Find Environment",
                self.draft
                    .as_ref()?
                    .setup
                    .environments
                    .keys()
                    .map(|name| environment_label(name))
                    .collect(),
                *cursor,
                "Esc returns to Environments. Press a to add an Environment.",
            ),
            ProjectEditPage::Destination { cursor, .. } => (
                "Find saved connection",
                self.draft
                    .as_ref()?
                    .destinations()
                    .iter()
                    .map(|destination| {
                        format!(
                            "ID {} · r{} · {}",
                            destination.key,
                            destination.revision.get(),
                            safe_text(&destination.endpoint)
                        )
                    })
                    .collect(),
                *cursor,
                "Esc returns to the target. Save or discard the editor, then add a connection from Projects.",
            ),
            ProjectEditPage::Discovery { report, cursor } => (
                "Find discovered Component",
                report
                    .components
                    .iter()
                    .map(|candidate| {
                        format!(
                            "{} · {:?} · {}",
                            candidate.name,
                            candidate.confidence,
                            safe_text(&candidate.source.display().to_string())
                        )
                    })
                    .collect(),
                *cursor,
                "Esc closes search. Esc returns to Components; press a there to add manually.",
            ),
            _ => return None,
        })
    }

    fn environment_search_rows(&self) -> Option<SearchRows> {
        Some(match &self.page {
            ProjectEditPage::Environment { form, cursor } => {
                let mut labels = vec![format!(
                    "Environment name: {}",
                    environment_label(&form.name)
                )];
                labels.extend(self.draft.as_ref()?.setup.components.keys().map(|name| {
                    format!(
                        "[{}] {name}",
                        if form.targets.contains_key(name) {
                            'x'
                        } else {
                            ' '
                        }
                    )
                }));
                labels.push("Apply Environment to draft (not YAML save)".into());
                (
                    "Find Environment field or Component",
                    labels,
                    *cursor,
                    "Esc returns to the Environment form.",
                )
            }
            ProjectEditPage::Dependencies {
                names,
                selected,
                cursor,
                ..
            } => (
                "Find ordering dependency",
                names
                    .iter()
                    .map(|name| {
                        format!(
                            "[{}] after {name}",
                            if selected.contains(name) { 'x' } else { ' ' }
                        )
                    })
                    .collect(),
                *cursor,
                "No other Component is selected. Esc closes search; Enter on the form applies no dependencies.",
            ),
            ProjectEditPage::Target { cursor, .. } => (
                "Find target field",
                [
                    "Connection",
                    "Remote root",
                    "Systemd unit",
                    "Health URL",
                    "Ordering dependencies",
                    "Apply target to Environment form",
                ]
                .into_iter()
                .map(str::to_owned)
                .collect(),
                *cursor,
                "Esc returns to the target form.",
            ),
            _ => return None,
        })
    }

    fn component_search_rows(&self) -> Option<SearchRows> {
        Some(match &self.page {
            ProjectEditPage::Component { cursor, .. } => (
                "Find Component field",
                [
                    "Name",
                    "Working directory",
                    "Artifact",
                    "Build argv commands",
                    "Apply Component to draft (not YAML save)",
                ]
                .into_iter()
                .map(str::to_owned)
                .collect(),
                *cursor,
                "Esc returns to the Component form.",
            ),
            ProjectEditPage::Commands { form, cursor } => (
                "Find build command",
                form.commands
                    .iter()
                    .map(|command| {
                        format!(
                            "{} · {} argument(s)",
                            safe_text(&command.program),
                            command.args.len()
                        )
                    })
                    .collect(),
                *cursor,
                "Esc returns to build commands. Press a to add an executable.",
            ),
            ProjectEditPage::Command {
                form,
                command,
                cursor,
            } => {
                let command = form.commands.get(*command)?;
                let mut labels = vec![format!("Executable: {}", safe_text(&command.program))];
                labels.extend(
                    command
                        .args
                        .iter()
                        .enumerate()
                        .map(|(index, value)| format!("argv[{}]: {}", index + 1, safe_text(value))),
                );
                (
                    "Find executable or argument",
                    labels,
                    *cursor,
                    "Esc returns to the command form.",
                )
            }
            _ => return None,
        })
    }

    pub(in crate::tui) fn select_search_choice(&mut self, index: usize) -> bool {
        if !self
            .search_choices()
            .is_some_and(|choices| choices.items.iter().any(|(original, _)| *original == index))
        {
            return false;
        }
        match &mut self.page {
            ProjectEditPage::Components { cursor }
            | ProjectEditPage::Environments { cursor }
            | ProjectEditPage::Environment { cursor, .. }
            | ProjectEditPage::Destination { cursor, .. }
            | ProjectEditPage::Dependencies { cursor, .. }
            | ProjectEditPage::Discovery { cursor, .. }
            | ProjectEditPage::Component { cursor, .. }
            | ProjectEditPage::Target { cursor, .. }
            | ProjectEditPage::Commands { cursor, .. }
            | ProjectEditPage::Command { cursor, .. } => *cursor = index,
            _ => return false,
        }
        true
    }
}
