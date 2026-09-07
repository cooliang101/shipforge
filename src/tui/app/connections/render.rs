use std::fmt::Write as _;

use ratatui::{
    Frame,
    layout::Rect,
    widgets::{Block, Borders, Paragraph, Wrap},
};

use crate::{
    config::DestinationSettings,
    tui::presentation::{context_label, endpoint_label},
};

use super::{ConnectionDetails, ConnectionForm, ConnectionsPage, ConnectionsScreen, SshField};

pub(super) use crate::tui::presentation::safe_text;

impl super::App {
    pub(in crate::tui) fn connections_context_label(&self, screen: &ConnectionsScreen) -> String {
        self.connections_task.as_ref().map_or_else(
            || screen.context_label(),
            |task| format!("{} · Working", task.origin.context_label()),
        )
    }
}

impl ConnectionsScreen {
    pub(in crate::tui) fn context_label(&self) -> String {
        let label = match &self.page {
            ConnectionsPage::Unavailable => "Connections / Unavailable saved connections".into(),
            ConnectionsPage::List { items, cursor } => items.get(*cursor).map_or_else(
                || "Connections / Saved connections".into(),
                |connection| connection_context("Connections", connection),
            ),
            ConnectionsPage::Detail { connection, .. } => {
                connection_context("Connections / Details", connection)
            }
            ConnectionsPage::Form(form) => form_context("Edit connection", form),
            ConnectionsPage::Keys { form, .. } => form_context("Browse SSH identities", form),
            ConnectionsPage::HostKey { confirmation, .. } => {
                let preview = confirmation.preview();
                let draft = preview.draft();
                format!(
                    "Connections / Confirm Host Key / ID {} / {}",
                    preview.key(),
                    endpoint_label(&draft.user, &draft.host, draft.port)
                )
            }
            ConnectionsPage::Remove(preview) => {
                connection_context("Connections / Remove registration", preview.details())
            }
            ConnectionsPage::ProjectRemove(_) => "Projects / Remove recent registration".into(),
            ConnectionsPage::ProjectRemoved(_) => "Projects / Registration removed".into(),
            ConnectionsPage::Loading { label, .. } => format!("Connections / {label}"),
        };
        context_label(&label, None, None, None)
    }

