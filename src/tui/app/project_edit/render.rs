use std::fmt::Write as _;

use ratatui::{
    Frame,
    layout::Rect,
    widgets::{Block, Borders, Paragraph, Wrap},
};

use super::{DeleteKind, ProjectEditPage, ProjectEditPreview, ProjectEditScreen};
use crate::tui::presentation::{context_label, environment_label};

pub(super) use crate::tui::presentation::safe_text;

impl super::App {
    pub(in crate::tui) fn project_edit_context_label(&self, screen: &ProjectEditScreen) -> String {
        self.project_edit_task.as_ref().map_or_else(
            || screen.context_label(),
            |task| format!("{} · Working", task.origin.context_label()),
        )
    }
}

impl ProjectEditScreen {
    pub(in crate::tui) fn context_label(&self) -> String {
        let draft = self.draft.as_deref();
        let mut page = &self.page;
        // Text inputs retain the form they will return to; never use the input
        // value itself as an identity or lose its deployment-target context.
        while let ProjectEditPage::Text(edit) = page {
            page = &edit.back;
        }
        let (label, environment, component) = page_context(page, draft);
        let project = match &self.page {
            ProjectEditPage::Loaded(draft) => Some(draft.setup.project.as_str()),
            ProjectEditPage::Saved(config) => Some(config.project.as_str()),
            ProjectEditPage::Preview(preview) => Some(preview.config().project.as_str()),
            _ => draft.map(|draft| draft.setup.project.as_str()),
        };
        let label = if matches!(self.page, ProjectEditPage::Text(_)) {
            "Project editor / Edit value"
        } else {
            label
        };
        context_label(label, project, environment, component)
    }

    pub(in crate::tui::app) fn requires_plain_confirmation(&self) -> bool {
        matches!(
            self.page,
            ProjectEditPage::Preview(_) | ProjectEditPage::Delete(_) | ProjectEditPage::Discard
        )
    }

