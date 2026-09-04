mod app;
mod deployment_error;
mod presentation;

use std::{
    fmt::Write as _,
    io::{self, BufWriter, Stdout},
    time::Duration,
};

use crossterm::{
    event::{self, Event, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
};

use self::app::{App, Screen};
use self::presentation::{endpoint_label, environment_label, is_production, safe_text, step_label};

type AppTerminal = Terminal<CrosstermBackend<BufWriter<Stdout>>>;

struct TerminalGuard {
    terminal: AppTerminal,
}

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        let mut output = BufWriter::new(io::stdout());
        if let Err(error) = execute!(output, EnterAlternateScreen) {
            let _ = disable_raw_mode();
            return Err(error);
        }

        let backend = CrosstermBackend::new(output);
        match Terminal::new(backend) {
            Ok(terminal) => Ok(Self { terminal }),
            Err(error) => {
                let _ = disable_raw_mode();
                let _ = execute!(io::stdout(), LeaveAlternateScreen);
                Err(error)
            }
        }
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen);
        let _ = self.terminal.show_cursor();
    }
}

/// Runs the MVP terminal shell. Press `q` or `Esc` to exit.
///
/// # Errors
///
/// Returns an error when terminal setup, rendering, or input handling fails.
pub fn run() -> io::Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| io::Error::other(error.to_string()))?;
    let _runtime_context = runtime.enter();
    run_event_loop()
}

fn run_event_loop() -> io::Result<()> {
    let registry_path = crate::projects::default_registry_path()
        .map_err(|error| io::Error::other(error.to_string()))?;
    let destination_registry_path = crate::config::default_destination_registry_path()
        .map_err(|error| io::Error::other(error.to_string()))?;
    let initial_directory = std::env::current_dir()?;
    let mut app = App::new(registry_path, destination_registry_path, &initial_directory)
        .map_err(|error| io::Error::other(error.to_string()))?;
    let mut guard = TerminalGuard::enter()?;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        drive_event_loop(&mut app, &mut guard)
    }));
    // Keep the runtime alive until safe cancellation finishes even if terminal
    // drawing or input fails. A broken terminal must not silently detach work.
    drop(guard);
    app.shutdown();
    match result {
        Ok(result) => result,
        Err(payload) => std::panic::resume_unwind(payload),
    }
}

fn drive_event_loop(app: &mut App, guard: &mut TerminalGuard) -> io::Result<()> {
    loop {
        app.poll_background();
        guard.terminal.draw(|frame| render(frame, app))?;

        if event::poll(Duration::from_millis(250))?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
            && app.handle_key(key)
        {
            break;
        }
    }

    Ok(())
}

