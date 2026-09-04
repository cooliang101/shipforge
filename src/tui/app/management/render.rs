use std::fmt::Write as _;

use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Style},
    widgets::{Block, Borders, Paragraph, Wrap},
};

use crate::{
    history::{CurrentAlignment, PackageAlignment},
    tui::presentation::{context_label, environment_label, is_production, step_label},
};

use super::{ManagementPage, ManagementScreen, ReleaseRef};
pub(super) use crate::tui::presentation::safe_text;

impl ManagementScreen {
    pub(in crate::tui) fn help(&self) -> &'static str {
        if self.historical_remote_page() {
            return "Esc back  Read-only historical scope; remote operations unavailable";
        }
        match &self.page {
            ManagementPage::Home if self.scope.historical_environment.is_some() => {
                "Esc current management  h history  p saved inspections  (read-only)"
            }
            ManagementPage::Home => {
                "Esc overview  ←/→ env  h history  i inspect  p reports  a old envs"
            }
            ManagementPage::Loading {
                cancelling: true, ..
            } => "Cancellation requested; waiting for a safe result. Do not close the terminal.",
            ManagementPage::Loading { .. } => {
                "Esc / Ctrl+C request cancellation; l opens active rollback logs"
            }
            ManagementPage::Environments { .. }
            | ManagementPage::History { .. }
            | ManagementPage::Reports { .. } => {
                "Esc back  ↑/↓ select  Enter details  n/b next/previous page  f refresh"
            }
            ManagementPage::Detail(_) if self.scope.historical_environment.is_some() => {
                "Esc back  ↑/↓ PgUp/PgDn scroll  l logs  (read-only historical scope)"
            }
            ManagementPage::Detail(details) if details.snapshots.is_empty() => {
                "Esc back  ↑/↓ PgUp/PgDn scroll  l logs  (no frozen remote context)"
            }
            ManagementPage::Detail(_) => {
                "Esc back  ↑/↓ PgUp/PgDn scroll  l logs  r rollback  i inspect remote"
            }
            ManagementPage::InspectSelection { selected, .. } if selected.is_empty() => {
                "Esc back  ↑/↓ Component  Space select at least one Component"
            }
            ManagementPage::InspectSelection { .. } => {
                "Esc back  ↑/↓ Component  Space toggle  Enter inspect selected (read-only)"
            }
            ManagementPage::RollbackSelection { selected, .. } if selected.is_empty() => {
                "Esc back  ↑/↓ Component  Space select at least one Component"
            }
            ManagementPage::RollbackSelection { .. } => {
                "Esc back  ↑/↓ Component  Space toggle  Enter find selected targets"
            }
            ManagementPage::RollbackTargets { selected, .. } if selected.is_empty() => {
                "Esc back  ↑/↓ Component  ←/→ version  Space select an available target"
            }
            ManagementPage::RollbackTargets { .. } => {
                "Esc back  ↑/↓ Component  ←/→ version  Space select  Enter check targets"
            }
            ManagementPage::RollbackReview(_) => {
                "Esc reject  ↑/↓ PgUp/PgDn scroll  c confirm rollback"
            }
            ManagementPage::RollbackFinished(_) | ManagementPage::RollbackFailed { .. } => {
                "Esc back  ↑/↓ PgUp/PgDn scroll  l this rollback's logs"
            }
            ManagementPage::Report { .. } => "Esc back  ↑/↓ PgUp/PgDn scroll  Home top",
        }
    }

    fn historical_remote_page(&self) -> bool {
        self.scope.historical_environment.is_some()
            && matches!(
                self.page,
                ManagementPage::InspectSelection { .. }
                    | ManagementPage::RollbackSelection { .. }
                    | ManagementPage::RollbackTargets { .. }
                    | ManagementPage::RollbackReview(_)
            )
    }

    pub(in crate::tui) fn context_label(&self) -> String {
        let page = match &self.page {
            ManagementPage::Home => "Manage",
            ManagementPage::Environments { .. } => "Historical Environments",
            ManagementPage::History { .. } | ManagementPage::Detail(_) => "Deployment history",
            ManagementPage::Reports { .. } | ManagementPage::Report { .. } => "Inspections",
            ManagementPage::InspectSelection { .. } => "Choose inspection targets",
            ManagementPage::RollbackSelection { .. } | ManagementPage::RollbackTargets { .. } => {
                "Choose rollback targets"
            }
            ManagementPage::RollbackReview(_) => "Confirm rollback",
            ManagementPage::RollbackFinished(_) => "Rollback result",
            ManagementPage::RollbackFailed { .. } => "Rollback did not complete normally",
            ManagementPage::Loading { .. } => "Working",
        };
        let environment = self.scope.historical_environment.as_ref().map_or_else(
            || Some(self.scope.environment.as_str()),
            |id| self.scope.current_name_for(id),
        );
        let mut label = context_label(
            page,
            Some(&self.scope.config.project),
            environment,
            self.context_component(),
        );
        if let Some(id) = &self.scope.historical_environment {
            let _ = write!(label, " · read-only historical {id}");
        }
        label
    }

    fn context_component(&self) -> Option<&str> {
        match &self.page {
            ManagementPage::InspectSelection { names, cursor, .. } => {
                names.get(*cursor).map(crate::domain::ComponentName::as_str)
            }
            ManagementPage::RollbackSelection {
                details, cursor, ..
            } => details
                .snapshots
                .get(*cursor)
                .map(|snapshot| snapshot.release.component.as_str()),
            ManagementPage::RollbackTargets {
                candidates, cursor, ..
            } => candidates
                .components
                .get(*cursor)
                .map(|component| component.component.as_str()),
            _ => None,
        }
    }

    pub(in crate::tui) fn render(&self, frame: &mut Frame<'_>, area: Rect, app: &super::App) {
        let (title, body, cursor) = self.content(app);
        let environment = self.scope.historical_environment.as_ref().map_or_else(
            || environment_label(&self.scope.environment),
            |id| self.historical_environment_label(id),
        );
        let production = self.scope.historical_environment.as_ref().map_or_else(
            || is_production(&self.scope.environment),
            |id| self.scope.current_name_for(id).is_some_and(is_production),
        );
        let heading = format!(
            " {} · {} / {} ",
            title,
            safe_text(&self.scope.config.project),
            safe_text(&environment),
        );
        let scroll = cursor.map_or(self.scroll, |cursor| {
            let visible = usize::from(area.height.saturating_sub(4)).max(1);
            u16::try_from(cursor.saturating_sub(visible.saturating_sub(1))).unwrap_or(u16::MAX)
        });
        let block = Block::default()
            .title(heading)
            .borders(Borders::ALL)
            .border_style(if production {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default()
            });
        let paragraph = Paragraph::new(body).block(block).scroll((scroll, 0));
        // List cursor offsets count explicit lines. Wrapping these rows would
        // hide the selected entry on narrow terminals; details remain wrapped.
        let paragraph = if cursor.is_some() {
            paragraph
        } else {
            paragraph.wrap(Wrap { trim: false })
        };
        frame.render_widget(paragraph, area);
    }

    fn content(&self, app: &super::App) -> (&'static str, String, Option<usize>) {
        if self.historical_remote_page() {
            return (
                "Historical Environment · read-only",
                super::HISTORICAL_READ_ONLY.into(),
                None,
            );
        }
        if let ManagementPage::Loading { cancelling, .. } = &self.page
            && app.management_has_live_progress()
            && let Some(progress) = &app.live_progress
        {
            let snapshot = progress.snapshot();
            let mut text = format!(
                "Rollback running · elapsed {} ms\n{}\nl opens logs, full step progress, search and export.\n\n",
                snapshot.elapsed_ms,
                if *cancelling {
                    "Safe cancellation requested; waiting for compensation."
                } else {
                    "Esc / Ctrl+C requests safe cancellation."
                }
            );
            let mut steps: Vec<_> = snapshot.steps.iter().collect();
            steps.sort_by_key(|step| std::cmp::Reverse(step.updated_sequence));
            for step in steps.iter().take(3) {
                let _ = writeln!(
                    text,
                    "{} / {}: {:?}; persistence {:?}",
                    step.scope.component,
                    step_label(&step.scope.step),
                    step.state,
                    step.persistence
                );
            }
            if snapshot.dropped_rows > 0
                || snapshot.dropped_steps > 0
                || snapshot.rejected_events > 0
            {
                let _ = writeln!(
                    text,
                    "Window gaps: {} rows / {} steps / {} rejected",
                    snapshot.dropped_rows, snapshot.dropped_steps, snapshot.rejected_events
                );
            }
            text.push_str("\nLatest output (bounded):\n");
            let rows = app.live_logs.matching();
            for row in rows.iter().skip(rows.len().saturating_sub(8)) {
                let _ = writeln!(
                    text,
                    "{}",
                    safe_text(row.event.message.lines().next().unwrap_or(""))
                );
            }
            return ("Rollback progress", text, None);
        }
        match &self.page {
            ManagementPage::Home | ManagementPage::Loading { .. } => self.overview_content(),
            ManagementPage::Environments { .. } => self.environments_content(),
            ManagementPage::History { .. } | ManagementPage::Detail(_) => self.deployment_content(),
            ManagementPage::Reports { .. } | ManagementPage::Report { .. } => {
                self.inspection_content()
            }
            ManagementPage::InspectSelection { .. } | ManagementPage::RollbackSelection { .. } => {
                self.selection_content(app)
            }
            ManagementPage::RollbackTargets { .. } => self.rollback_targets_content(),
            ManagementPage::RollbackReview(_) | ManagementPage::RollbackFinished(_) => {
                self.rollback_execution_content()
            }
            ManagementPage::RollbackFailed {
                request_id,
                message,
                progress,
            } => {
                let snapshot = progress.snapshot();
                let deployment = snapshot.deployment.as_ref().map_or_else(
                    || "Unavailable: this request supplied no Deployment ID; the source Deployment is not substituted.".into(),
                    ToString::to_string,
                );
                (
                    "Rollback did not complete normally",
                    format!(
                        "Local request: {request_id}\nRollback Deployment: {deployment}\nElapsed: {} ms (execution stopped)\n\n{}\n\nInspect this request's available logs and recorded history; missing evidence is unknown. This error does not prove that remote state is unchanged.\n\nl opens this rollback's log window; h there reads its retained files when an ID is available. Inspect current state before retrying; this page cannot re-confirm the old plan.",
                        snapshot.elapsed_ms,
                        safe_text(message)
                    ),
                    None,
                )
            }
        }
    }

    fn overview_content(&self) -> (&'static str, String, Option<usize>) {
        match &self.page {
            ManagementPage::Home if self.scope.historical_environment.is_some() => (
                "Historical Environment · read-only",
                format!(
                    "Project directory: {}\nEnvironment: {}\n\n[h] Local deployment history and logs\n[p] Saved inspection reports\n\nRead-only local evidence; no remote connections or rollback.\nEnvironment identity, not its name, determines this scope.\nOld configuration is not reconstructed. Esc returns to current management.",
                    safe_text(&self.scope.root.display().to_string()),
                    self.scope
                        .historical_environment
                        .as_ref()
                        .map_or_else(String::new, |id| self.historical_environment_label(id),),
                ),
                None,
            ),
            ManagementPage::Home => (
                "Manage",
                format!(
                    "Project directory: {}\n\n[h] Local deployment history and logs\n[i] Inspect selected Components / remote Releases\n[p] Saved inspection reports\n[a] Historical Environment IDs (including removed Environments)\n\nOpening history never connects to a server.\nInspection is read-only on the server; it saves a separate local report.\nInventory does not prove service health or historical success.",
                    safe_text(&self.scope.root.display().to_string())
                ),
                None,
            ),
            ManagementPage::Loading {
                label,
                started,
                cancelling,
            } => (
                "Working",
                format!(
                    "{label}\nElapsed: {}s\n{}",
                    started.elapsed().as_secs(),
                    if *cancelling {
                        "Cancellation requested. Waiting for the worker and any required compensation."
                    } else {
                        "The interface remains responsive. Esc requests cancellation."
                    }
                ),
                None,
            ),
            _ => unreachable!("page category is selected by the exhaustive renderer"),
        }
    }

    fn historical_environment_label(&self, id: &crate::domain::EnvironmentId) -> String {
        self.scope.current_name_for(id).map_or_else(
            || format!("{id} · removed"),
            |name| format!("{id} · {} (current configuration)", environment_label(name)),
        )
    }

    fn environments_content(&self) -> (&'static str, String, Option<usize>) {
        let ManagementPage::Environments {
            page,
            offset,
            cursor,
        } = &self.page
        else {
            unreachable!("only the Environment selector is routed here");
        };
        let mut text = if !page.database_missing && page.items.is_empty() {
            "No recorded Environment IDs for this Project.\n\n".into()
        } else {
            page_header(
                page.database_missing,
                page.items.is_empty(),
                *offset,
                page.more,
            )
        };
        text.push_str("Select an ID for read-only local history, logs and saved reports.\n\n");
        let first_row = text.lines().count();
        for (index, environment) in page.items.iter().enumerate() {
            let _ = writeln!(
                text,
                "{} {}",
                mark(index == *cursor),
                self.historical_environment_label(environment),
            );
        }
        (
            "Project historical Environments · local",
            text,
            Some(cursor + first_row),
        )
    }

    fn deployment_content(&self) -> (&'static str, String, Option<usize>) {
        match &self.page {
            ManagementPage::History {
                page,
                offset,
                cursor,
            } => {
                let mut text = page_header(
                    page.database_missing,
                    page.items.is_empty(),
                    *offset,
                    page.more,
                );
                for (index, record) in page.items.iter().enumerate() {
                    let _ = writeln!(
                        text,
                        "{} {}  {:?} / {:?}  pending:{}  started:{}",
                        mark(index == *cursor),
                        record.deployment,
                        record.kind,
                        record.state,
                        record.pending_intent_count,
                        timestamp(record.created_at_ms)
                    );
                }
                ("Deployment history · local", text, Some(cursor + 2))
            }
            ManagementPage::Detail(details) => {
                let mut text = deployment_details(details);
                if details.snapshots.is_empty() {
                    let _ = writeln!(text, "\n{}", super::MISSING_FROZEN_CONTEXT);
                }
                ("Deployment details · original record", text, None)
            }
            _ => unreachable!("page category is selected by the exhaustive renderer"),
        }
    }

    fn inspection_content(&self) -> (&'static str, String, Option<usize>) {
        match &self.page {
            ManagementPage::Reports {
                page,
                offset,
                cursor,
            } => {
                let mut text = page_header(
                    page.database_missing,
                    page.items.is_empty(),
                    *offset,
                    page.more,
                );
                for (index, report) in page.items.iter().enumerate() {
                    let _ = writeln!(
                        text,
                        "{} {}  {} Components  source:{}  checked:{}",
                        mark(index == *cursor),
                        report.id,
                        report.components.len(),
                        report
                            .related_deployment
                            .as_ref()
                            .map_or_else(|| "inventory only".into(), ToString::to_string),
                        timestamp(report.completed_at_ms)
                    );
                }
                ("Saved inspections · local", text, Some(cursor + 2))
            }
            ManagementPage::Report { report, warning } => {
                let mut text = report_details(report);
                if let Some(warning) = warning {
                    let _ = writeln!(text, "\nPERSISTENCE WARNING: {}", safe_text(warning));
                }
                (
                    "Inspection · observed facts, not repaired history",
                    text,
                    None,
                )
            }
            _ => unreachable!("page category is selected by the exhaustive renderer"),
        }
    }

    fn selection_content(&self, app: &super::App) -> (&'static str, String, Option<usize>) {
        match &self.page {
            ManagementPage::InspectSelection {
                source,
                historical_releases,
                names,
                selected,
                cursor,
            } => {
                let mut text = format!(
                    "Source: {}\nNo server changes; old outcomes and pending intents remain unchanged.\n\n",
                    source.as_ref().map_or_else(
                        || "current project configuration".into(),
                        ToString::to_string
                    )
                );
                for (index, name) in names.iter().enumerate() {
                    let target = if source.is_some() {
                        historical_releases.iter().find(|release| &release.component == name).map_or_else(
                            || "Historical target unknown; inspection must validate frozen context".into(),
                            |release| format!("Historical target {} revision {} generation {}; requires service validation", release.destination, release.destination_revision.get(), release.generation.get()),
                        )
                    } else {
                        self.scope.config.environments[&self.scope.environment]
                            .components.get(name).map_or_else(
                                || "unavailable in current configuration; inspection will refuse missing context".into(),
                                |target| format!("{} · {}", app.destination_label(&target.destination), safe_text(&target.root)),
                            )
                    };
                    let _ = writeln!(
                        text,
                        "{} [{}] {} · {}",
                        mark(index == *cursor),
                        if selected.contains(name) { 'x' } else { ' ' },
                        name,
                        target
                    );
                }
                if selected.is_empty() {
                    text.push_str("\nSelect at least one Component.\n");
                }
                ("Choose inspection scope", text, Some(cursor + 3))
            }
            ManagementPage::RollbackSelection {
                details,
                selected,
                cursor,
            } => {
                let mut text = format!(
                    "Source Deployment: {}\nSelect only the Components you intend to roll back.\n\n",
                    details.record.deployment
                );
                for (index, snapshot) in details.snapshots.iter().enumerate() {
                    let name = &snapshot.release.component;
                    let _ = writeln!(
                        text,
                        "{} [{}] {} · source version {}",
                        mark(index == *cursor),
                        if selected.contains(name) { 'x' } else { ' ' },
                        name,
                        snapshot.release.version
                    );
                }
                if selected.is_empty() {
                    text.push_str(
                        "\nSelect at least one Component. Nothing is implicitly selected.\n",
                    );
                }
                ("Choose rollback Components", text, Some(cursor + 3))
            }
            _ => unreachable!("page category is selected by the exhaustive renderer"),
        }
    }

    fn rollback_targets_content(&self) -> (&'static str, String, Option<usize>) {
        match &self.page {
            ManagementPage::RollbackTargets {
                candidates,
                selected,
                options,
                cursor,
            } => {
                let mut text = format!(
                    "Source Deployment: {}\nOnly locally proven options are eligible; no server was contacted yet.\n\n",
                    candidates.source
                );
                for (index, component) in candidates.components.iter().enumerate() {
                    let option = component
                        .options
                        .get(*options.get(&component.component).unwrap_or(&0));
                    let target = option.map_or_else(
                        || "no proven target".into(),
                        |option| release_label(option.target.as_ref()),
                    );
                    let _ = writeln!(
                        text,
                        "{} [{}] {} -> {}\n    {} · {}\n    {}",
                        mark(index == *cursor),
                        if selected.contains(&component.component) {
                            'x'
                        } else {
                            ' '
                        },
                        component.component,
                        target,
                        safe_text(&component.destination),
                        safe_text(&component.root),
                        component
                            .unavailable
                            .as_ref()
                            .or_else(|| option.and_then(|option| option.unavailable.as_ref()))
                            .map_or_else(
                                || "Select explicitly, then check current remote state.".into(),
                                |message| format!("UNAVAILABLE: {}", safe_text(message))
                            )
                    );
                }
                ("Choose rollback targets", text, Some(cursor * 3 + 3))
            }
            _ => unreachable!("page category is selected by the exhaustive renderer"),
        }
    }

    fn rollback_execution_content(&self) -> (&'static str, String, Option<usize>) {
        match &self.page {
            ManagementPage::RollbackReview(plan) => {
                let mut text = format!(
                    "CONFIRM ROLLBACK — this changes the selected servers.\nProject: {}\nEnvironment: {}\nSource Deployment: {}\n\n",
                    safe_text(&self.scope.config.project),
                    environment_label(&self.scope.environment),
                    plan.source()
                );
                if is_production(&self.scope.environment) {
                    text.insert_str(0, "[PRODUCTION] This rollback changes the selected live services and may interrupt them.\n");
                }
                for entry in plan.entries() {
                    let _ = writeln!(
                        text,
                        "{}\n  Server: {}\n  Root: {}\n  Current: {}\n  Roll back to: {}\n",
                        entry.component,
                        safe_text(&entry.destination),
                        safe_text(&entry.root),
                        entry.current.version,
                        release_label(entry.target.as_ref())
                    );
                }
                let _ = writeln!(
                    text,
                    "Execution order: {}\n\nPress c to confirm only these Components. Esc rejects the plan.\nRemote drift is checked again before effects. A failed rollback may require compensation.",
                    plan.execution_order()
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(" -> ")
                );
                ("Rollback review", text, None)
            }
            ManagementPage::RollbackFinished(report) => {
                let mut text = format!(
                    "Rollback Deployment: {}\nResult: {:?}\n\n",
                    report.deployment.id, report.deployment.state
                );
                for (name, result) in &report.deployment.components {
                    let _ = writeln!(
                        text,
                        "{name}: {:?}; reported version: {}",
                        result.outcome,
                        result
                            .observed_release
                            .as_ref()
                            .map_or("none reported (not proof of absence)", |version| version
                                .as_str())
                    );
                }
                if report.failure.is_some() {
                    text.push_str("\nRollback failed. Open this Deployment in history for recorded steps and observations.\n");
                }
                for (name, error) in &report.compensation_failures {
                    let _ = writeln!(
                        text,
                        "MANUAL RECOVERY REQUIRED: {name}: {}",
                        crate::tui::deployment_error::driver_error(error)
                    );
                }
                for warning in &report.warnings {
                    let _ = writeln!(text, "WARNING: {}", safe_text(warning));
                }
                ("Rollback result", text, None)
            }
            _ => unreachable!("page category is selected by the exhaustive renderer"),
        }
    }
}