    pub(in crate::tui) fn help(&self) -> &'static str {
        match &self.page {
            ConnectionsPage::Unavailable => "Esc projects   f retry reading saved connections",
            ConnectionsPage::List { items, .. } if items.is_empty() => {
                "Esc projects   a add SSH connection   f refresh"
            }
            ConnectionsPage::List { .. } => {
                "Esc projects   F4 search   ↑/↓ select   Enter details   a add   f refresh"
            }
            ConnectionsPage::Detail { .. } => {
                "Esc list   e edit   v verify (read-only)   x remove registration"
            }
            ConnectionsPage::Form(form) if form.field == SshField::Host => {
                "Esc list   Tab field   F4 hosts   F2 next host   F3 keys   Enter capture"
            }
            ConnectionsPage::Form(form) if form.field == SshField::Credential => {
                "Esc list   Tab field   ↑/↓ identity   F4 search   F3 keys   Enter capture"
            }
            ConnectionsPage::Form(_) => {
                "Esc list   Tab field   type edit   F2 next host   F3 keys   Enter capture"
            }
            ConnectionsPage::Keys { directory, .. } if directory.entries.is_empty() => {
                "Esc form   Backspace parent"
            }
            ConnectionsPage::Keys { .. } => {
                "Esc form   F4 search   ↑/↓ select   Enter dir   Backspace parent   s key"
            }
            ConnectionsPage::HostKey { .. } => {
                "Esc / n reject   y trust fingerprint, authenticate and save"
            }
            ConnectionsPage::Remove(preview) if !preview.can_remove() => {
                "Esc return   Removal blocked   PgUp/PgDn scroll"
            }
            ConnectionsPage::Remove(_) | ConnectionsPage::ProjectRemove(_) => {
                "Esc reject   PgUp/PgDn scroll   c confirm removal"
            }
            ConnectionsPage::Loading {
                cancelling: true, ..
            } => "CANCELLING   Waiting for a definite result before leaving",
            ConnectionsPage::Loading { .. } => {
                "Esc / Ctrl+C cancel   Wait for completion before leaving (no detach)"
            }
            ConnectionsPage::ProjectRemoved(_) => {
                "Project unregistered; files and history unchanged"
            }
        }
    }

    pub(in crate::tui) fn render(&self, frame: &mut Frame<'_>, area: Rect) {
        let (title, body, cursor) = self.content();
        let scroll = cursor.map_or(self.scroll, |cursor| {
            let visible = usize::from(area.height.saturating_sub(4)).max(1);
            u16::try_from(cursor.saturating_sub(visible.saturating_sub(1))).unwrap_or(u16::MAX)
        });
        let mut paragraph = Paragraph::new(body)
            .block(Block::default().title(title).borders(Borders::ALL))
            .scroll((scroll, 0));
        // Cursor offsets are logical rows. Wrapping a long hostname/path would
        // otherwise push the selected entry outside a small terminal's viewport.
        if cursor.is_none() {
            paragraph = paragraph.wrap(Wrap { trim: false });
        }
        frame.render_widget(paragraph, area);
    }

    fn content(&self) -> (&'static str, String, Option<usize>) {
        match &self.page {
            ConnectionsPage::Unavailable => (
                " Saved connections unavailable ",
                "UNKNOWN: saved connections could not be loaded.\nThis does not mean no connections exist.\n\nPress f to retry reading local settings, or Esc to return to Projects.".into(),
                None,
            ),
            ConnectionsPage::List { items, cursor } => {
                let mut body =
                    "Standalone saved connections · no Project is required\n\n".to_owned();
                if items.is_empty() {
                    body.push_str("No saved connections.\nPress a to add one, f to refresh, or Esc to return.\n");
                }
                for (index, connection) in items.iter().enumerate() {
                    let DestinationSettings::LinuxSsh {
                        host, port, user, ..
                    } = &connection.current.settings;
                    let _ = writeln!(
                        body,
                        "{} ID {} · r{} · {}",
                        mark(index == *cursor),
                        connection.key,
                        connection.current.revision.get(),
                        endpoint_label(user, host, *port)
                    );
                }
                (" Connections ", body, Some(cursor + 2))
            }
            ConnectionsPage::Detail { connection, notice } => {
                let mut body = connection_text(connection);
                body.push_str("\nEditing preserves this connection ID and retains earlier revisions.\nRemoving requires every local reference source to be readable and unused.\nNo action here deletes remote files or services.\n");
                if let Some(notice) = notice {
                    let _ = write!(body, "\n{}", safe_text(notice));
                }
                (" Connection details ", body, None)
            }
            ConnectionsPage::Form(form) => (" Connection setup ", form_text(form), None),
            ConnectionsPage::Keys {
                directory, cursor, ..
            } => {
                let mut body = format!(
                    "Directory: {}\nSelect a private key; file contents are never displayed.\n\n",
                    safe_text(&directory.path.display().to_string())
                );
                if directory.entries.is_empty() {
                    body.push_str("No visible files or directories here.\nBackspace opens the parent directory; Esc returns to the form.\n");
                }
                for (index, entry) in directory.entries.iter().enumerate() {
                    let _ = writeln!(
                        body,
                        "{} {}{}",
                        mark(index == *cursor),
                        safe_text(&entry.path.file_name().unwrap_or_default().to_string_lossy()),
                        if entry.directory { "/" } else { "" }
                    );
                }
                (" Select SSH identity ", body, Some(cursor + 3))
            }
            ConnectionsPage::HostKey { confirmation, .. } => (
                " Explicit Host Key confirmation ",
                host_key_text(confirmation),
                None,
            ),
            ConnectionsPage::Remove(preview) => {
                (" Confirm connection removal ", removal_text(preview), None)
            }
            ConnectionsPage::ProjectRemove(preview) => (
                " Remove Project from recents ",
                format!(
                    "Project: {}\n\nThis only unregisters the recent-Project entry.\nThe directory, shipforge.yaml, connection registry, remote deployments, and local history are NOT deleted.\n\nSelect this directory again later to register it again.\nPress c to confirm.\n",
                    safe_text(&preview.root().display().to_string())
                ),
                None,
            ),
            ConnectionsPage::ProjectRemoved(_) => (
                " Project removed from recents ",
                "Project files and local history remain unchanged.".into(),
                None,
            ),
            ConnectionsPage::Loading {
                label,
                started,
                cancelling,
            } => (
                " Working ",
                format!(
                    "{label}\nElapsed: {}s\n\n{}",
                    started.elapsed().as_secs(),
                    if *cancelling {
                        "Cancellation requested. Waiting for the worker result; saved effects will be reported accurately."
                    } else {
                        "RUNNING: Esc requests cancellation. Wait for the worker before leaving."
                    }
                ),
                None,
            ),
        }
    }
}

fn connection_context(page: &str, connection: &ConnectionDetails) -> String {
    let DestinationSettings::LinuxSsh {
        host, port, user, ..
    } = &connection.current.settings;
    format!(
        "{page} / ID {} / {}",
        connection.key,
        endpoint_label(user, host, *port)
    )
}