fn render(frame: &mut Frame<'_>, app: &App) {
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(2),
        ])
        .split(frame.area());
    let context = app.context_label();
    let context_style =
        Style::default()
            .add_modifier(Modifier::BOLD)
            .fg(if context.starts_with("[PRODUCTION]") {
                Color::Yellow
            } else {
                Color::Cyan
            });
    frame.render_widget(Paragraph::new(context).style(context_style), areas[0]);
    render_screen(frame, areas[1], app);
    let help = if app.deployment_session.is_active()
        && !matches!(
            app.screen,
            Screen::Management(_)
                | Screen::Connections(_)
                | Screen::ProjectEdit(_)
                | Screen::DeploymentRunning { .. }
        ) {
        "Deployment active   return to progress or cancel safely"
    } else {
        match &app.screen {
            Screen::Management(screen) => screen.help(),
            Screen::Connections(screen) => screen.help(),
            Screen::ProjectEdit(screen) => screen.help(),
            Screen::Projects => {
                "↑/↓ select   Enter open   o browse   c connections   x unregister project   q quit"
            }
            Screen::Browser(_) => {
                "↑/↓ select   Enter enter directory   Backspace parent   s select root   Esc back"
            }
            Screen::Overview { .. } => {
                "←/→ env   ↑/↓ scroll   d deploy   m manage   e edit   Esc projects   q quit"
            }
            Screen::DeploySelection(selection) if selection.selected.is_empty() => {
                "↑/↓ Component   Space select (required)   ←/→ Environment   Esc overview"
            }
            Screen::DeploySelection(_) => {
                "←/→ Environment   ↑/↓ Component   Space toggle   Enter check   Esc overview"
            }
            Screen::DeploymentPlanning { .. } => "Checking local and remote state…   Esc cancel",
            Screen::DeploymentReview { .. } => {
                "↑/↓ or PgUp/PgDn scroll   c confirm Deployment   Esc change selection"
            }
            Screen::DeploymentRunning {
                cancellation_requested,
                ..
            } => {
                if *cancellation_requested {
                    "Safe cancellation requested; recovery may still be running"
                } else {
                    "Deployment running   Esc request safe cancellation"
                }
            }
            Screen::DeploymentFinished { .. } => {
                "↑/↓ or PgUp/PgDn scroll   Enter/Esc project overview"
            }
            Screen::SetupComponents(_) => "↑/↓ select   Space toggle   Enter next   Esc projects",
            Screen::SetupDestinations(_) => {
                "←/→ Component   ↑/↓ Destination   Space assign   a add SSH   n review   Esc Components"
            }
            Screen::NewSshDestination(_) => {
                "Tab field   ↑/↓ identity   F3 browse key   F2 next host   Enter probe   Esc cancel"
            }
            Screen::HostKeyPending { .. } => "Fetching Host Key…   Esc cancel",
            Screen::HostKeyConfirm { .. } => "y trust fingerprint and authenticate   n/Esc reject",
            Screen::SshAuthenticationPending { .. } => "Authenticating and probing…   Esc cancel",
            Screen::RemoteSetupSelection(_) => {
                "↑/↓ select systemd service   Enter accept   Esc skip service"
            }
            Screen::KeyBrowser { .. } => {
                "↑/↓ select   Enter directory   Backspace parent   s choose file   Esc back"
            }
            Screen::SetupReview { .. } => {
                "↑/↓ or PgUp/PgDn scroll   c confirm and save   Esc Destinations"
            }
        }
    };
    let mut footer = vec![Line::from(help)];
    if let Some(message) = &app.message {
        footer.push(Line::from(vec![
            Span::styled(
                "Message: ",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(safe_text(message)),
        ]));
    }
    frame.render_widget(Paragraph::new(footer), areas[2]);
}

fn render_screen(frame: &mut Frame<'_>, area: ratatui::layout::Rect, app: &App) {
    match &app.screen {
        Screen::Management(screen) => screen.render(frame, area, app),
        Screen::Connections(screen) => screen.render(frame, area),
        Screen::ProjectEdit(screen) => screen.render(frame, area),
        Screen::Projects => render_projects(frame, area, app),
        Screen::Browser(browser) => render_project_browser(frame, area, browser),
        Screen::Overview { root, config } => {
            let mut content = format!(
                "Project: {}\nRoot: {}\nComponents: {}\nEnvironments: {}\n\n{}",
                safe_text(&config.project),
                safe_text(&root.display().to_string()),
                config.components.len(),
                config.environments.len(),
                app.overview_targets(config)
            );
            if let Some(notice) = app.attention_notice() {
                let _ = write!(content, "\n\n{notice}");
            }
            frame.render_widget(
                panel(" Project overview ", content).scroll((app.overview_scroll(), 0)),
                area,
            );
        }
        Screen::DeploySelection(selection) => render_deploy_selection(frame, area, selection, app),
        Screen::DeploymentPlanning { selection, .. } => {
            render_deployment_planning(frame, area, selection);
        }
        Screen::DeploymentReview { plan, scroll } => {
            render_deployment_review(frame, area, plan, *scroll);
        }
        Screen::DeploymentRunning {
            logs,
            cancellation_requested,
            ..
        } => render_deployment_running(frame, area, logs, *cancellation_requested),
        Screen::DeploymentFinished {
            summary,
            logs,
            scroll,
            ..
        } => {
            render_deployment_finished(frame, area, summary, logs, *scroll);
        }
        Screen::SetupComponents(setup) => {
            render_component_setup(frame, area, setup);
        }
        Screen::SetupDestinations(setup) => {
            render_destination_setup(frame, area, setup);
        }
        Screen::NewSshDestination(draft) => {
            render_new_ssh_destination(frame, area, draft);
        }
        Screen::HostKeyPending { draft, .. } => render_host_key_pending(frame, area, draft),
        Screen::HostKeyConfirm { draft, fingerprint } => {
            render_host_key_confirmation(frame, area, draft, fingerprint);
        }
        Screen::SshAuthenticationPending { draft, .. } => {
            render_authentication_pending(frame, area, draft);
        }
        Screen::RemoteSetupSelection(selection) => {
            render_remote_setup_selection(frame, area, selection);
        }
        Screen::KeyBrowser { browser, .. } => render_key_browser(frame, area, browser),
        Screen::SetupReview {
            prepared, scroll, ..
        } => {
            frame.render_widget(
                Paragraph::new(prepared.preview().to_owned())
                    .block(
                        Block::default()
                            .title(" First-time setup · Review shipforge.yaml ")
                            .borders(Borders::ALL),
                    )
                    .scroll((*scroll, 0))
                    .wrap(Wrap { trim: false }),
                area,
            );
        }
    }
}