fn page_header(missing: bool, empty: bool, offset: u32, more: bool) -> String {
    if missing {
        return "Local database is missing; historical outcomes are unknown.\nNo database was created.\n".into();
    }
    if empty {
        return "No records in this page for the selected Project / Environment.\n\n".into();
    }
    format!(
        "Local records · page {} · {}\n\n",
        offset / super::PAGE_SIZE + 1,
        if more { "more available" } else { "last page" }
    )
}

fn deployment_details(details: &crate::application::history_query::DeploymentDetails) -> String {
    let record = &details.record;
    let mut text = format!(
        "Deployment: {}\n{:?} / {:?}\nProject: {}  Environment: {}\nStarted: {}\nUpdated: {}\nPending intents: {} (not replayed by inspection)\n",
        record.deployment,
        record.kind,
        record.state,
        record.project,
        record.environment,
        timestamp(record.created_at_ms),
        timestamp(record.updated_at_ms),
        record.pending_intent_count
    );
    if let Some(source) = &record.related_deployment {
        let _ = writeln!(text, "Related Deployment: {source}");
    }
    if let Some(metadata) = &details.metadata {
        let _ = writeln!(
            text,
            "Branch: {}  Commit: {}  Worktree: {:?}\nOperator: {}",
            optional_text(metadata.git_branch.as_deref()),
            optional_text(metadata.git_revision.as_deref()),
            metadata.git_worktree,
            optional_text(metadata.operator.as_deref())
        );
    } else {
        text.push_str("Source and operator metadata: unknown\n");
    }
    text.push_str("\nFrozen Components (no current-config substitution)\n");
    for snapshot in &details.snapshots {
        let release = &snapshot.release;
        let _ = writeln!(
            text,
            "{} · generation {} · destination {} revision {}\n  endpoint fingerprint: {}\n  before: {} -> target: {}",
            release.component,
            release.generation.get(),
            release.destination,
            release.destination_revision.get(),
            String::from(release.endpoint_fingerprint.clone()),
            release_label(snapshot.expected_current.as_ref()),
            release_label(snapshot.target.as_ref())
        );
    }
    if details.snapshots.is_empty() {
        text.push_str("Unknown: no frozen Component plan.\n");
    }
    text.push_str("\nPackages\n");
    for package in &details.packages {
        let _ = writeln!(
            text,
            "{} / {} · {} bytes\n  SHA-256: {}",
            package.release.component, package.release.version, package.size, package.sha256
        );
    }
    text.push_str("\nComponent results\n");
    for item in &details.results {
        let _ = writeln!(
            text,
            "{}: {:?} · version {} · {}",
            item.component,
            item.result.outcome,
            item.result
                .observed_release
                .as_ref()
                .map_or("none reported", |version| version.as_str()),
            optional_text(item.error.as_deref())
        );
    }
    append_execution_details(&mut text, details);
    text
}