    pub(in crate::tui) fn help(&self) -> &'static str {
        match &self.page {
            ProjectEditPage::Home if self.draft.is_none() => {
                "Esc return   r retry loading configuration"
            }
            ProjectEditPage::Home => {
                "Esc leave/discard   n name   c Components   e Environments   p YAML preview"
            }
            ProjectEditPage::Loading {
                cancelling: true, ..
            } => "CANCELLING   Wait for the worker result before leaving (no detach)",
            ProjectEditPage::Loading { .. } => {
                "Esc / Ctrl+C cancel   Wait for the worker before leaving (no detach)"
            }
            ProjectEditPage::Components { .. }
                if self
                    .draft
                    .as_ref()
                    .is_none_or(|draft| draft.setup.components.is_empty()) =>
            {
                "Esc back   f discover candidates   a add Component"
            }
            ProjectEditPage::Components { .. }
                if self
                    .draft
                    .as_ref()
                    .is_some_and(|draft| draft.setup.components.len() == 1) =>
            {
                "Esc back   F4 search   ↑/↓ choose   Enter edit   f discover   a add"
            }
            ProjectEditPage::Components { .. } => {
                "Esc back   F4 search   ↑/↓ choose   Enter edit   f discover   a add   d remove"
            }
            ProjectEditPage::Environments { .. }
                if self
                    .draft
                    .as_ref()
                    .is_none_or(|draft| draft.setup.environments.is_empty()) =>
            {
                "Esc back   a add Environment"
            }
            ProjectEditPage::Environments { .. }
                if self
                    .draft
                    .as_ref()
                    .is_some_and(|draft| draft.setup.environments.len() == 1) =>
            {
                "Esc back   F4 search   ↑/↓ choose   Enter edit/rename   a add"
            }
            ProjectEditPage::Environments { .. } => {
                "Esc back   F4 search   ↑/↓ choose   Enter edit/rename   a add   d remove"
            }
            ProjectEditPage::Commands { form, .. } if form.commands.is_empty() => {
                "Esc Component   a add executable"
            }
            ProjectEditPage::Commands { .. } => {
                "Esc Component   F4 search   ↑/↓ command   Enter edit argv   a add   d remove"
            }
            ProjectEditPage::Command { .. } => {
                "Esc commands   F4 search   ↑/↓ argv item   Enter edit   a add   d remove arg"
            }
            ProjectEditPage::Environment { .. } => {
                "Esc discard form   F4 search   ↑/↓ field   Space toggle   Enter edit/apply"
            }
            ProjectEditPage::Target { .. } => {
                "F4 search · ↑↓ field · Enter edit/apply · b remote choices · Esc back"
            }
            ProjectEditPage::Dependencies { names, .. } if names.is_empty() => {
                "Esc cancel   Enter apply no dependencies"
            }
            ProjectEditPage::Dependencies { .. } => {
                "Esc cancel   F4 search   ↑/↓ Component   Space toggle   Enter apply"
            }
            ProjectEditPage::Discovery { report, .. } if report.components.is_empty() => {
                "Esc Components   Then a adds manually or f retries discovery"
            }
            ProjectEditPage::Destination { .. }
                if self
                    .draft
                    .as_ref()
                    .is_none_or(|draft| draft.destinations().is_empty()) =>
            {
                "Esc target   Add a connection from Projects after leaving the editor"
            }
            ProjectEditPage::Discovery { .. } | ProjectEditPage::Destination { .. } => {
                "Esc cancel   F4 search   ↑/↓ choose   Enter use selected"
            }
            ProjectEditPage::Text(_) => {
                "Esc cancel field   Type value   Backspace erase   Delete clear   Enter apply"
            }
            ProjectEditPage::Preview(_) => {
                "Esc reject preview   ↑/↓ PgUp/PgDn scroll   c confirm exact YAML save"
            }
            ProjectEditPage::Delete(_) => {
                "Esc cancel   c remove from draft only (remote resources untouched)"
            }
            ProjectEditPage::Discard => "c discard unsaved draft and leave   Esc keep editing",
            _ => "Esc discard form   F4 search   ↑/↓ field   Enter edit / apply to draft",
        }
    }

    pub(in crate::tui) fn render(&self, frame: &mut Frame<'_>, area: Rect) {
        let (title, body, cursor) = self.content();
        let scroll = cursor.map_or(self.scroll, |line| {
            let visible = usize::from(area.height.saturating_sub(2)).max(1);
            u16::try_from(line.saturating_sub(visible.saturating_sub(1))).unwrap_or(u16::MAX)
        });
        let paragraph = Paragraph::new(body)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!(" {title} ")),
            )
            .scroll((scroll, 0));
        let paragraph = if cursor.is_none() {
            paragraph.wrap(Wrap { trim: false })
        } else {
            paragraph
        };
        frame.render_widget(paragraph, area);
    }

    fn content(&self) -> (&'static str, String, Option<usize>) {
        match &self.page {
            ProjectEditPage::Home => ("Edit project configuration", self.home(), None),
            ProjectEditPage::Loading { label, started, cancelling } => ("Working", format!("{label}\nElapsed: {}s\n{}", started.elapsed().as_secs(), if *cancelling { "CANCELLING: waiting for the worker result before leaving." } else { "RUNNING: Esc requests cancellation. The interface remains responsive." }), None),
            ProjectEditPage::Text(edit) => ("Edit one value", format!("{}\n\n{}\n\nNo IDs or secrets should be entered. Changes remain in memory until YAML preview is confirmed.", edit.field.label(), safe_text(&edit.value)), None),
            ProjectEditPage::Discard => ("Discard unsaved draft?", "No file has been saved. Press c to discard all in-memory edits; Esc keeps editing.".into(), None),
            ProjectEditPage::Delete(kind) => ("Confirm draft removal", delete_text(kind), None),
            ProjectEditPage::Preview(preview) => ("Confirm exact shipforge.yaml", format!("{}\n--- EXACT YAML TO SAVE ---\n{}", preview_summary(preview), preview.yaml()), None),
            ProjectEditPage::Components { cursor } => self.components(*cursor),
            ProjectEditPage::Environments { cursor } => self.environments(*cursor),
            ProjectEditPage::Discovery { report, cursor } => {
                let mut text = "Local discovery only; selecting a candidate opens an editable argv form.\n\n".to_owned();
                for (index, candidate) in report.components.iter().enumerate() { let _ = writeln!(text, "{} {} · {:?} · {}", mark(index == *cursor), candidate.name, candidate.confidence, safe_text(&candidate.source.display().to_string())); }
                if report.components.is_empty() { text.push_str("No supported candidates were found.\nEsc returns to Components; press a there to add manually or f to retry discovery.\n"); }
                for notice in &report.notices { let _ = writeln!(text, "NOTICE: {}", safe_text(notice)); }
                ("Choose discovered Component", text, Some(cursor + 2))
            }
            _ => self.form_content(),
        }
    }

    fn home(&self) -> String {
        let Some(draft) = &self.draft else {
            return "Configuration is unavailable. Press r to retry, or Esc to return. No file was created.".into();
        };
        format!(
            "Project: {}\nDirectory: {}\n{}\n\n[n] Rename Project (identity is retained)\n[c] Add / edit / remove Components and argv build commands\n[e] Add / explicitly rename / remove Environments and their targets\n[p] Validate changes and inspect exact YAML before saving\n\nAll edits are drafts. No build, SSH connection, deployment, or remote deletion occurs here.\nInternal IDs are retained/generated by the service; never type IDs or secrets.",
            safe_text(&draft.setup.project),
            safe_text(&self.root.display().to_string()),
            if self.dirty {
                "UNSAVED DRAFT"
            } else {
                "Saved configuration loaded; no changes yet"
            }
        )
    }

    fn components(&self, cursor: usize) -> (&'static str, String, Option<usize>) {
        let mut text =
            "Choose a Component; prefer f discovery for a new build setup.\n\n".to_owned();
        if let Some(draft) = &self.draft {
            for (index, (name, component)) in draft.setup.components.iter().enumerate() {
                let _ = writeln!(
                    text,
                    "{} {} · workdir {} · artifact {} · {} argv command(s)",
                    mark(index == cursor),
                    name,
                    component.working_directory.as_ref().map_or_else(
                        || format!("{name}/ (default)"),
                        |path| safe_text(&path.display().to_string())
                    ),
                    safe_text(&component.artifact.path.display().to_string()),
                    component.build.len()
                );
            }
        }
        if self
            .draft
            .as_ref()
            .is_none_or(|draft| draft.setup.components.is_empty())
        {
            text.push_str("No Components in the draft.\nPress f to discover candidates, a to add one, or Esc to return.\n");
        }
        text.push_str("\nThe last Component cannot be deleted. Removing one also removes its Environment assignments and ordering links in the draft.");
        ("Components", text, Some(cursor + 2))
    }

    fn environments(&self, cursor: usize) -> (&'static str, String, Option<usize>) {
        let mut text =
            "Choose an Environment; rename is explicit and retains its identity.\n\n".to_owned();
        if let Some(draft) = &self.draft {
            for (index, (name, environment)) in draft.setup.environments.iter().enumerate() {
                let _ = writeln!(
                    text,
                    "{} {} · {} selected Component(s)",
                    mark(index == cursor),
                    environment_label(name),
                    environment.components.len()
                );
            }
        }
        if self
            .draft
            .as_ref()
            .is_none_or(|draft| draft.setup.environments.is_empty())
        {
            text.push_str("No Environments in the draft.\nPress a to add one or Esc to return.\n");
        }
        text.push_str("\nThe last Environment cannot be deleted. Config removal never removes remote files or stops services.");
        ("Environments", text, Some(cursor + 2))
    }

    fn form_content(&self) -> (&'static str, String, Option<usize>) {
        match &self.page {
            ProjectEditPage::Component { form, cursor } => (
                "Component draft",
                fields(
                    &[
                        format!(
                            "Name: {}{}",
                            safe_text(&form.name),
                            if form.original.is_some() {
                                " (existing key retained)"
                            } else {
                                ""
                            }
                        ),
                        format!(
                            "Working directory: {}",
                            if form.working_directory.is_empty() {
                                format!("{}/ (Component default)", safe_text(&form.name))
                            } else {
                                safe_text(&form.working_directory)
                            }
                        ),
                        format!(
                            "Artifact (relative to workdir): {}",
                            safe_text(&form.artifact)
                        ),
                        format!("Build argv commands: {}", form.commands.len()),
                        "Apply Component to draft (not YAML save)".into(),
                    ],
                    *cursor,
                ),
                Some(*cursor),
            ),
            ProjectEditPage::Commands { form, cursor } => command_list(form, *cursor),
            ProjectEditPage::Command {
                form,
                command,
                cursor,
            } => command_form(form, *command, *cursor),
            ProjectEditPage::Environment { form, cursor } => self.environment_form(form, *cursor),
            ProjectEditPage::Target { form, cursor } => self.target_form(form, *cursor),
            ProjectEditPage::Destination { form: _, cursor } => {
                let choices = self
                    .draft
                    .as_ref()
                    .map(|draft| draft.destinations())
                    .unwrap_or_default();
                let rows = choices
                    .iter()
                    .map(|destination| {
                        format!(
                            "ID {} · r{} · {}",
                            destination.key,
                            destination.revision.get(),
                            safe_text(&destination.endpoint)
                        )
                    })
                    .collect::<Vec<_>>();
                (
                    "Choose saved connection (no manual ID entry)",
                    if rows.is_empty() {
                        "No saved connections were available when this editor loaded.\nEsc returns to the target form. Save or discard the editor, add a connection from Projects, then reopen this editor.".into()
                    } else {
                        fields(&rows, *cursor)
                    },
                    Some(*cursor),
                )
            }
            ProjectEditPage::Dependencies {
                names,
                selected,
                cursor,
                ..
            } => {
                let rows = names
                    .iter()
                    .map(|name| {
                        format!(
                            "[{}] after {}",
                            if selected.contains(name) { 'x' } else { ' ' },
                            name
                        )
                    })
                    .collect::<Vec<_>>();
                (
                    "Choose ordering dependencies",
                    if rows.is_empty() {
                        "No other Component is selected in this Environment.\nEnter applies no dependencies; Esc cancels.".into()
                    } else {
                        fields(&rows, *cursor)
                    },
                    Some(*cursor),
                )
            }
            _ => (
                "Project editor",
                "Waiting for a valid editor result.".into(),
                None,
            ),
        }
    }

    fn environment_form(
        &self,
        form: &super::forms::EnvironmentForm,
        cursor: usize,
    ) -> (&'static str, String, Option<usize>) {
        let mut rows = vec![format!(
            "Environment name: {}{}",
            environment_label(&form.name),
            if form.original.is_some() {
                " (explicit rename preserves ID)"
            } else {
                " (new ID generated on preview)"
            }
        )];
        if let Some(draft) = &self.draft {
            rows.extend(draft.setup.components.keys().map(|name| {
                format!(
                    "[{}] {}{}",
                    if form.targets.contains_key(name) {
                        'x'
                    } else {
                        ' '
                    },
                    name,
                    form.targets
                        .get(name)
                        .map_or_else(String::new, |target| format!(
                            " · {}",
                            destination_label(draft, &target.destination)
                        ))
                )
            }));
        }
        rows.push("Apply Environment to draft (not YAML save)".into());
        (
            "Environment / Component subset",
            fields(&rows, cursor),
            Some(cursor),
        )
    }

    fn target_form(
        &self,
        form: &super::forms::TargetForm,
        cursor: usize,
    ) -> (&'static str, String, Option<usize>) {
        let destination = self.draft.as_ref().map_or_else(
            || "unavailable".into(),
            |draft| destination_label(draft, &form.target.destination),
        );
        let root = form.target.root.as_ref().map_or_else(
            || "empty: retain existing frozen root; new targets use generated default (see YAML preview)".into(),
            |root| safe_text(root),
        );
        let rows = [
            format!("Connection: {destination}"),
            format!("Remote root: {root}"),
            format!(
                "Service: {}",
                form.target
                    .service
                    .as_ref()
                    .map_or("none", |service| service
                        .preset_unit()
                        .unwrap_or("custom commands (b to configure)"))
            ),
            format!(
                "Health URL: {}",
                form.target.health.as_deref().unwrap_or("none")
            ),
            format!(
                "After: {}",
                form.target
                    .after
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            "Apply target to Environment form".into(),
        ];
        (
            "Component deployment target",
            fields(&rows, cursor),
            Some(cursor),
        )
    }
}