fn render_project_browser(
    frame: &mut Frame<'_>,
    area: ratatui::layout::Rect,
    browser: &app::DirectoryBrowser,
) {
    let mut lines = vec![Line::from(format!(
        "Current: {}",
        browser.directory.display()
    ))];
    if browser.children.is_empty() {
        lines.push(Line::from("  No child directories"));
    } else {
        lines.extend(browser.children.iter().enumerate().map(|(index, path)| {
            selected_line(
                index == browser.selected,
                &path.file_name().map_or_else(
                    || path.display().to_string(),
                    |name| name.to_string_lossy().into(),
                ),
            )
        }));
    }
    let content = Paragraph::new(lines)
        .block(
            Block::default()
                .title(" Select project directory ")
                .borders(Borders::ALL),
        )
        .wrap(Wrap { trim: false });
    frame.render_widget(content, area);
}

fn render_deploy_selection(
    frame: &mut Frame<'_>,
    area: ratatui::layout::Rect,
    selection: &app::DeploySelectionState,
    app: &App,
) {
    let environments = app::environment_names(selection);
    let components = app::deployment_components(selection);
    let environment = environments
        .get(selection.environment_cursor)
        .map_or("none", String::as_str);
    let mut lines = vec![
        Line::from(format!("Project: {}", safe_text(&selection.config.project))),
        Line::from(format!("Environment: {}", environment_label(environment))),
        Line::from(""),
    ];
    for (index, component) in components.iter().enumerate() {
        let checked = if selection.selected.contains(component) {
            "[x]"
        } else {
            "[ ]"
        };
        lines.push(selected_line(
            index == selection.component_cursor,
            &format!("{checked} {component}"),
        ));
        let target = selection
            .config
            .environments
            .get(environment)
            .and_then(|environment| environment.components.get(component));
        if let Some(target) = target {
            lines.push(Line::from(format!(
                "    {}",
                app.destination_label(&target.destination)
            )));
            lines.push(Line::from(format!("    Root: {}", safe_text(&target.root))));
        } else {
            lines.push(Line::from("    Target unavailable; edit configuration"));
            lines.push(Line::from(""));
        }
    }
    let selected_row = selection
        .component_cursor
        .saturating_mul(3)
        .saturating_add(5);
    let visible = usize::from(area.height.saturating_sub(2)).max(1);
    let scroll =
        u16::try_from(selected_row.saturating_sub(visible.saturating_sub(1))).unwrap_or(u16::MAX);
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .title(if is_production(environment) {
                        " [PRODUCTION] Select Components "
                    } else {
                        " Select Components "
                    })
                    .borders(Borders::ALL),
            )
            .scroll((scroll, 0)),
        area,
    );
}

fn render_deployment_planning(
    frame: &mut Frame<'_>,
    area: ratatui::layout::Rect,
    selection: &app::DeploySelectionState,
) {
    frame.render_widget(
        panel(
            " New Deployment · Environment check ",
            format!(
                "Checking Git state and {} selected Component target(s)…\n\nNo build, upload, or remote mutation occurs in this step.",
                selection.selected.len()
            ),
        ),
        area,
    );
}