fn append_execution_details(
    text: &mut String,
    details: &crate::application::history_query::DeploymentDetails,
) {
    text.push_str("\nSteps\n");
    for step in &details.steps {
        let elapsed = step
            .started_at_ms
            .zip(step.completed_at_ms)
            .map(|(start, end)| end.saturating_sub(start));
        let _ = writeln!(
            text,
            "{} / {}: {:?} · elapsed {}\n  {}",
            step.component,
            step_label(&step.name),
            step.status,
            elapsed.map_or_else(
                || "unknown / ongoing".into(),
                |millis| format!("{millis}ms")
            ),
            optional_text(step.error.as_deref())
        );
    }
    text.push_str("\nObserved facts (empty result fields alone do not establish absence)\n");
    for observation in &details.observations {
        let current = match &observation.observed {
            Ok(None) => "not_deployed (confirmed absent)".into(),
            Ok(Some(release)) => release.version.to_string(),
            Err(error) => format!("UNKNOWN: {}", safe_text(error)),
        };
        let health = match observation.healthy {
            Some(true) => "healthy",
            Some(false) => "unhealthy",
            None => "health unknown",
        };
        let _ = writeln!(
            text,
            "{} / {}: {current}; {health} ({})",
            observation.component,
            step_label(&observation.stage),
            timestamp(observation.observed_at_ms)
        );
    }
    text.push_str("\nPending intents — inspect, do not replay\n");
    for pending in &details.pending {
        let _ = writeln!(
            text,
            "{} / {} · target: {}",
            pending.component,
            step_label(&pending.stage),
            safe_text(&pending.target)
        );
    }
}

