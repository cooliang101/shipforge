//! Search only focuses local choices; authentication and saves keep their own keys.

use crate::tui::{
    picker::ChoiceSet,
    presentation::{endpoint_label, safe_text},
};

use super::{ConnectionsPage, ConnectionsScreen, SshField};

impl ConnectionsScreen {
    pub(in crate::tui) fn search_choices(&self) -> Option<ChoiceSet> {
        let (title, labels, selected, empty_hint) = match &self.page {
            ConnectionsPage::List { items, cursor } => (
                "Find saved connection",
                items
                    .iter()
                    .map(|item| {
                        format!(
                            "ID {} · r{} · {}",
                            item.key,
                            item.current.revision.get(),
                            safe_text(&item.current.settings.endpoint_label())
                        )
                    })
                    .collect::<Vec<_>>(),
                *cursor,
                "Esc closes search. Press a to add a connection or f to refresh the list.",
            ),
            ConnectionsPage::Keys {
                directory, cursor, ..
            } => (
                "Find SSH identity file or directory",
                directory
                    .entries
                    .iter()
                    .map(|entry| {
                        format!(
                            "{}{}",
                            safe_text(
                                &entry.path.file_name().unwrap_or_default().to_string_lossy()
                            ),
                            if entry.directory { "/" } else { "" }
                        )
                    })
                    .collect(),
                *cursor,
                "Esc closes search. Backspace opens the parent directory; Esc returns to the form.",
            ),
            ConnectionsPage::Form(form) if form.field == SshField::Host => (
                "Find suggested SSH host (local draft only)",
                form.hosts
                    .iter()
                    .map(|host| {
                        format!(
                            "{} · {}",
                            safe_text(&host.host),
                            endpoint_label(
                                host.user.as_deref().unwrap_or(&form.user),
                                host.hostname.as_deref().unwrap_or(&host.host),
                                host.port.unwrap_or(22),
                            )
                        )
                    })
                    .collect(),
                form.host_cursor,
                "Esc returns to the form. Type a hostname in the Host field; no suggestions were found.",
            ),
            ConnectionsPage::Form(form) if form.field == SshField::Credential => (
                "Find SSH identity (local draft only)",
                form.credentials
                    .iter()
                    .map(|choice| safe_text(choice.label()))
                    .collect(),
                form.credential_cursor,
                "Esc returns to the form. Press F3 to browse a local private-key file.",
            ),
            _ => return None,
        };
        Some(ChoiceSet {
            title,
            items: labels.into_iter().enumerate().collect(),
            selected,
            empty_hint,
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
            ConnectionsPage::List { cursor, .. } | ConnectionsPage::Keys { cursor, .. } => {
                *cursor = index;
            }
            ConnectionsPage::Form(form) => {
                let form = std::sync::Arc::make_mut(form);
                if form.field == SshField::Credential {
                    form.credential_cursor = index;
                } else {
                    form.host_cursor = index;
                    super::next_host(form);
                }
            }
            _ => return false,
        }
        true
    }
}