fn render_deployment_review(
    frame: &mut Frame<'_>,
    area: ratatui::layout::Rect,
    plan: &crate::application::DeploymentPlan,
    scroll: u16,
) {
    let git = match &plan.git {
        crate::application::GitWorktreeState::Clean => "clean",
        crate::application::GitWorktreeState::Dirty { .. } => {
            "dirty — confirmation deploys these local changes"
        }
        crate::application::GitWorktreeState::NotRepository => "not a Git repository",
    };
    let mut content = format!(
        "CONFIRM DEPLOYMENT — changes selected servers.\nProject: {}\nEnvironment: {}\nGit: {git}\n\n",
        safe_text(&plan.selection.config.project),
        environment_label(&plan.selection.environment)
    );
    let _ = writeln!(
        content,
        "Branch: {}\nCommit: {}",
        plan.git_metadata
            .branch
            .as_deref()
            .unwrap_or("detached / unavailable"),
        plan.git_metadata.revision.as_deref().unwrap_or("no commit")
    );
    let order = plan
        .activation_order
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(" → ");
    let _ = writeln!(content, "Activation order: {order}");
    content.push_str(
        "Build order: Component name order. Only selected Components will be deployed.\n\n",
    );
    for entry in &plan.entries {
        let current = entry
            .current
            .as_ref()
            .map_or("not_deployed".into(), ToString::to_string);
        let _ = write!(
            content,
            "{}: {current} → {}\n  Destination: {}\n  Root: {}\n",
            entry.component,
            entry.release,
            safe_text(&entry.destination),
            safe_text(&entry.root)
        );
        for notice in &entry.notices {
            let _ = writeln!(content, "  Environment check: {notice}");
        }
        if let Some(build) = plan.selection.config.components.get(&entry.component) {
            content.push_str(&build_preview(build));
        }
        if let Some(target) = plan
            .selection
            .config
            .environments
            .get(&plan.selection.environment)
            .and_then(|environment| environment.components.get(&entry.component))
        {
            let _ = writeln!(
                content,
                "  Service: {}",
                target.systemd.as_deref().unwrap_or("none; files only")
            );
            let health = match (target.systemd.is_some(), target.health.is_some()) {
                (true, true) => "systemd stability and remote HTTP/HTTPS",
                (true, false) => "systemd stability",
                (false, true) => "remote HTTP/HTTPS",
                (false, false) => "none configured; application health will not be verified",
            };
            let _ = writeln!(content, "  Health: {health}\n");
        }
    }
    content.push_str("\nConfirm to build, package, upload, activate, and check health.");
    frame.render_widget(
        Paragraph::new(content)
            .block(
                Block::default()
                    .title(if is_production(&plan.selection.environment) {
                        " [PRODUCTION] Confirm deployment "
                    } else {
                        " Confirm deployment "
                    })
                    .borders(Borders::ALL),
            )
            .scroll((scroll, 0))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn build_preview(build: &crate::config::ComponentConfig) -> String {
    let mut content = format!(
        "  Working directory: {}\n  Artifact (relative to working directory): {}\n",
        build.working_directory.display(),
        build.artifact.path.display()
    );
    for command in &build.build {
        if command.shell {
            let _ = writeln!(content, "  Build [explicit shell]: {:?}", command.program);
        } else {
            let _ = writeln!(
                content,
                "  Build [program + arguments]: {:?} {:?}",
                command.program, command.args
            );
        }
    }
    content
}

fn render_deployment_running(
    frame: &mut Frame<'_>,
    area: ratatui::layout::Rect,
    logs: &std::collections::VecDeque<crate::drivers::DriverLog>,
    cancellation_requested: bool,
) {
    let status = if cancellation_requested {
        "Safe cancellation requested; waiting for recovery. Do not close the terminal."
    } else {
        "Deployment running. Esc requests safe cancellation."
    };
    render_deployment_log(frame, area, " Deployment progress ", status, logs);
}

fn render_deployment_finished(
    frame: &mut Frame<'_>,
    area: ratatui::layout::Rect,
    summary: &str,
    logs: &std::collections::VecDeque<crate::drivers::DriverLog>,
    scroll: u16,
) {
    let mut content = format!("{summary}\nRecent events:\n");
    for event in logs {
        let _ = writeln!(
            content,
            "[{}] {}",
            step_label(&event.namespace),
            event.message
        );
    }
    frame.render_widget(
        panel(" Deployment result ", content).scroll((scroll, 0)),
        area,
    );
}

fn render_deployment_log(
    frame: &mut Frame<'_>,
    area: ratatui::layout::Rect,
    title: &'static str,
    status: &str,
    logs: &std::collections::VecDeque<crate::drivers::DriverLog>,
) {
    let areas = Layout::vertical([Constraint::Length(3), Constraint::Min(0)]).split(area);
    frame.render_widget(panel(title, status.to_owned()), areas[0]);
    let visible = usize::from(areas[1].height.saturating_sub(2));
    let lines: Vec<_> = logs
        .iter()
        .skip(logs.len().saturating_sub(visible))
        .map(|event| {
            Line::from(format!(
                "[{}] {}",
                step_label(&event.namespace),
                event.message
            ))
        })
        .collect();
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .title(" Recent events ")
                .borders(Borders::ALL),
        ),
        areas[1],
    );
}