fn report_details(report: &crate::history::RecoveryReport) -> String {
    let mut text = format!(
        "Inspection: {}\nCheck started: {}\nCheck completed: {}\nNo commands replayed, services repaired or original outcomes changed.\nVersion equality does not prove health or operation success.\n\n",
        report.id,
        timestamp(report.started_at_ms),
        timestamp(report.completed_at_ms)
    );
    if let Some(source) = &report.related_deployment {
        let _ = writeln!(text, "Source Deployment: {source}\n");
    }
    for component in &report.components {
        let scope = &component.scope;
        let _ = writeln!(
            text,
            "{} · generation {} · destination {} revision {}\n  endpoint fingerprint: {}\n  current alignment: {} · package: {}",
            scope.component,
            scope.generation.get(),
            scope.destination,
            scope.destination_revision.get(),
            String::from(scope.endpoint_fingerprint.clone()),
            alignment(component.alignment),
            package_alignment(component.package_alignment)
        );
        match &component.inventory {
            Err(error) => {
                let _ = writeln!(text, "  UNKNOWN: {}", safe_text(error));
            }
            Ok(inventory) => {
                let current = match &inventory.releases.current {
                    Ok(Some(version)) => version.to_string(),
                    Ok(None) => "not_deployed (confirmed absent)".into(),
                    Err(error) => format!("UNKNOWN: {}", safe_text(error)),
                };
                let _ = writeln!(text, "  Current: {current}");
                for release in &inventory.releases.releases {
                    let _ = writeln!(
                        text,
                        "  {} · {} bytes · {}\n    SHA-256:{}\n    source:{} · created:{}",
                        release.manifest.version,
                        release.size,
                        if release.extracted {
                            "archive + extracted directory"
                        } else {
                            "archive only; extracted directory missing"
                        },
                        release.sha256,
                        optional_text(release.manifest.source_revision.as_deref()),
                        release
                            .manifest
                            .created_at_unix
                            .checked_mul(1000)
                            .map_or_else(|| "timestamp outside supported range".into(), timestamp)
                    );
                }
                for issue in &inventory.releases.issues {
                    let _ = writeln!(
                        text,
                        "  INCOMPLETE {}: {}",
                        issue
                            .version
                            .as_ref()
                            .map_or("unknown version", |version| version.as_str()),
                        safe_text(&issue.message)
                    );
                }
                let _ = writeln!(
                    text,
                    "  Audit: {} entries; {}\n  Temporary remnants: {}; {}",
                    inventory.audit.records.len(),
                    if inventory.audit.incomplete {
                        "incomplete / missing; not proof of success"
                    } else {
                        "complete scan"
                    },
                    inventory.remnants.entries.len(),
                    if inventory.remnants.incomplete {
                        "scan incomplete"
                    } else {
                        "complete scan; nothing removed"
                    }
                );
                for notice in inventory
                    .releases
                    .notices
                    .iter()
                    .chain(&inventory.audit.notices)
                    .chain(&inventory.remnants.notices)
                {
                    let _ = writeln!(text, "  NOTICE: {}", safe_text(notice));
                }
            }
        }
        for notice in &component.notices {
            let _ = writeln!(text, "  NOTICE: {}", safe_text(notice));
        }
        text.push('\n');
    }
    text
}