fn form_context(page: &str, form: &ConnectionForm) -> String {
    form.existing.as_ref().map_or_else(
        || format!("Connections / New connection / {page}"),
        |connection| connection_context(&format!("Connections / {page}"), connection),
    )
}

fn form_text(form: &ConnectionForm) -> String {
    let mut body = format!(
        "{}\n\n{} Host: {}\n{} User: {}\n{} Port: {}\n\n{} SSH identity (↑/↓):\n",
        if form.existing.is_some() {
            "Edit connection: append a revision after Host Key confirmation"
        } else {
            "New connection: ID generated automatically on preview"
        },
        mark(form.field == SshField::Host),
        safe_text(&form.host),
        mark(form.field == SshField::User),
        safe_text(&form.user),
        mark(form.field == SshField::Port),
        safe_text(&form.port),
        mark(form.field == SshField::Credential)
    );
    let start = form.credential_cursor.saturating_sub(5);
    for (index, choice) in form.credentials.iter().enumerate().skip(start).take(12) {
        let _ = writeln!(
            body,
            "{} {}",
            mark(index == form.credential_cursor),
            safe_text(choice.label())
        );
    }
    if form.credentials.is_empty() {
        body.push_str("No identity found. F3: private key; F5: password.\n");
    }
    if form.hosts.is_empty() {
        body.push_str("No suggested hosts. Type a hostname in the Host field.\n");
    } else {
        body.push_str("Host field: F4 searches suggested hosts; F2 cycles them.\n");
    }
    body.push_str(
        "F5: enter/change password (hidden); Backspace: erase last; Delete: clear.\nPassword is saved encrypted for this Windows user after authentication.\nEnter captures the host-key fingerprint only. Review it before plain y saves.\n",
    );
    for notice in &form.notices {
        let _ = writeln!(body, "\n{}", safe_text(notice));
    }
    body
}

fn host_key_text(confirmation: &crate::application::HostKeyConfirmation) -> String {
    let preview = confirmation.preview();
    let draft = preview.draft();
    let mut body = format!(
        "Proposed connection: {}\nConnection ID: {}\n\nCaptured Host Key:\n{}\n\nVerify this fingerprint through a trusted channel.\nPress y only if you trust it. Authentication happens after confirmation.\nNo connection settings have been saved yet.\n",
        endpoint_label(&draft.user, &draft.host, draft.port),
        preview.key(),
        safe_text(confirmation.fingerprint())
    );
    if let Some(existing) = preview.details() {
        let DestinationSettings::LinuxSsh { host_key, .. } = &existing.current.settings;
        let _ = writeln!(
            body,
            "\nPreviously saved fingerprint: {}",
            safe_text(host_key.as_str())
        );
        if host_key.as_str() != confirmation.fingerprint() {
            body.push_str("WARNING: host-key fingerprint differs from the saved connection.\n");
        }
    }
    body
}

fn removal_text(preview: &crate::application::DestinationRemovalPreview) -> String {
    let mut body = connection_text(preview.details());
    body.push_str("\nOnly this local connection registration is removed. Remote files, services, credentials, Project YAML, and history stay unchanged.\n");
    for root in preview.project_references() {
        let _ = writeln!(
            body,
            "Referenced by Project: {}",
            safe_text(&root.display().to_string())
        );
    }
    if let Some(history) = preview.history_references() {
        let _ = writeln!(
            body,
            "Local history references: {} Deployments, {} Release records, {} Recovery reports",
            history.deployments, history.releases, history.recovery_reports
        );
    }
    if preview.history_missing() {
        body.push_str("Local history database is absent; no database was created.\n");
    }
    for blocker in preview.blockers() {
        let _ = writeln!(
            body,
            "UNKNOWN · {}: {}",
            safe_text(&blocker.source.to_string()),
            safe_text(blocker.reason)
        );
    }
    body.push_str(if preview.can_remove() {
        "\nNo local references found. Press c to recheck and remove.\n"
    } else {
        "\nRemoval blocked: referenced or unverified sources remain.\n"
    });
    body
}

fn connection_text(connection: &ConnectionDetails) -> String {
    let DestinationSettings::LinuxSsh {
        host,
        port,
        user,
        host_key,
        ..
    } = &connection.current.settings;
    format!(
        "Connection ID: {}\nSSH: {}\nRevision: {}\nSaved Host Key: {}\n",
        connection.key,
        endpoint_label(user, host, *port),
        connection.current.revision.get(),
        safe_text(host_key.as_str())
    )
}

const fn mark(selected: bool) -> &'static str {
    if selected { ">" } else { " " }
}