fn render_host_key_pending(
    frame: &mut Frame<'_>,
    area: ratatui::layout::Rect,
    draft: &app::NewSshDestinationState,
) {
    frame.render_widget(
        panel(
            " New SSH Destination · Host Key ",
            format!(
                "Connecting to {}…\n\nOnly the SSH handshake is performed. No authentication or remote command is attempted.",
                setup_endpoint(draft)
            ),
        ),
        area,
    );
}

fn render_host_key_confirmation(
    frame: &mut Frame<'_>,
    area: ratatui::layout::Rect,
    draft: &app::NewSshDestinationState,
    fingerprint: &crate::config::HostKeyFingerprint,
) {
    frame.render_widget(
        panel(
            " New SSH Destination · Confirm Host Key ",
            format!(
                "Endpoint: {}\n\nHost Key:\n{}\n\nVerify this fingerprint through a trusted channel before confirming. Only y accepts; Enter does not trust the key.",
                setup_endpoint(draft),
                fingerprint.as_str()
            ),
        ),
        area,
    );
}

fn render_authentication_pending(
    frame: &mut Frame<'_>,
    area: ratatui::layout::Rect,
    draft: &app::NewSshDestinationState,
) {
    frame.render_widget(
        panel(
            " New SSH Destination · Authentication ",
            format!(
                "Authenticating {}…\n\nAfter authentication, ShipForge runs read-only probes for the default root and systemd service candidates.",
                setup_endpoint(draft)
            ),
        ),
        area,
    );
}