fn page_context<'a>(
    page: &'a ProjectEditPage,
    draft: Option<&'a super::ProjectEditDraft>,
) -> (&'static str, Option<&'a str>, Option<&'a str>) {
    match page {
        ProjectEditPage::Components { cursor } => (
            "Project editor / Components",
            None,
            draft.and_then(|draft| {
                draft
                    .setup
                    .components
                    .keys()
                    .nth(*cursor)
                    .map(crate::domain::ComponentName::as_str)
            }),
        ),
        ProjectEditPage::Component { form, .. }
        | ProjectEditPage::Commands { form, .. }
        | ProjectEditPage::Command { form, .. } => {
            ("Project editor / Component", None, Some(&form.name))
        }
        ProjectEditPage::Environments { cursor } => (
            "Project editor / Environments",
            draft.and_then(|draft| {
                draft
                    .setup
                    .environments
                    .keys()
                    .nth(*cursor)
                    .map(String::as_str)
            }),
            None,
        ),
        ProjectEditPage::Environment { form, .. } => {
            ("Project editor / Environment", Some(&form.name), None)
        }
        ProjectEditPage::Target { form, .. }
        | ProjectEditPage::Destination { form, .. }
        | ProjectEditPage::Dependencies { form, .. } => (
            match page {
                ProjectEditPage::Destination { .. } => "Project editor / Choose connection",
                ProjectEditPage::Dependencies { .. } => "Project editor / Dependencies",
                _ => "Project editor / Target",
            },
            Some(&form.environment.name),
            Some(form.component.as_str()),
        ),
        ProjectEditPage::Delete(DeleteKind::Component(name)) => (
            "Project editor / Remove Component",
            None,
            Some(name.as_str()),
        ),
        ProjectEditPage::Delete(DeleteKind::Environment(name)) => {
            ("Project editor / Remove Environment", Some(name), None)
        }
        ProjectEditPage::Loading { label, .. } => (label, None, None),
        ProjectEditPage::Discovery { .. } => ("Project editor / Discovery", None, None),
        ProjectEditPage::Preview(_) => ("Project editor / Confirm YAML", None, None),
        ProjectEditPage::Discard => ("Project editor / Discard draft", None, None),
        _ => ("Project editor", None, None),
    }
}