const fn alignment(value: CurrentAlignment) -> &'static str {
    match value {
        CurrentAlignment::Unplanned => "inventory only",
        CurrentAlignment::Target => "target version",
        CurrentAlignment::Previous => "previous version",
        CurrentAlignment::Other => "other version",
        CurrentAlignment::Unknown => "unknown",
    }
}

const fn package_alignment(value: PackageAlignment) -> &'static str {
    match value {
        PackageAlignment::Unplanned => "inventory only",
        PackageAlignment::Matches => "matches frozen evidence",
        PackageAlignment::ArchiveOnly => "archive only",
        PackageAlignment::Missing => "missing",
        PackageAlignment::Mismatch => "mismatched",
        PackageAlignment::Unknown => "unknown",
    }
}

fn timestamp(millis: u64) -> String {
    time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(millis) * 1_000_000).map_or_else(
        |_| "timestamp outside supported range".into(),
        |value| {
            format!(
                "{} {:02}:{:02}:{:02}.{:03} UTC",
                value.date(),
                value.hour(),
                value.minute(),
                value.second(),
                value.millisecond()
            )
        },
    )
}

fn optional_text(value: Option<&str>) -> String {
    value.map_or_else(|| "unknown / not recorded".into(), safe_text)
}

fn release_label(release: Option<&ReleaseRef>) -> String {
    release.map_or_else(
        || "not_deployed".into(),
        |release| release.version.to_string(),
    )
}

