mod app;

use std::{
    io::{self, BufWriter, Stdout},
    time::Duration,
};

use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
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

    loop {
        app.poll_background();
        guard.terminal.draw(|frame| render(frame, &app))?;

        if event::poll(Duration::from_millis(250))?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
            && (matches!(key.code, KeyCode::Char('q')) && matches!(app.screen, Screen::Projects)
                || app.handle_key(key))
        {
            break;
        }
    }

    Ok(())
}

fn render(frame: &mut Frame<'_>, app: &App) {
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(2)])
        .split(frame.area());
    render_screen(frame, areas[0], app);
    let help = if app.deployment_session.is_active() {
        "Deployment active   return to progress or cancel safely"
    } else {
        match &app.screen {
            Screen::Projects => "↑/↓ select   Enter open   o browse   q quit",
            Screen::Browser(_) => {
                "↑/↓ select   Enter enter directory   Backspace parent   s select root   Esc back"
            }
            Screen::Overview { .. } => "Esc projects   q quit",
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
            Screen::DeploymentFinished { .. } => "Enter/Esc return to project overview",
            Screen::SetupComponents(_) => "↑/↓ select   Space toggle   Enter next   Esc projects",
            Screen::SetupDestinations(_) => {
                "←/→ Component   ↑/↓ Destination   Space assign   a add SSH   n review   Esc Components"
            }
            Screen::NewSshDestination(_) => {
                "Tab field   type edit   ↑/↓ identity   f browse key   F2 next host   Enter probe   Esc cancel"
            }
            Screen::HostKeyPending { .. } => "Fetching Host Key…   Esc cancel",
            Screen::HostKeyConfirm { .. } => "Enter/y trust and save   n/Esc reject",
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
    let footer = app.message.as_ref().map_or_else(
        || Line::from(help),
        |message| {
            Line::from(vec![
                Span::styled(
                    "Error: ",
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                ),
                Span::raw(message),
            ])
        },
    );
    frame.render_widget(Paragraph::new(footer), areas[1]);
}

fn render_screen(frame: &mut Frame<'_>, area: ratatui::layout::Rect, app: &App) {
    match &app.screen {
        Screen::Projects => render_projects(frame, area, app),
        Screen::Browser(browser) => {
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
        Screen::Overview { root, config } => {
            let content = format!(
                "Project: {}\nRoot: {}\nComponents: {}\nEnvironments: {}\n\nConfiguration loaded successfully.",
                config.project,
                root.display(),
                config.components.len(),
                config.environments.len()
            );
            frame.render_widget(panel(" Project overview ", content), area);
        }
        Screen::DeploySelection(selection) => render_deploy_selection(frame, area, selection),
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
        Screen::DeploymentFinished { summary, logs, .. } => {
            render_deployment_finished(frame, area, summary, logs);
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

fn render_deploy_selection(
    frame: &mut Frame<'_>,
    area: ratatui::layout::Rect,
    selection: &app::DeploySelectionState,
) {
    let environments = app::environment_names(selection);
    let components = app::deployment_components(selection);
    let environment = environments
        .get(selection.environment_cursor)
        .map_or("none", String::as_str);
    let mut lines = vec![
        Line::from(format!("Project: {}", selection.config.project)),
        Line::from(format!("Environment: {environment}")),
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
    }
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .title(" New Deployment · Select Components ")
                    .borders(Borders::ALL),
            )
            .wrap(Wrap { trim: false }),
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
        "Project: {}\nEnvironment: {}\nGit: {git}\n\n",
        plan.selection.config.project, plan.selection.environment
    );
    for entry in &plan.entries {
        let current = entry
            .current
            .as_ref()
            .map_or("not_deployed".into(), ToString::to_string);
        content.push_str(&format!(
            "{}: {current} → {}\n  Destination: {}\n  Root: {}\n",
            entry.component, entry.release, entry.destination, entry.root
        ));
    }
    content.push_str("\nConfirm to build, package, upload, activate, and check health.");
    frame.render_widget(
        Paragraph::new(content)
            .block(Block::default().title(" Deployment plan ").borders(Borders::ALL))
           Holder .scroll((scroll, 0))
            .wrap(Wrap { trim: false }),
        area,
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
                "Connecting to {}:{}…\n\nOnly the SSH handshake is performed. No authentication or remote command is attempted.",
                draft.host, draft.port
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
                "Endpoint: {}@{}:{}\n\nHost Key:\n{}\n\nVerify this fingerprint through a trusted channel before confirming.",
                draft.user,
                draft.host,
                draft.port,
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
                "Authenticating {}@{}:{}…\n\nAfter authentication, ShipForge runs read-only probes for the default root and systemd service candidates.",
                draft.user, draft.host, draft.port
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
            &destination.endpoint,
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