fn destination_label(
    draft: &super::ProjectEditDraft,
    key: &crate::domain::DestinationKey,
) -> String {
    draft
        .destinations()
        .iter()
        .find(|destination| &destination.key == key)
        .map_or_else(
            || format!("ID {key} · unavailable saved connection"),
            |destination| {
                format!(
                    "ID {} · r{} · {}",
                    destination.key,
                    destination.revision.get(),
                    safe_text(&destination.endpoint)
                )
            },
        )
}

fn fields(rows: &[String], cursor: usize) -> String {
    rows.iter()
        .enumerate()
        .fold(String::new(), |mut text, (index, row)| {
            let _ = writeln!(text, "{} {}", mark(index == cursor), safe_text(row));
            text
        })
}

fn command_list(
    form: &super::forms::ComponentForm,
    cursor: usize,
) -> (&'static str, String, Option<usize>) {
    let mut text =
        "Each command has one executable and separate literal argv entries.\n\n".to_owned();
    for (index, command) in form.commands.iter().enumerate() {
        let _ = writeln!(
            text,
            "{} {} · {} argument(s){}",
            mark(index == cursor),
            safe_text(&command.program),
            command.args.len(),
            if command.shell {
                " — UNSUPPORTED SHELL STRING: explicitly replace executable"
            } else {
                ""
            }
        );
    }
    if form.commands.is_empty() {
        text.push_str("Press a to add the first executable. No shell command-line parser is used.");
    }
    ("Build commands", text, Some(cursor + 2))
}