const fn mark(selected: bool) -> &'static str {
    if selected { ">" } else { " " }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ratatui::{Terminal, backend::TestBackend};

    use crate::{
        application::history_query::{DeploymentDetails, HistoryPage},
        domain::{
            ComponentGeneration, ComponentName, DeploymentId, DeploymentState, DestinationKey,
            DestinationRevision, DriverCapabilities, EnvironmentId, ProjectId, ReleaseVersion,
        },
        drivers::{DriverKind, EndpointFingerprint},
        history::{
            DeploymentComponentSnapshot, DeploymentKind, DeploymentRecord, ObservationRecord,
        },
    };

    use super::*;

    fn record() -> DeploymentRecord {
        DeploymentRecord {
            deployment: DeploymentId::new(),
            project: ProjectId::new(),
            environment: EnvironmentId::new(),
            state: DeploymentState::Failed,
            kind: DeploymentKind::Rollback,
            related_deployment: None,
            created_at_ms: 1,
            updated_at_ms: 2,
            pending_intent_count: 0,
        }
    }

    fn details() -> DeploymentDetails {
        let record = record();
        let release = ReleaseRef {
            driver: DriverKind::linux_ssh(),
            project_id: record.project.clone(),
            environment_id: record.environment.clone(),
            component: ComponentName::parse("api").unwrap(),
            generation: ComponentGeneration::INITIAL,
            version: ReleaseVersion::parse("v1.2").unwrap(),
            destination: DestinationKey::new(),
            destination_revision: DestinationRevision::INITIAL,
            endpoint_fingerprint: EndpointFingerprint::parse("a".repeat(64)).unwrap(),
            effective_capabilities: DriverCapabilities::new([]),
        };
        DeploymentDetails {
            record,
            metadata: None,
            snapshots: vec![DeploymentComponentSnapshot {
                release: release.clone(),
                expected_current: Some(release),
                target: None,
                execution_order: 0,
            }],
            results: Vec::new(),
            steps: Vec::new(),
            observations: Vec::new(),
            pending: Vec::new(),
            packages: Vec::new(),
            receipts: Vec::new(),
            log: None,
        }
    }

    #[test]
    fn planned_absence_is_not_an_observation_and_unknown_is_not_absence() {
        let mut details = details();
        let plan = deployment_details(&details);
        assert!(plan.contains("target: not_deployed"));
        assert!(!plan.contains("confirmed absent"));
        let name = ComponentName::parse("api").unwrap();
        details.observations = vec![
            ObservationRecord {
                sequence: 1,
                component: name.clone(),
                stage: "before".into(),
                observed: Ok(None),
                healthy: None,
                observed_at_ms: 1,
            },
            ObservationRecord {
                sequence: 2,
                component: name,
                stage: "after".into(),
                observed: Err("connection lost".into()),
                healthy: None,
                observed_at_ms: 2,
            },
        ];
        let text = deployment_details(&details);
        assert!(text.contains("api / before: not_deployed (confirmed absent); health unknown"));
        assert!(text.contains("api / after: UNKNOWN: connection lost; health unknown"));
        assert_eq!(text.matches("confirmed absent").count(), 1);
    }

    #[test]
    fn cleanup_step_keeps_its_action_and_complete_dotted_version() {
        assert_eq!(step_label("cleanup.v1.2"), "Cleanup v1.2");
        assert_eq!(step_label("rollback"), "Rolling back");
        assert_eq!(step_label("compensate"), "Compensating changes");
        assert_eq!(step_label("linux-ssh.uploading"), "Uploading");
        assert_eq!(step_label("build.packaging"), "Packaging");
        assert_eq!(step_label("unknown.action"), "unknown.action");
    }

    #[test]
    fn management_help_keeps_escape_visible_in_eighty_columns() {
        let (_directory, app) = super::super::tests::fixture();
        let mut screen = super::super::tests::screen(&app);
        let mut local_only = details();
        local_only.snapshots.clear();
        for page in [
            ManagementPage::Home,
            ManagementPage::Detail(std::sync::Arc::new(details())),
            ManagementPage::Detail(std::sync::Arc::new(local_only)),
        ] {
            screen.page = page;
            for historical in [None, Some(crate::domain::EnvironmentId::new())] {
                std::sync::Arc::make_mut(&mut screen.scope).historical_environment = historical;
                assert!(screen.help().starts_with("Esc "));
                assert!(screen.help().chars().count() <= 80, "{}", screen.help());
            }
        }
    }

    #[test]
    fn historical_inspection_targets_never_borrow_current_connection_labels() {
        use crate::config::{DestinationRegistry, DestinationSettings, HostKeyFingerprint};
        let (directory, mut app) = super::super::tests::fixture();
        let mut screen = super::super::tests::screen(&app);
        let component = ComponentName::parse("frontend").unwrap();
        let destination = screen.scope.config.environments[&screen.scope.environment].components
            [&component]
            .destination
            .clone();
        let mut connections = DestinationRegistry::new();
        connections
            .create(
                destination.clone(),
                DestinationSettings::LinuxSsh {
                    host: "current-endpoint.invalid".into(),
                    user: "deploy".into(),
                    port: 22,
                    credential: crate::drivers::CredentialHandle::new(),
                    host_key: HostKeyFingerprint::parse("SHA256:fixture").unwrap(),
                },
            )
            .unwrap();
        connections
            .save(&directory.path().join("destinations.yaml"))
            .unwrap();
        app.refresh_destination_labels();
        let mut release = details().snapshots[0].release.clone();
        release.component = component.clone();
        release.destination = destination;
        screen.page = ManagementPage::InspectSelection {
            source: None,
            historical_releases: Vec::new(),
            names: vec![component.clone()],
            selected: [component.clone()].into_iter().collect(),
            cursor: 0,
        };
        assert!(
            screen
                .selection_content(&app)
                .1
                .contains("current-endpoint.invalid")
        );
        screen.page = ManagementPage::InspectSelection {
            source: Some(DeploymentId::new()),
            historical_releases: vec![release],
            names: vec![component.clone()],
            selected: [component].into_iter().collect(),
            cursor: 0,
        };
        let text = screen.selection_content(&app).1;
        assert!(text.contains("Historical target") && text.contains("requires service validation"));
        assert!(!text.contains("current-endpoint.invalid"));
        assert!(screen.context_label().contains("frontend"));
    }

    #[test]
    fn historical_context_never_classifies_an_opaque_environment_id_as_production() {
        let (_directory, app) = super::super::tests::fixture();
        let mut screen = super::super::tests::screen(&app);
        Arc::make_mut(&mut screen.scope).historical_environment =
            Some("env_prodOpaqueId".parse().unwrap());
        let context = screen.context_label();
        assert!(context.contains("read-only historical env_prodOpaqueId"));
        assert!(!context.contains("[PRODUCTION]"));
    }

    #[test]
    fn technical_step_metadata_is_translated_without_changing_persisted_records() {
        let mut details = details();
        details.observations.push(ObservationRecord {
            sequence: 1,
            component: ComponentName::parse("api").unwrap(),
            stage: "linux-ssh.upload".into(),
            observed: Ok(None),
            healthy: None,
            observed_at_ms: 1,
        });
        let original = details.clone();
        let text = deployment_details(&details);
        assert!(text.contains("Uploading"));
        assert!(!text.contains("linux-ssh"));
        assert_eq!(details, original);
    }

    #[test]
    fn narrow_history_list_keeps_the_selected_last_row_on_screen() {
        let (_directory, app) = super::super::tests::fixture();
        let mut screen = super::super::tests::screen(&app);
        let items: Vec<_> = (0..20).map(|_| record()).collect();
        let selected = items[19].deployment.to_string();
        screen.page = ManagementPage::History {
            page: Arc::new(HistoryPage {
                items,
                more: false,
                database_missing: false,
            }),
            offset: 0,
            cursor: 19,
        };
        let mut terminal = Terminal::new(TestBackend::new(80, 10)).unwrap();
        terminal
            .draw(|frame| screen.render(frame, frame.area(), &app))
            .unwrap();
        let buffer: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(buffer.contains(&format!("> {selected}")));
    }

    #[test]
    fn times_are_explicit_utc_and_invalid_extremes_are_not_wrapped() {
        assert_eq!(timestamp(0), "1970-01-01 00:00:00.000 UTC");
        assert_eq!(timestamp(86_400_123), "1970-01-02 00:00:00.123 UTC");
        assert_eq!(timestamp(u64::MAX), "timestamp outside supported range");
    }

    #[test]
    fn long_diagnostics_mark_display_truncation() {
        let text = safe_text(&"x".repeat(4097));
        assert!(text.ends_with("[display truncated]"));
        assert!(text.len() < 4200);
    }
}