fn render_remote_setup_selection(
    frame: &mut Frame<'_>,
    area: ratatui::layout::Rect,
    selection: &app::RemoteSetupSelectionState,
) {
    let root_state = match selection.root_state {
        crate::application::SetupRootState::Missing => {
            "missing (created on deployment if parent permissions allow)"
        }
        crate::application::SetupRootState::WritableDirectory => "writable directory",
        crate::application::SetupRootState::ReadOnlyDirectory => "read-only directory",
        crate::application::SetupRootState::NotDirectory => "exists but is not a directory",
    };
    let mut lines = vec![
        Line::from(format!("Component: {}", selection.component)),
        Line::from(format!("Default root: {}", selection.root)),
        Line::from(format!("Remote status: {root_state}")),
        Line::from(""),
        Line::from("Optional systemd service:"),
        selected_line(selection.cursor == 0, "Do not manage a service"),
    ];
    lines.extend(
        selection
            .systemd_units
            .iter()
            .enumerate()
            .map(|(index, unit)| selected_line(selection.cursor == index + 1, unit)),
    );
    if selection.systemd_units.is_empty() {
        lines.push(Line::from("  No service candidates were discovered"));
    }
    lines.extend(selection.notices.iter().map(|notice| {
        Line::from(vec![Span::styled(
            format!("Note: {notice}"),
            Style::default().fg(Color::Yellow),
        )])
    }));
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .title(" Remote setup candidates ")
                    .borders(Borders::ALL),
            )
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn render_key_browser(
    frame: &mut Frame<'_>,
    area: ratatui::layout::Rect,
    browser: &app::KeyFileBrowser,
) {
    let mut lines = vec![Line::from(format!(
        "Directory: {}",
        browser.directory.display()
    ))];
    if browser.entries.is_empty() {
        lines.push(Line::from("  No files or directories"));
    } else {
        lines.extend(browser.entries.iter().enumerate().map(|(index, path)| {
            let suffix = if path.is_dir() { "/" } else { "" };
            let name = path.file_name().map_or_else(
                || path.display().to_string(),
                |name| name.to_string_lossy().into_owned(),
            );
            selected_line(index == browser.selected, &format!("{name}{suffix}"))
        }));
    }
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .title(" Select SSH private key ")
                    .borders(Borders::ALL),
            )
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn render_new_ssh_destination(
    frame: &mut Frame<'_>,
    area: ratatui::layout::Rect,
    draft: &app::NewSshDestinationState,
) {
    let mut lines = vec![
        Line::from("Values are discovered when possible; edit only what is missing."),
        Line::from(""),
        form_line("Host", &draft.host, draft.field == app::SshField::Host),
        form_line("User", &draft.user, draft.field == app::SshField::User),
        form_line("Port", &draft.port, draft.field == app::SshField::Port),
        Line::from(""),
        Line::from(Span::styled(
            &draft.agent_status,
            Style::default().fg(Color::DarkGray),
        )),
    ];
    if draft.credentials.is_empty() {
        lines.push(Line::from(Span::styled(
            "No modern SSH identity found in the Agent, saved credentials, or ~/.ssh.",
            Style::default().fg(Color::Yellow),
        )));
    } else {
        lines.push(Line::from("Identity:"));
        lines.extend(
            draft
                .credentials
                .iter()
                .enumerate()
                .map(|(index, credential)| {
                    let selected = draft.field == app::SshField::Credential
                        && index == draft.credential_cursor;
                    selected_line(selected, credential.label())
                }),
        );
    }
    if !draft.connections.is_empty() {
        lines.push(Line::from(format!(
            "SSH config candidate {}/{} (press F2 to cycle)",
            draft.connection_cursor + 1,
            draft.connections.len()
        )));
    }
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .title(" New SSH Destination ")
                    .borders(Borders::ALL),
            )
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn form_line(label: &str, value: &str, selected: bool) -> Line<'static> {
    let value = safe_text(value);
    let marker = if selected { ">" } else { " " };
    let style = if selected {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    Line::from(Span::styled(format!("{marker} {label}: {value}"), style))
}

fn setup_endpoint(draft: &app::NewSshDestinationState) -> String {
    draft.port.parse::<u16>().map_or_else(
        |_| {
            format!(
                "{}@{}:{}",
                safe_text(&draft.user),
                safe_text(&draft.host),
                safe_text(&draft.port)
            )
        },
        |port| endpoint_label(&draft.user, &draft.host, port),
    )
}