fn command_form(
    form: &super::forms::ComponentForm,
    index: usize,
    cursor: usize,
) -> (&'static str, String, Option<usize>) {
    let Some(command) = form.commands.get(index) else {
        return (
            "Build argv",
            "Command unavailable. Press Esc to return to build commands.".into(),
            None,
        );
    };
    let mut rows = vec![format!("Executable: {}", safe_text(&command.program))];
    rows.extend(
        command
            .args
            .iter()
            .enumerate()
            .map(|(index, value)| format!("argv[{}]: {:?}", index + 1, safe_text(value))),
    );
    (
        "One command · literal argv",
        fields(&rows, cursor),
        Some(cursor),
    )
}

fn delete_text(kind: &DeleteKind) -> String {
    match kind {
        DeleteKind::Component(name) => format!(
            "Remove Component {name} from this draft, including its Environment assignments and after references?\n\nPress c to confirm the draft change. A separate exact-YAML preview and confirmation are still required before saving. Remote resources are untouched."
        ),
        DeleteKind::Environment(name) => format!(
            "Remove Environment {} and its Component assignments from this draft?\n\nPress c to confirm the draft change. A separate exact-YAML preview and confirmation are still required before saving. Remote resources are untouched.",
            environment_label(name)
        ),
    }
}

fn preview_summary(preview: &ProjectEditPreview) -> String {
    let original = preview.draft().original();
    let config = preview.config();
    let mut text = format!(
        "CONFIRM LOCAL CONFIGURATION SAVE — no build or deployment\nFile: {}\nProject name: {} -> {}\nProject ID: {} ({})\n",
        safe_text(&preview.root().join("shipforge.yaml").display().to_string()),
        safe_text(&original.project),
        safe_text(&config.project),
        config.project_id,
        if original.project_id == config.project_id {
            "preserved"
        } else {
            "changed: review carefully"
        }
    );
    for (name, environment) in &config.environments {
        let before = original
            .environments
            .iter()
            .find(|(_, before)| before.id == environment.id);
        let _ = writeln!(
            text,
            "\nEnvironment {}: ID {} ({})",
            environment_label(name),
            environment.id,
            before.map_or("newly generated", |_| "preserved")
        );
        if let Some((old_name, _)) = before
            && old_name != name
        {
            let _ = writeln!(
                text,
                "Explicit rename: {} -> {}",
                environment_label(old_name),
                environment_label(name)
            );
        }
        for (component, target) in &environment.components {
            let old = before.and_then(|(_, environment)| environment.components.get(component));
            let _ = writeln!(
                text,
                "  {}: generation {} -> {}; root {} -> {}; connection {}",
                component,
                old.map_or_else(
                    || "new".into(),
                    |target| target.generation.get().to_string()
                ),
                target.generation.get(),
                old.map_or_else(|| "new".into(), |target| safe_text(&target.root)),
                safe_text(&target.root),
                destination_label(preview.draft(), &target.destination)
            );
        }
        if let Some((_, previous)) = before {
            for component in previous
                .components
                .keys()
                .filter(|component| !environment.components.contains_key(*component))
            {
                let _ = writeln!(
                    text,
                    "  REMOVED assignment: {component} (configuration only; remote resources untouched)"
                );
            }
        }
    }
    for name in original
        .components
        .keys()
        .filter(|name| !config.components.contains_key(*name))
    {
        let _ = writeln!(text, "REMOVED Component: {name} (configuration only)");
    }
    for (name, before) in &original.environments {
        if !config
            .environments
            .values()
            .any(|environment| environment.id == before.id)
        {
            let _ = writeln!(
                text,
                "REMOVED Environment: {} (configuration only)",
                environment_label(name)
            );
        }
    }
    text.push_str("\nPress plain c to save exactly the YAML below. Esc rejects this preview.\n");
    text
}

const fn mark(selected: bool) -> &'static str {
    if selected { ">" } else { " " }
}

pub(super) fn safe_character(character: char) -> bool {
    !character.is_control()
        && !matches!(character, '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{206f}' | '\u{feff}')
}
