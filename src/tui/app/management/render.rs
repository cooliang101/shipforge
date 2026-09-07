use std::fmt::Write as _;

use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Style},
    widgets::{Block, Borders, Paragraph, Wrap},
};

use crate::tui::presentation::{context_label, environment_label, is_production, step_label};

use super::evidence::{deployment_details, release_label, report_details, timestamp};
use super::{ManagementPage, ManagementScreen};
pub(super) use crate::tui::presentation::safe_text;

impl ManagementScreen {
    pub(in crate::tui) fn help(&self) -> &'static str {
        if self.historical_remote_page() {
            return "Esc back  Read-only historical scope; remote operations unavailable";
        }
        match &self.page {
            ManagementPage::Home if self.scope.historical_environment.is_some() => {
                "Esc back  h history  p saved inspections  (read-only)"
            }
            ManagementPage::Home => {
                "Esc overview  ←/→ env  h history  i inspect  p reports  a old envs"
            }
            ManagementPage::Loading {
                cancelling: true, ..
            } => "Cancellation requested; waiting for a safe result. Do not close the terminal.",
            ManagementPage::Loading { .. } => {
                "Esc / Ctrl+C request cancellation; wait for a safe result"
            }
            ManagementPage::Environments { .. }
            | ManagementPage::History { .. }
            | ManagementPage::Reports { .. } => {
                "Esc back  ↑/↓ select  Enter details  n/b page  f refresh  [/] pan"
            }
            ManagementPage::Detail(_) if self.scope.historical_environment.is_some() => {
                "Esc back  ↑/↓ PgUp/PgDn scroll  [/] pan  l logs  (read-only)"
            }
            ManagementPage::Detail(details) if details.snapshots.is_empty() => {
                "Esc back  ↑/↓ PgUp/PgDn scroll  [/] pan  l logs  (no remote context)"
            }
            ManagementPage::Detail(_) => {
                "Esc back  ↑/↓ PgUp/PgDn scroll  [/] pan  l logs  r rollback  i inspect"
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
                "Esc back  ↑/↓ Component  ←/→ version  Space select  d details  [/] pan"
            }
            ManagementPage::RollbackTargets { .. } => {
                "Esc back  ↑/↓ Component  ←/→ version  Space select  Enter check  d details"
            }
            ManagementPage::RollbackReview(_) => {
                "Esc reject  ↑/↓ PgUp/PgDn scroll  [/] pan  c confirm rollback"
            }
            ManagementPage::RollbackFinished(_) | ManagementPage::RollbackFailed { .. } => {
                "Esc back  ↑/↓ PgUp/PgDn scroll  [/] pan  l this rollback's logs"
            }
            ManagementPage::Report { .. } | ManagementPage::RollbackTargetDetail { .. } => {
                "Esc back  ↑/↓ PgUp/PgDn scroll  Home/End top/bottom  [/] pan  0 left"
            }
            ManagementPage::Failed { retry: Some(_), .. } => {
                "Esc back to snapshot  f retry check  ↑/↓ scroll  [/] pan"
            }
            ManagementPage::Failed { retry: None, .. } => {
                "Esc back to snapshot  ↑/↓ scroll  [/] pan  review before retrying"
            }
        }
    }

    fn historical_remote_page(&self) -> bool {
        self.scope.historical_environment.is_some()
            && matches!(
                self.page,
                ManagementPage::InspectSelection { .. }
                    | ManagementPage::RollbackSelection { .. }
                    | ManagementPage::RollbackTargets { .. }
                    | ManagementPage::RollbackTargetDetail { .. }
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
            ManagementPage::RollbackSelection { .. }
            | ManagementPage::RollbackTargets { .. }
            | ManagementPage::RollbackTargetDetail { .. } => "Choose rollback targets",
            ManagementPage::RollbackReview(_) => "Confirm rollback",
            ManagementPage::RollbackFinished(_) => "Rollback result",
            ManagementPage::RollbackFailed { .. } => "Rollback did not complete normally",
            ManagementPage::Loading { label, .. } | ManagementPage::Failed { label, .. } => label,
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
            }
            | ManagementPage::RollbackTargetDetail {
                candidates, cursor, ..
            } => candidates
                .components
                .get(*cursor)
                .map(|component| component.component.as_str()),
            _ => None,
        }
    }

    pub(in crate::tui) fn render(&self, frame: &mut Frame<'_>, area: Rect, app: &super::App) {
        let historical_blocked = self.historical_remote_page();
        let cached = !historical_blocked
            && matches!(
                self.page,
                ManagementPage::Detail(_)
                    | ManagementPage::Report { .. }
                    | ManagementPage::RollbackReview(_)
                    | ManagementPage::RollbackFinished(_)
                    | ManagementPage::RollbackFailed { .. }
                    | ManagementPage::RollbackTargetDetail { .. }
                    | ManagementPage::Failed { .. }
            );
        let dynamic;
        let (document, cursor) = if cached {
            (self.view.document(|| self.content(app)), None)
        } else {
            let (title, body, cursor) = self.content(app);
            dynamic = super::viewport::Document::new(title, body);
            (&dynamic, cursor)
        };
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
            document.title,
            safe_text(&self.scope.config.project),
            safe_text(&environment),
        );
        let block = Block::default()
            .title(heading)
            .title_bottom(if cached {
                format!(
                    " line {}/{} · [/] pan · ,/. fine · col {} ",
                    self.view.top(document).saturating_add(1),
                    document.lines(),
                    self.view.horizontal().saturating_add(1)
                )
            } else if cursor.is_some() {
                format!(
                    " [/] pan · ,/. fine · col {} · 0 left ",
                    self.view.horizontal().saturating_add(1)
                )
            } else {
                String::new()
            })
            .borders(Borders::ALL)
            .border_style(if production {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default()
            });
        let mut inner = block.inner(area);
        frame.render_widget(block, area);
        if let Some(notice) = &self.notice {
            let [banner, body] =
                Layout::vertical([Constraint::Length(2), Constraint::Min(0)]).areas(inner);
            frame.render_widget(
                Paragraph::new(safe_text(notice))
                    .style(Style::default().fg(Color::Yellow))
                    .wrap(Wrap { trim: false }),
                banner,
            );
            inner = body;
        }
        let top = if historical_blocked {
            0
        } else {
            cursor.map_or_else(
                || self.view.top(document),
                |cursor| {
                    let visible = usize::from(inner.height.saturating_sub(2)).max(1);
                    cursor.saturating_sub(visible.saturating_sub(1))
                },
            )
        };
        let horizontal = if historical_blocked {
            0
        } else {
            self.view.horizontal()
        };
        if !cached && cursor.is_none() && horizontal == 0 {
            // Short overview/loading text remains wrapped. Evidence and lists
            // use logical-line windows, so every row and long tail is reachable.
            frame.render_widget(
                Paragraph::new(document.window(top, 0, inner.height, u16::MAX))
                    .wrap(Wrap { trim: false }),
                inner,
            );
        } else {
            frame.render_widget(
                Paragraph::new(document.window(top, horizontal, inner.height, inner.width)),
                inner,
            );
        }
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
            return Self::live_rollback_content(app, progress, *cancelling);
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
            ManagementPage::RollbackTargetDetail { .. } => self.rollback_target_detail(),
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
            ManagementPage::Failed {
                label,
                message,
                retry,
            } => (
                "Request failed · no fresh result",
                format!(
                    "{label}\n\n{}\n\nNo fresh result is available. Esc returns to the prior snapshot, which is not a new check.\n{}",
                    safe_text(message),
                    if retry.is_some() {
                        "Press f to run this read/check again. No rollback execution is retried here."
                    } else {
                        "No automatic retry is offered. Review saved reports and current state before starting another inspection."
                    }
                ),
                None,
            ),
        }
    }

    fn live_rollback_content(
        app: &super::App,
        progress: &crate::tui::live_progress::LiveProgress,
        cancelling: bool,
    ) -> (&'static str, String, Option<usize>) {
        let snapshot = progress.snapshot();
        let mut text = format!(
            "Rollback running · elapsed {} ms\n{}\nl opens logs, full step progress, search and export.\n\n",
            snapshot.elapsed_ms,
            if cancelling {
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
        if snapshot.dropped_rows > 0 || snapshot.dropped_steps > 0 || snapshot.rejected_events > 0 {
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
        ("Rollback progress", text, None)
    }

    fn overview_content(&self) -> (&'static str, String, Option<usize>) {
        match &self.page {
            ManagementPage::Home if self.scope.historical_environment.is_some() => (
                "Historical Environment · read-only",
                format!(
                    "Project directory: {}\nEnvironment: {}\n\n[h] Local deployment history and logs\n[p] Saved inspection reports\n\nRead-only local evidence; no remote connections or rollback.\nEnvironment identity, not its name, determines this scope.\nOld configuration is not reconstructed. Esc returns to the previous page.",
                    safe_text(&crate::tui::presentation::path_label(&self.scope.root)),
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
                    "Project directory: {}\n\n[h] Local deployment history and logs\n[i] Inspect selected Components / remote Releases\n[p] Saved inspection reports\n[a] Historical Environment IDs (including removed Environments)\n\nOpening history never connects to a server.\nInspection is read-only on the server; it saves a separate local report.\nInventory is not an environment preflight or a health check.\nIt does not prove service health or historical success.",
                    safe_text(&crate::tui::presentation::path_label(&self.scope.root))
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
                        "{} {:?} / {:?} · {} · pending:{}\n  {}",
                        mark(index == *cursor),
                        record.kind,
                        record.state,
                        timestamp(record.created_at_ms),
                        record.pending_intent_count,
                        record.deployment
                    );
                }
                (
                    "Deployment history · local · created time",
                    text,
                    Some(cursor * 2 + 2),
                )
            }
            ManagementPage::Detail(details) => {
                let mut text = deployment_details(details);
                if details.snapshots.is_empty() {
                    text.insert_str(0, &format!("{}\n\n", super::MISSING_FROZEN_CONTEXT));
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
                        "{} {} · {} Components · {}\n  {}",
                        mark(index == *cursor),
                        timestamp(report.completed_at_ms),
                        report.components.len(),
                        if report.related_deployment.is_some() {
                            "Deployment comparison"
                        } else {
                            "inventory only"
                        },
                        report.id
                    );
                }
                (
                    "Saved inspections · local · checked time",
                    text,
                    Some(cursor * 2 + 2),
                )
            }
            ManagementPage::Report { report, warning } => {
                let mut text = warning.as_ref().map_or_else(
                    || "Saved inspection report · historical observations, not live health.\n\n".into(),
                    |warning| format!("SAVE UNCONFIRMED: observations retained on this page.\nPERSISTENCE WARNING: {}\n\n", safe_text(warning)),
                );
                text.push_str(&report_details(report));
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
            ManagementPage::RollbackFinished(report) => (
                "Rollback result",
                super::outcome::rollback_result(report),
                None,
            ),
            _ => unreachable!("page category is selected by the exhaustive renderer"),
        }
    }

    fn rollback_target_detail(&self) -> (&'static str, String, Option<usize>) {
        let ManagementPage::RollbackTargetDetail {
            candidates,
            cursor,
            option,
        } = &self.page
        else {
            unreachable!("only candidate detail is routed here");
        };
        let Some(component) = candidates.components.get(*cursor) else {
            return (
                "Rollback candidate · local evidence",
                "No candidate recorded. Return and refresh the Component selection.".into(),
                None,
            );
        };
        let target = component.options.get(*option);
        let reason = component
            .unavailable
            .as_ref()
            .or_else(|| target.and_then(|target| target.unavailable.as_ref()));
        let text = format!(
            "Component: {}\nSource Deployment: {}\nServer: {}\nRoot: {}\nOption: {} of {}\nTarget: {}\nAvailability: {}\n\nLocal evidence only; current remote state has not been checked.\nThis page does not select or execute a target. Esc preserves the selection.\nUse [/] to pan long paths and messages; 0 returns to the left edge.",
            component.component,
            candidates.source,
            safe_text(&component.destination),
            safe_text(&component.root),
            option.saturating_add(1),
            component.options.len(),
            target.map_or_else(
                || "unknown / no proven option".into(),
                |target| release_label(target.target.as_ref())
            ),
            reason.map_or_else(
                || if target.is_some() {
                    "eligible for explicit selection and a fresh remote check".into()
                } else {
                    "unavailable: no proven option".into()
                },
                |reason| format!("UNAVAILABLE: {}", safe_text(reason))
            ),
        );
        ("Rollback candidate · local evidence", text, None)
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
        drivers::{DriverKind, EndpointFingerprint, ReleaseRef},
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
                target_snapshot: None,
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
        let (_directory, mut app) = super::super::tests::fixture();
        let mut screen = super::super::tests::screen(&app);
        let items: Vec<_> = (0..20)
            .map(|index| {
                let mut item = record();
                item.created_at_ms = index * 86_400_000;
                item
            })
            .collect();
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
        app.screen = super::super::Screen::Management(screen);
        let buffer = render_app(&app, 80, 10);
        assert!(buffer.contains(&selected));
        assert!(buffer.contains("> Rollback / Failed"));
        assert!(buffer.contains("1970-01-20"));
        assert!(buffer.contains("pending:0"));
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

    fn render_app(app: &super::super::App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| crate::tui::render(frame, app))
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect()
    }

    #[test]
    fn saved_report_warning_precedes_long_observations_in_real_small_viewport() {
        let (_directory, mut app) = super::super::tests::fixture();
        let mut screen = super::super::tests::screen(&app);
        let report = Arc::new(crate::history::RecoveryReport {
            id: uuid::Uuid::now_v7(),
            related_deployment: None,
            source_revision: None,
            started_at_ms: 1,
            completed_at_ms: 2,
            components: (0..32)
                .map(|index| {
                    let mut scope =
                        crate::history::InspectionScope::from(&details().snapshots[0].release);
                    scope.component = ComponentName::parse(format!("service-{index}")).unwrap();
                    crate::history::RecoveryComponentReport {
                        scope,
                        inventory: Err("No observation obtained".into()),
                        alignment: crate::history::CurrentAlignment::Unknown,
                        package_alignment: crate::history::PackageAlignment::Unknown,
                        notices: (0..32)
                            .map(|notice| format!("Recorded notice {notice}"))
                            .collect(),
                    }
                })
                .collect(),
        });
        screen.page = ManagementPage::Report {
            report: Arc::clone(&report),
            warning: Some("Local report persistence failed; observed facts retained.".into()),
        };
        app.screen = super::super::Screen::Management(screen);
        let buffer = render_app(&app, 80, 10);
        assert!(buffer.contains("SAVE UNCONFIRMED: observations retained on this page."));
        assert!(buffer.contains("PERSISTENCE WARNING"));
        assert!(!buffer.contains("Saved inspection report"));
        let mut screen = super::super::tests::screen(&app);
        screen.page = ManagementPage::Report {
            report,
            warning: None,
        };
        screen.view = super::super::viewport::Viewport::default();
        app.screen = super::super::Screen::Management(screen);
        assert!(render_app(&app, 80, 10).contains("Saved inspection report"));
    }

    #[test]
    fn selected_rollback_candidate_has_complete_pannable_details_without_execution() {
        use crate::application::{RollbackComponentCandidates, RollbackOption};
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let (_directory, mut app) = super::super::tests::fixture();
        let mut screen = super::super::tests::screen(&app);
        let name = ComponentName::parse("c".repeat(63)).unwrap();
        let mut release = details().snapshots[0].release.clone();
        release.component = name.clone();
        release.version = ReleaseVersion::parse("v-same-prefix-target-B").unwrap();
        screen.page = ManagementPage::RollbackTargets {
            candidates: Arc::new(crate::application::RollbackCandidates {
                source: DeploymentId::new(),
                components: vec![RollbackComponentCandidates {
                    component: name,
                    destination: format!("{}endpoint-tail", "server".repeat(25)),
                    root: format!("/srv/{}/root-tail", "x".repeat(100)),
                    options: vec![RollbackOption {
                        target: Some(release),
                        unavailable: Some(format!("{}REASON-TAIL", "x".repeat(160))),
                    }],
                    unavailable: None,
                }],
            }),
            selected: std::collections::BTreeSet::default(),
            options: std::collections::BTreeMap::default(),
            cursor: 0,
        };
        app.screen = super::super::Screen::Management(screen);
        app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE));
        let _ = render_app(&app, 80, 10);
        assert!(matches!(
            super::super::tests::screen(&app).page,
            ManagementPage::RollbackTargetDetail { .. }
        ));
        app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE));
        assert!(app.management_task.is_none());
        for _ in 0..5 {
            app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        }
        assert!(render_app(&app, 80, 10).contains("Target: v-same-prefix-target-B"));
        for _ in 0..4 {
            app.handle_key(KeyEvent::new(KeyCode::Char(']'), KeyModifiers::NONE));
        }
        assert!(render_app(&app, 80, 10).contains("REASON-TAIL"));
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(matches!(
            super::super::tests::screen(&app).page,
            ManagementPage::RollbackTargets { cursor: 0, .. }
        ));
    }

    #[test]
    fn management_details_render_without_panicking_in_tiny_and_empty_terminals() {
        let (_directory, mut app) = super::super::tests::fixture();
        let mut screen = super::super::tests::screen(&app);
        screen.page = ManagementPage::Detail(Arc::new(details()));
        screen.notice = Some("Cancelled; retained snapshot is not a fresh check.".into());
        app.screen = super::super::Screen::Management(screen);
        for (width, height) in [(0, 0), (1, 1), (5, 2), (10, 4), (80, 10)] {
            let _ = render_app(&app, width, height);
        }
    }

    #[test]
    fn rollback_failure_warning_and_manual_recovery_counts_are_visible_at_eighty_by_ten() {
        use crate::{
            application::{DeploymentFailure, OrchestrationStage, RollbackReport},
            domain::Deployment,
            drivers::DriverError,
        };
        let (_directory, mut app) = super::super::tests::fixture();
        let mut screen = super::super::tests::screen(&app);
        let mut deployment = Deployment::new();
        deployment.state = DeploymentState::Failed;
        let error = DriverError {
            recovery_blocked: false,
            stage: "linux-ssh.health".into(),
            target: "private-error-target".into(),
            message: "private-error-message".into(),
            suggested_action: "private-error-advice".into(),
        };
        let name = ComponentName::parse("api").unwrap();
        screen.page = ManagementPage::RollbackFinished(Arc::new(RollbackReport {
            deployment,
            failure: Some(DeploymentFailure::Driver { component: name.clone(), stage: OrchestrationStage::Rollback, error: error.clone(), observed_release: None }),
            compensation_failures: [(name, error)].into_iter().collect(),
            warnings: vec!["persist Rollback terminal state: local history persistence failed; inspect durable history before retrying".into()],
        }));
        app.screen = super::super::Screen::Management(screen);
        let buffer = render_app(&app, 80, 10);
        assert!(buffer.contains("Result: Failed"));
        assert!(buffer.contains("warnings:1"));
        assert!(buffer.contains("manual recovery:1"));
        assert!(buffer.contains("Main failure: api"));
        assert!(!buffer.contains("private-error"));
    }
}