fn render_destination_setup(
    frame: &mut Frame<'_>,
    area: ratatui::layout::Rect,
    setup: &app::DestinationSetupState,
) {
    let components = setup
        .components
        .report
        .components
        .iter()
        .filter(|candidate| setup.components.selected.contains(&candidate.name))
        .collect::<Vec<_>>();
    let mut lines = vec![Line::from(format!(
        "Root: {}",
        setup.components.root.display()
    ))];
    if let Some(component) = components.get(setup.component_cursor) {
        let assigned = setup
            .assignments
            .get(&component.name)
            .and_then(|key| {
                setup
                    .destinations
                    .iter()
                    .find(|destination| destination.key == *key)
            })
            .map_or("not assigned", |destination| destination.endpoint.as_str());
        lines.push(Line::from(format!(
            "Component {}/{}: {}   assigned: {assigned}",
            setup.component_cursor + 1,
            components.len(),
            component.name
        )));
    }
    if setup.destinations.is_empty() {
        lines.push(Line::from(Span::styled(
            "No saved Destinations. Create an SSH Destination to continue.",
            Style::default().fg(Color::Yellow),
        )));
    }
    for (index, destination) in setup.destinations.iter().enumerate() {
        lines.push(selected_line(
            index == setup.destination_cursor,
            &format!(
                "{} · {} · revision {}",
                destination.key,
                safe_text(&destination.endpoint),
                destination.revision.get()
            ),
        ));
    }
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .title(" First-time setup · Destinations ")
                    .borders(Borders::ALL),
            )
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn render_component_setup(
    frame: &mut Frame<'_>,
    area: ratatui::layout::Rect,
    setup: &app::ComponentSetupState,
) {
    let mut lines = vec![Line::from(format!("Root: {}", setup.root.display()))];
    if setup.report.components.is_empty() {
        lines.push(Line::from("No deployable Components were inferred."));
    }
    for (index, candidate) in setup.report.components.iter().enumerate() {
        let checked = if setup.selected.contains(&candidate.name) {
            "[x]"
        } else {
            "[ ]"
        };
        let text = format!(
            "{checked} {}  {:?}  artifact={}  source={}",
            candidate.name,
            candidate.confidence,
            candidate.setup.artifact.path.display(),
            candidate.source.display()
        );
        lines.push(selected_line(index == setup.cursor, &text));
    }
    for notice in &setup.report.notices {
        lines.push(Line::from(Span::styled(
            format!("Note: {notice}"),
            Style::default().fg(Color::Yellow),
        )));
    }
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .title(" First-time setup · Components ")
                    .borders(Borders::ALL),
            )
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn render_projects(frame: &mut Frame<'_>, area: ratatui::layout::Rect, app: &App) {
    let mut lines = Vec::new();
    if let Some(notice) = app.attention_notice() {
        lines.push(Line::styled(notice, Style::default().fg(Color::Yellow)));
        lines.push(Line::from(""));
    }
    for (index, status) in app.recent.iter().enumerate() {
        let suffix = if status.available {
            ""
        } else {
            " [unavailable]"
        };
        lines.push(selected_line(
            index == app.selected_recent,
            &format!("{}{}", status.project.root.display(), suffix),
        ));
    }
    lines.push(selected_line(
        app.selected_recent == app.recent.len(),
        "Browse directories…",
    ));
    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::default().title(" Projects ").borders(Borders::ALL))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn selected_line(selected: bool, text: &str) -> Line<'static> {
    let text = safe_text(text);
    if selected {
        Line::from(Span::styled(
            format!("> {text}"),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ))
    } else {
        Line::from(format!("  {text}"))
    }
}

fn panel(title: &'static str, content: String) -> Paragraph<'static> {
    Paragraph::new(content)
        .block(Block::default().title(title).borders(Borders::ALL))
        .wrap(Wrap { trim: false })
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use ratatui::{Terminal, backend::TestBackend};

    use crate::drivers::DriverLog;

    #[test]
    fn build_preview_distinguishes_shell_and_argument_boundaries() {
        use crate::config::{ArtifactSpec, BuildCommand, ComponentConfig};
        let build = ComponentConfig {
            working_directory: "services/api".into(),
            artifact: ArtifactSpec {
                path: "dist".into(),
            },
            build: vec![
                BuildCommand::argv("builder", ["--label", "two words"]),
                BuildCommand::shell("build && prepare"),
            ],
        };
        let preview = super::build_preview(&build);
        assert!(preview.contains("services/api"));
        assert!(preview.contains("relative to working directory): dist"));
        assert!(preview.contains(r#"[program + arguments]: "builder" ["--label", "two words"]"#));
        assert!(preview.contains(r#"[explicit shell]: "build && prepare""#));
    }

    #[test]
    fn progress_keeps_cancellation_status_and_latest_events_visible() {
        let logs: VecDeque<_> = (0..100)
            .map(|index| DriverLog {
                namespace: "build".into(),
                message: format!("event-{index:03}"),
            })
            .collect();
        let mut terminal = Terminal::new(TestBackend::new(100, 12)).unwrap();
        terminal
            .draw(|frame| super::render_deployment_running(frame, frame.area(), &logs, true))
            .unwrap();
        let content = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        assert!(content.contains("Safe cancellation requested"));
        assert!(content.contains("event-099"));
        assert!(!content.contains("event-000"));
    }

    #[test]
    fn deployment_result_renders_on_tiny_and_empty_terminals() {
        for (width, height) in [(0, 0), (1, 1), (20, 3), (80, 10)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| {
                    super::render_deployment_finished(
                        frame,
                        frame.area(),
                        "Deployment failed",
                        &VecDeque::new(),
                        0,
                    );
                })
                .unwrap();
        }
    }
}
