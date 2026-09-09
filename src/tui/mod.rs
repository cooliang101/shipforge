mod app;
mod clipboard;
mod deployment_error;
mod i18n;
mod live_progress;
mod log_view;
mod overview;
mod picker;
mod presentation;

use std::{
    fmt::Write as _,
    io::{self, BufWriter, Stdout},
    sync::mpsc::{self, Receiver, TryRecvError},
    time::{Duration, Instant},
};

use crossterm::{
    cursor::Show,
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

use self::app::{App, ExitState, Screen};
use self::presentation::{endpoint_label, environment_label, is_production, safe_text, step_label};

type AppTerminal = Terminal<CrosstermBackend<BufWriter<Stdout>>>;

const ACTIVE_FRAME_INTERVAL: Duration = Duration::from_millis(50);
const IDLE_POLL_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Debug)]
struct FrameSchedule {
    dirty: bool,
    immediate: bool,
    last_draw: Option<Instant>,
}

impl Default for FrameSchedule {
    fn default() -> Self {
        Self {
            dirty: true,
            immediate: true,
            last_draw: None,
        }
    }
}

impl FrameSchedule {
    fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    fn mark_interaction(&mut self) {
        self.dirty = true;
        self.immediate = true;
    }

    fn draw_due(&self, now: Instant, periodic: bool) -> bool {
        self.immediate
            || (self.dirty || periodic)
                && self
                    .last_draw
                    .is_none_or(|last| now.saturating_duration_since(last) >= ACTIVE_FRAME_INTERVAL)
    }

    fn record_draw(&mut self, now: Instant) {
        self.dirty = false;
        self.immediate = false;
        self.last_draw = Some(now);
    }

    fn wait_timeout(&self, now: Instant, periodic: bool) -> Duration {
        if self.immediate {
            return Duration::ZERO;
        }
        if !self.dirty && !periodic {
            return IDLE_POLL_INTERVAL;
        }
        self.last_draw.map_or(Duration::ZERO, |last| {
            ACTIVE_FRAME_INTERVAL
                .saturating_sub(now.saturating_duration_since(last))
                .min(IDLE_POLL_INTERVAL)
        })
    }
}

struct TerminalGuard {
    terminal: AppTerminal,
    restoration: Restoration,
}

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        let mut setup = SetupGuard::raw();
        setup.alternate_attempted = true;
        if let Err(error) = execute!(io::stdout(), EnterAlternateScreen) {
            return Err(with_cleanup(error, setup.restore()));
        }

        let output = BufWriter::new(io::stdout());
        let backend = CrosstermBackend::new(output);
        match Terminal::new(backend) {
            Ok(terminal) => {
                setup.disarm();
                Ok(Self {
                    terminal,
                    restoration: Restoration::default(),
                })
            }
            Err(error) => Err(with_cleanup(error, setup.restore())),
        }
    }

    fn restore(&mut self) -> io::Result<()> {
        let mut cleanup = LiveTerminalCleanup {
            terminal: &mut self.terminal,
        };
        self.restoration.run(&mut cleanup)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

trait TerminalCleanup {
    fn disable_raw(&mut self) -> io::Result<()>;
    fn leave_alternate(&mut self) -> io::Result<()>;
    fn show_cursor(&mut self) -> io::Result<()>;
}

#[derive(Debug, Default)]
struct Restoration {
    attempted: bool,
}

impl Restoration {
    fn run(&mut self, cleanup: &mut impl TerminalCleanup) -> io::Result<()> {
        if std::mem::replace(&mut self.attempted, true) {
            return Ok(());
        }
        restore_terminal(cleanup)
    }
}

fn restore_terminal(cleanup: &mut impl TerminalCleanup) -> io::Result<()> {
    let failures = [
        ("disable raw mode", cleanup.disable_raw()),
        ("leave alternate screen", cleanup.leave_alternate()),
        ("show cursor", cleanup.show_cursor()),
    ]
    .into_iter()
    .filter_map(|(operation, result)| result.err().map(|_| operation))
    .collect::<Vec<_>>();
    if failures.is_empty() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "Terminal restoration was incomplete: {}",
            failures.join(", ")
        )))
    }
}

struct LiveTerminalCleanup<'a> {
    terminal: &'a mut AppTerminal,
}

impl TerminalCleanup for LiveTerminalCleanup<'_> {
    fn disable_raw(&mut self) -> io::Result<()> {
        disable_raw_mode()
    }

    fn leave_alternate(&mut self) -> io::Result<()> {
        execute!(self.terminal.backend_mut(), LeaveAlternateScreen)
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        self.terminal.show_cursor()
    }
}

#[derive(Debug)]
struct SetupGuard {
    raw_enabled: bool,
    alternate_attempted: bool,
    restored: bool,
}

impl SetupGuard {
    const fn raw() -> Self {
        Self {
            raw_enabled: true,
            alternate_attempted: false,
            restored: false,
        }
    }

    fn disarm(&mut self) {
        self.raw_enabled = false;
        self.alternate_attempted = false;
        self.restored = true;
    }

    fn restore(&mut self) -> io::Result<()> {
        if std::mem::replace(&mut self.restored, true) {
            return Ok(());
        }
        let mut failures = Vec::new();
        if self.raw_enabled && disable_raw_mode().is_err() {
            failures.push("disable raw mode");
        }
        if self.alternate_attempted {
            if execute!(io::stdout(), LeaveAlternateScreen).is_err() {
                failures.push("leave alternate screen");
            }
            if execute!(io::stdout(), Show).is_err() {
                failures.push("show cursor");
            }
        }
        self.raw_enabled = false;
        self.alternate_attempted = false;
        if failures.is_empty() {
            Ok(())
        } else {
            Err(io::Error::other(format!(
                "Terminal setup cleanup was incomplete: {}",
                failures.join(", ")
            )))
        }
    }
}

impl Drop for SetupGuard {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

fn with_cleanup(primary: io::Error, cleanup: io::Result<()>) -> io::Error {
    match cleanup {
        Ok(()) => primary,
        Err(cleanup) => io::Error::new(
            primary.kind(),
            format!("{primary}; additionally, {cleanup}"),
        ),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProcessControl {
    ExitRequested,
}

struct ProcessSignals {
    receiver: Receiver<ProcessControl>,
    watchers: Vec<tokio::task::JoinHandle<()>>,
}

impl ProcessSignals {
    #[cfg(unix)]
    fn install(runtime: &tokio::runtime::Handle) -> io::Result<Self> {
        use tokio::signal::unix::{SignalKind, signal};

        let mut interrupt = signal(SignalKind::interrupt())?;
        let mut terminate = signal(SignalKind::terminate())?;
        let mut hangup = signal(SignalKind::hangup())?;
        let (sender, receiver) = mpsc::sync_channel(1);
        let watcher = runtime.spawn(async move {
            loop {
                let notification = tokio::select! {
                    event = interrupt.recv() => event.is_some(),
                    event = terminate.recv() => event.is_some(),
                    event = hangup.recv() => event.is_some(),
                };
                if !notification {
                    break;
                }
                let _ = sender.try_send(ProcessControl::ExitRequested);
            }
        });
        Ok(Self {
            receiver,
            watchers: vec![watcher],
        })
    }

    #[cfg(windows)]
    fn install(runtime: &tokio::runtime::Handle) -> io::Result<Self> {
        use tokio::signal::windows::{ctrl_break, ctrl_c, ctrl_close, ctrl_shutdown};

        let mut interrupt = ctrl_c()?;
        let mut ctrl_break = ctrl_break()?;
        let mut close = ctrl_close()?;
        let mut shutdown = ctrl_shutdown()?;
        let (sender, receiver) = mpsc::sync_channel(1);
        let watcher = runtime.spawn(async move {
            loop {
                let notification = tokio::select! {
                    event = interrupt.recv() => event.is_some(),
                    event = ctrl_break.recv() => event.is_some(),
                    event = close.recv() => event.is_some(),
                    event = shutdown.recv() => event.is_some(),
                };
                if !notification {
                    break;
                }
                // Windows can impose a short deadline after close/shutdown
                // notification. ShipForge requests orderly cancellation but
                // cannot promise cleanup after the host forcibly terminates it.
                let _ = sender.try_send(ProcessControl::ExitRequested);
            }
        });
        Ok(Self {
            receiver,
            watchers: vec![watcher],
        })
    }

    #[cfg(not(any(unix, windows)))]
    fn install(_: &tokio::runtime::Handle) -> io::Result<Self> {
        let (_sender, receiver) = mpsc::sync_channel(1);
        Ok(Self {
            receiver,
            watchers: Vec::new(),
        })
    }

    fn consume(&self, request_exit: impl FnOnce()) -> bool {
        consume_process_control(&self.receiver, request_exit)
    }
}

impl Drop for ProcessSignals {
    fn drop(&mut self) {
        for watcher in &self.watchers {
            watcher.abort();
        }
    }
}

fn consume_process_control(
    receiver: &Receiver<ProcessControl>,
    request_exit: impl FnOnce(),
) -> bool {
    match receiver.try_recv() {
        Ok(ProcessControl::ExitRequested) => {
            while receiver.try_recv().is_ok() {}
            request_exit();
            true
        }
        Err(TryRecvError::Empty | TryRecvError::Disconnected) => false,
    }
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;

    #[derive(Default)]
    struct FakeCleanup {
        calls: Vec<&'static str>,
        fail_disable: bool,
        fail_leave: bool,
        fail_cursor: bool,
    }

    impl TerminalCleanup for FakeCleanup {
        fn disable_raw(&mut self) -> io::Result<()> {
            self.calls.push("disable");
            failure_if(self.fail_disable)
        }

        fn leave_alternate(&mut self) -> io::Result<()> {
            self.calls.push("leave");
            failure_if(self.fail_leave)
        }

        fn show_cursor(&mut self) -> io::Result<()> {
            self.calls.push("cursor");
            failure_if(self.fail_cursor)
        }
    }

    fn failure_if(fail: bool) -> io::Result<()> {
        if fail {
            Err(io::Error::other("private cleanup failure"))
        } else {
            Ok(())
        }
    }

    #[test]
    fn restoration_attempts_every_step_and_aggregates_failed_operations() {
        let mut cleanup = FakeCleanup {
            fail_disable: true,
            fail_leave: true,
            fail_cursor: true,
            ..Default::default()
        };
        let error = restore_terminal(&mut cleanup).unwrap_err();
        assert_eq!(cleanup.calls, ["disable", "leave", "cursor"]);
        let error = error.to_string();
        assert!(error.contains("disable raw mode"));
        assert!(error.contains("leave alternate screen"));
        assert!(error.contains("show cursor"));
        assert!(!error.contains("private cleanup failure"));
    }

    #[test]
    fn explicit_restoration_is_idempotent_and_drop_style_retry_is_a_noop() {
        let mut cleanup = FakeCleanup::default();
        let mut restoration = Restoration::default();
        restoration.run(&mut cleanup).unwrap();
        restoration.run(&mut cleanup).unwrap();
        assert_eq!(cleanup.calls, ["disable", "leave", "cursor"]);
    }

    #[test]
    fn process_control_event_invokes_the_exit_request_once_and_is_consumed() {
        let (sender, receiver) = mpsc::channel();
        sender.send(ProcessControl::ExitRequested).unwrap();
        sender.send(ProcessControl::ExitRequested).unwrap();
        let mut requests = 0;
        assert!(consume_process_control(&receiver, || requests += 1));
        assert_eq!(requests, 1);
        assert!(!consume_process_control(&receiver, || requests += 1));
        assert_eq!(requests, 1);
    }

    #[test]
    fn frame_schedule_draws_initial_and_dirty_frames_at_the_active_cap() {
        let started = Instant::now();
        let mut schedule = FrameSchedule::default();
        assert!(schedule.draw_due(started, false));
        schedule.record_draw(started);

        schedule.mark_dirty();
        let before_cap = started + ACTIVE_FRAME_INTERVAL.saturating_sub(Duration::from_millis(1));
        assert!(!schedule.draw_due(before_cap, false));
        assert_eq!(
            schedule.wait_timeout(before_cap, false),
            Duration::from_millis(1)
        );
        assert!(schedule.draw_due(started + ACTIVE_FRAME_INTERVAL, false));
    }

    #[test]
    fn frame_schedule_does_not_redraw_clean_idle_state() {
        let started = Instant::now();
        let mut schedule = FrameSchedule::default();
        schedule.record_draw(started);

        assert!(!schedule.draw_due(started + Duration::from_secs(2), false));
        assert_eq!(
            schedule.wait_timeout(started + Duration::from_secs(2), false),
            IDLE_POLL_INTERVAL
        );
    }

    #[test]
    fn frame_schedule_presents_each_interaction_before_reading_another() {
        let started = Instant::now();
        let mut schedule = FrameSchedule::default();
        schedule.record_draw(started);
        schedule.mark_interaction();

        let immediately_after = started + Duration::from_millis(1);
        assert!(schedule.draw_due(immediately_after, false));
        assert_eq!(
            schedule.wait_timeout(immediately_after, false),
            Duration::ZERO
        );
    }

    #[test]
    fn frame_schedule_refreshes_active_work_without_state_events() {
        let started = Instant::now();
        let mut schedule = FrameSchedule::default();
        schedule.record_draw(started);

        assert!(!schedule.draw_due(started + Duration::from_millis(49), true));
        assert!(schedule.draw_due(started + ACTIVE_FRAME_INTERVAL, true));
    }

    #[test]
    fn frame_schedule_coalesces_continuous_log_changes_to_twenty_frames_per_second() {
        let started = Instant::now();
        let mut schedule = FrameSchedule::default();
        schedule.record_draw(started);
        let mut draws = 0;
        for elapsed_ms in 1..=2_000 {
            schedule.mark_dirty();
            let now = started + Duration::from_millis(elapsed_ms);
            if schedule.draw_due(now, true) {
                draws += 1;
                schedule.record_draw(now);
            }
        }
        assert_eq!(draws, 40);
    }

    #[test]
    fn session_and_restoration_failures_are_both_reported() {
        let error = combine_session_result(
            Err(io::Error::other("session failed")),
            Err(io::Error::other("restoration failed")),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("session failed"));
        assert!(error.contains("restoration failed"));
    }
}

/// Runs the MVP terminal shell. Press `q` from an idle top-level page to exit.
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
    let signals = ProcessSignals::install(&tokio::runtime::Handle::current())?;
    let mut guard = TerminalGuard::enter()?;
    let Ok(mut app) = App::new(registry_path, destination_registry_path, &initial_directory) else {
        let error = io::Error::other(
            "Could not open the initial project directory. Check its path and local read permissions.",
        );
        return Err(with_cleanup(error, guard.restore()));
    };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        drive_event_loop(&mut app, &mut guard, &signals)
    }));
    // Keep the runtime alive until safe cancellation finishes even if terminal
    // drawing or input fails. A broken terminal must not silently detach work.
    let restoration = guard.restore();
    app.request_process_exit();
    app.shutdown();
    match result {
        Ok(result) => combine_session_result(result, restoration),
        Err(payload) => std::panic::resume_unwind(payload),
    }
}

fn combine_session_result(session: io::Result<()>, restoration: io::Result<()>) -> io::Result<()> {
    match session {
        Ok(()) => restoration,
        Err(error) => Err(with_cleanup(error, restoration)),
    }
}

fn drive_event_loop(
    app: &mut App,
    guard: &mut TerminalGuard,
    signals: &ProcessSignals,
) -> io::Result<()> {
    let mut frames = FrameSchedule::default();
    loop {
        let background = app.poll_background();
        if background.requires_frame_boundary() {
            frames.mark_interaction();
        } else if background.changed() {
            frames.mark_dirty();
        }
        if signals.consume(|| app.request_process_exit()) {
            frames.mark_interaction();
        }
        if event_loop_exit_ready(app) {
            break;
        }
        if let Some(text) = app.take_clipboard_request() {
            let transport = clipboard_transport();
            let outcome = clipboard::request_copy(guard.terminal.backend_mut(), &text, transport);
            app.finish_clipboard_request(outcome);
            frames.mark_dirty();
        }

        let now = Instant::now();
        if frames.draw_due(now, app.needs_periodic_redraw()) {
            guard.terminal.draw(|frame| render(frame, app))?;
            frames.record_draw(Instant::now());
        }

        let timeout = frames.wait_timeout(Instant::now(), app.needs_periodic_redraw());
        if event::poll(timeout)? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    if app.handle_key(key) {
                        app.request_process_exit();
                    }
                    frames.mark_interaction();
                }
                Event::Resize(_, _) => frames.mark_interaction(),
                _ => {}
            }
        }
    }

    Ok(())
}

fn event_loop_exit_ready(app: &App) -> bool {
    app.exit_ready()
}

fn clipboard_transport() -> clipboard::ClipboardTransport {
    // On Unix the terminal uses ANSI. On Windows, do not route OSC 52 through
    // the legacy console fallback. This detects the transport, not OSC support.
    #[cfg(windows)]
    let supported = crossterm::ansi_support::supports_ansi();
    #[cfg(not(windows))]
    let supported = true;
    if supported {
        clipboard::ClipboardTransport::Ansi
    } else {
        clipboard::ClipboardTransport::Unsupported
    }
}

fn render(frame: &mut Frame<'_>, app: &App) {
    i18n::with_language(app.language, || render_localized(frame, app));
}

fn render_localized(frame: &mut Frame<'_>, app: &App) {
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(if frame.area().height >= 18 { 2 } else { 1 }),
            Constraint::Min(3),
            Constraint::Length(if frame.area().height >= 18 { 3 } else { 2 }),
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
    render_content_with_actions(frame, areas[1], app);
    let help = navigation_help(app);
    let mut footer = vec![Line::from(help)];
    if let Some(message) = app.message.as_deref().or_else(|| {
        matches!(app.screen, Screen::Overview { .. })
            .then(|| app.attention_notice())
            .flatten()
    }) {
        footer.push(Line::from(vec![
            Span::styled(
                i18n::tr("Message: "),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(safe_text(message)),
        ]));
    }
    let footer = Paragraph::new(footer);
    let footer = if frame.area().height >= 18 {
        footer.block(
            Block::default()
                .borders(Borders::TOP)
                .border_style(Style::default().fg(Color::DarkGray)),
        )
    } else {
        footer
    };
    frame.render_widget(footer, areas[2]);
    if areas[0].width >= 48 {
        let hints = ratatui::layout::Rect::new(areas[0].right() - 34, areas[0].y, 34, 1);
        frame.render_widget(
            Paragraph::new(i18n::choose(
                " F1 Help  F4 Find  F6 Language ",
                " F1 帮助  F4 搜索  F6 语言 ",
            ))
            .style(Style::default().add_modifier(Modifier::REVERSED)),
            hints,
        );
    }
    if let Some(picker) = &app.picker {
        picker.render(frame, areas[1]);
    }
    if let Some(logs) = &app.log_workspace {
        logs.render(frame, areas[1]);
    }
    if app.help_open {
        frame.render_widget(ratatui::widgets::Clear, areas[1]);
        if let Some(logs) = &app.log_workspace {
            frame.render_widget(
                panel(" Log viewer help and current message ", logs.help_text())
                    .scroll((app.help_scroll, 0)),
                areas[1],
            );
        } else {
            let message = app
                .message
                .as_deref()
                .map_or_else(|| "No current message.".into(), safe_text);
            let instructions = crate::tui::i18n::format!(
                "Current page: {}\n\nPage keys: {help}\n\nF6 selects and saves the interface language.\n\nF4 finds candidates on choice pages; type to filter, Enter focuses a row, Esc keeps the original selection. It does not run an operation.\n\nF1 / Esc closes this help. Up/Down or PgUp/PgDn scroll. Ctrl+C requests safe cancellation of an active operation.\n\nSelect a confirmation action with Down, then unmodified Enter confirms. Optional shortcuts: c deploy/rollback, y trust a host key.\n\nFocus uses > and reverse video; selections use [x]/[ ]; warnings and production status have text labels, not color alone.\n\nMessage: {message}",
                "当前页面：{}\n\n页面快捷键：{help}\n\nF6 选择语言并保存，F4 搜索候选；Enter 仅定位，不执行操作。\n\nF1 / Esc 关闭帮助，↑/↓ 或 PgUp/PgDn 滚动。Ctrl+C 安全取消活动任务。\n\n向下选中确认项后，普通 Enter 确认。可选快捷键：c 发布/回退，y 信任主机指纹。\n\n> 和反色表示焦点，[x]/[ ] 表示勾选；警告和生产环境有文字标识。\n\n提示：{message}",
                app.context_label()
            );
            frame.render_widget(
                panel(" Keyboard help and current message ", instructions)
                    .scroll((app.help_scroll, 0)),
                areas[1],
            );
        }
    }
    if app.language_menu.is_some() {
        overview::render_language(frame, app);
    }
    render_exit_overlay(frame, app.exit_state());
}

fn navigation_help(app: &App) -> &'static str {
    if matches!(app.screen, Screen::Projects) {
        i18n::choose(
            "↑/↓ select · Enter open · Ctrl+C / Esc twice exit",
            "↑/↓ 选择 · Enter 打开 · Ctrl+C / 连按两次 Esc 退出",
        )
    } else if app.page_actions().is_some() {
        i18n::choose(
            "↑/↓ select · Enter open / toggle · Esc back · PgUp/PgDn scroll",
            "↑/↓ 选择（列表末尾进入操作） · Enter 打开 / 勾选 · Esc 返回 · PgUp/PgDn 滚动",
        )
    } else {
        i18n::tr(page_help(app))
    }
}

fn render_content_with_actions(frame: &mut Frame<'_>, area: ratatui::layout::Rect, app: &App) {
    if app.page_actions().is_some() {
        let split = Layout::vertical([
            Constraint::Min(3),
            Constraint::Length(u16::from(area.height >= 18)),
            Constraint::Length(app.action_height(area.height)),
        ])
        .split(area);
        render_screen(frame, split[0], app);
        app.render_actions(frame, split[2]);
    } else {
        render_screen(frame, area, app);
    }
}

fn render_exit_overlay(frame: &mut Frame<'_>, state: ExitState) {
    let (title, message) = match state {
        ExitState::Running => return,
        ExitState::Confirm => (
            " Exit? c / Esc ",
            "c / Ctrl+C: cancel tracked work and exit safely.\nEsc / r: resume the task.\n\nWork is still active.",
        ),
        ExitState::Waiting => (
            " Exiting safely ",
            "Waiting for tracked workers to stop.\nCancellation requested; recovery may still be running.\n\nThe terminal will be restored before the process exits.",
        ),
    };
    let bounds = frame.area();
    let width = bounds.width.min(72);
    let height = bounds.height.min(7);
    let area = ratatui::layout::Rect::new(
        bounds.x + bounds.width.saturating_sub(width) / 2,
        bounds.y + bounds.height.saturating_sub(height) / 2,
        width,
        height,
    );
    frame.render_widget(ratatui::widgets::Clear, area);
    frame.render_widget(
        Paragraph::new(crate::tui::i18n::tr(message))
            .block(
                Block::default()
                    .title(crate::tui::i18n::tr(title))
                    .borders(Borders::ALL)
                    .style(Style::default().fg(Color::Yellow)),
            )
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn page_help(app: &App) -> &'static str {
    if app.log_workspace.is_some() {
        "Log viewer open · F1 full keys / current message · Ctrl+C safe cancellation"
    } else if app.setup_busy() {
        if app.setup_cancelling() {
            "SSH setup cancellation requested; waiting for the worker to stop"
        } else {
            "SSH setup working; Esc requests cancellation and waits before retry"
        }
    } else if app.deployment_session.is_active()
        && !matches!(
            app.screen,
            Screen::Management(_)
                | Screen::Connections(_)
                | Screen::ProjectEdit(_)
                | Screen::DeploymentRunning { .. }
        )
    {
        "Deployment active   return to progress or cancel safely"
    } else {
        match &app.screen {
            Screen::Management(screen) => screen.help(),
            Screen::Connections(screen) => screen.help(),
            Screen::ProjectEdit(screen) => screen.help(),
            Screen::ManualComponent(screen) => screen.help(),
            Screen::Reinitialize(screen) => screen.help(),
            Screen::Projects => {
                "↑↓ select · Enter open · o browse · f refresh · c connections · x unregister · q quit"
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
                    "Esc request safe cancellation   l logs/search/export"
                }
            }
            Screen::DeploymentFinished { .. } => {
                "↑/↓ scroll   l logs/search/export   Enter/Esc project overview"
            }
            Screen::SetupComponents(setup) if setup.selected.is_empty() => {
                "Esc projects   a add manually   ↑/↓ move   Space select (required)"
            }
            Screen::SetupComponents(_) => {
                "Esc projects   ↑/↓ move   Space toggle   Enter next   a add manually"
            }
            Screen::SetupDestinations(_) => {
                "←→ Component · ↑↓ connection · Space assign · e target · a SSH · n review · Esc back"
            }
            Screen::NewSshDestination(draft)
                if matches!(
                    draft.credentials.get(draft.credential_cursor),
                    Some(app::CredentialChoice::Password(_))
                ) =>
            {
                "Tab field   Backspace erase   Delete clear   F3 key   Enter next   Esc cancel"
            }
            Screen::NewSshDestination(_) => {
                "Tab field   ↑/↓ identity   F3 key   F5 password   Enter probe   Esc cancel"
            }
            Screen::HostKeyPending { .. } => "Fetching Host Key…   Esc cancel",
            Screen::HostKeyConfirm { .. } => "y trust fingerprint and authenticate   n/Esc reject",
            Screen::SshAuthenticationPending { .. } => "Authenticating and probing…   Esc cancel",
            Screen::RemoteSetupSelection(screen) => screen.help(),
            Screen::KeyBrowser { .. } => {
                "↑/↓ select   Enter directory   Backspace parent   s choose file   Esc back"
            }
            Screen::SetupReview { .. } => {
                "↑/↓ or PgUp/PgDn scroll   c confirm and save   Esc Destinations"
            }
        }
    }
}

fn render_screen(frame: &mut Frame<'_>, area: ratatui::layout::Rect, app: &App) {
    match &app.screen {
        Screen::Management(screen) => screen.render(frame, area, app),
        Screen::Connections(screen) => screen.render(frame, area),
        Screen::ProjectEdit(screen) => screen.render(frame, area),
        Screen::ManualComponent(screen) => screen.render(frame, area),
        Screen::Reinitialize(screen) => screen.render(frame, area),
        Screen::Projects => render_projects(frame, area, app),
        Screen::Browser(browser) => render_project_browser(frame, area, browser),
        Screen::Overview { root, config } => overview::render(frame, area, app, root, config),
        Screen::DeploySelection(selection) => render_deploy_selection(frame, area, selection, app),
        Screen::DeploymentPlanning { selection, .. } => {
            render_deployment_planning(frame, area, selection);
        }
        Screen::DeploymentReview { plan, scroll } => {
            render_deployment_review(frame, area, plan, *scroll);
        }
        Screen::DeploymentRunning {
            cancellation_requested,
            ..
        } => render_deployment_running(
            frame,
            area,
            &app.live_logs,
            *cancellation_requested,
            app.live_progress.as_ref(),
        ),
        Screen::DeploymentFinished {
            summary, scroll, ..
        } => {
            render_deployment_finished(frame, area, summary, &app.live_logs, *scroll);
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
            selection.render(frame, area);
        }
        Screen::KeyBrowser { browser, .. } => render_key_browser(frame, area, browser),
        Screen::SetupReview {
            prepared, scroll, ..
        } => {
            frame.render_widget(
                Paragraph::new(prepared.preview().to_owned())
                    .block(
                        Block::default()
                            .title(i18n::tr(" First-time setup · Review shipforge.yaml "))
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
    let mut lines = vec![Line::from(crate::tui::i18n::format!(
        "Current: {}",
        "当前位置：{}",
        crate::tui::presentation::path_label(&browser.directory)
    ))];
    if browser.children.is_empty() {
        lines.push(Line::from("  No child directories"));
    } else {
        lines.extend(browser.children.iter().enumerate().map(|(index, path)| {
            selected_line(
                index == browser.selected,
                &path.file_name().map_or_else(
                    || crate::tui::presentation::path_label(path),
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
        .scroll((
            choice_scroll(browser.selected.saturating_add(1), area.height),
            0,
        ));
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
        Line::from(crate::tui::i18n::format!(
            "Project: {}",
            "项目：{}",
            safe_text(&selection.config.project)
        )),
        Line::from(crate::tui::i18n::format!(
            "Environment: {}",
            "环境：{}",
            environment_label(environment)
        )),
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
            lines.push(Line::from(crate::tui::i18n::format!(
                "    Root: {}",
                "    目录：{}",
                safe_text(&target.root)
            )));
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
            crate::tui::i18n::format!(
                "Checking Git state and {} selected Component target(s)…\n\nNo build, upload, or remote mutation occurs in this step.","正在检查 Git 状态和 {} 个已选组件目标…\n\n此步骤不会构建、上传或修改远端。",
                selection.selected.len()
            ),
        ),
        area,
    );
}

fn deployment_review_header(plan: &crate::application::DeploymentPlan) -> String {
    let git = match &plan.git {
        crate::application::GitWorktreeState::Clean => "clean",
        crate::application::GitWorktreeState::Dirty { .. } => {
            "dirty — confirmation deploys these local changes"
        }
        crate::application::GitWorktreeState::NotRepository => "not a Git repository",
    };
    let git = i18n::tr(git);
    let mut content = crate::tui::i18n::format!(
        "CONFIRM DEPLOYMENT — changes selected servers.\nProject: {}\nEnvironment: {}\nGit: {git}\n\n",
        "确认发布 — 将修改所选服务器。\n项目：{}\n环境：{}\nGit：{git}\n\n",
        safe_text(&plan.selection.config.project),
        environment_label(&plan.selection.environment)
    );
    let _ = crate::tui::i18n::writeln!(
        content,
        "Branch: {}\nCommit: {}",
        "分支：{}\n提交：{}",
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
    let _ = crate::tui::i18n::writeln!(content, "Activation order: {order}", "发布顺序：{order}");
    content.push_str(i18n::tr(
        "Build order: Component name order. Only selected Components will be deployed.\n\n",
    ));
    content
}

fn render_deployment_review(
    frame: &mut Frame<'_>,
    area: ratatui::layout::Rect,
    plan: &crate::application::DeploymentPlan,
    scroll: u16,
) {
    let mut content = deployment_review_header(plan);
    for entry in &plan.entries {
        let current = entry
            .current
            .as_ref()
            .map_or("not_deployed".into(), ToString::to_string);
        let _ = crate::tui::i18n::write!(
            content,
            "{}: {current} → {}\n  Destination: {}\n  Root: {}\n",
            "{}：{current} → {}\n  目标：{}\n  目录：{}\n",
            entry.component,
            entry.release,
            safe_text(&entry.destination),
            safe_text(&entry.root)
        );
        for notice in &entry.notices {
            let _ = crate::tui::i18n::writeln!(
                content,
                "  Environment check: {notice}",
                "  环境检查：{notice}"
            );
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
            let label = target
                .service
                .as_ref()
                .map_or(i18n::tr("none; files only"), |service| {
                    service
                        .preset_unit()
                        .unwrap_or_else(|| i18n::tr("custom commands"))
                });
            let _ = crate::tui::i18n::writeln!(content, "  Service: {label}", "  服务：{label}");
            if let Some(service) = &target.service {
                content.push_str(&service_preview(service, &target.root));
            }
            let health = match (
                target
                    .service
                    .as_ref()
                    .is_some_and(|service| service.check.is_some()),
                target.health.is_some(),
            ) {
                (true, true) => "configured service check and remote HTTP/HTTPS",
                (true, false) => "configured service check",
                (false, true) => "remote HTTP/HTTPS",
                (false, false) => "none configured; application health will not be verified",
            };
            let health = i18n::tr(health);
            let _ = crate::tui::i18n::writeln!(
                content,
                "  Health: {health}\n",
                "  健康检查：{health}\n"
            );
        }
    }
    content.push_str(crate::tui::i18n::tr(
        "\nConfirm to build, package, upload, activate, and check health.",
    ));
    frame.render_widget(
        Paragraph::new(content)
            .block(
                Block::default()
                    .title(i18n::tr(if is_production(&plan.selection.environment) {
                        " [PRODUCTION] Confirm deployment "
                    } else {
                        " Confirm deployment "
                    }))
                    .borders(Borders::ALL),
            )
            .scroll((scroll, 0))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn service_preview(service: &crate::config::ServiceConfig, root: &str) -> String {
    let root = safe_text(root);
    let mut content =
        crate::tui::i18n::format!("  Service directory: {root}\n", "  服务工作目录：{root}\n");
    for (stage, commands) in [
        ("First start", &service.start),
        ("Update", service.update_commands()),
        ("Restore", service.restore_commands()),
        ("Stop before publishing / on empty rollback", &service.stop),
    ] {
        for argv in commands {
            let _ = writeln!(content, "  {stage}: {argv:?}");
        }
    }
    match &service.check {
        Some(crate::config::ServiceCheck::Command { argv }) => {
            let _ = crate::tui::i18n::writeln!(
                content,
                "  Read-only check in {root}: {argv:?}; exit 0 required, at most 5 attempts",
                "  只读检查目录 {root}：{argv:?}；退出码须为 0，最多 5 次"
            );
        }
        Some(crate::config::ServiceCheck::Systemd { unit }) => {
            let _ = crate::tui::i18n::writeln!(
                content,
                "  Systemd check: {unit}; active and stable NRestarts for 10 seconds",
                "  Systemd 检查：{unit}；active 且重启次数稳定 10 秒"
            );
        }
        None => content.push_str("  No service health check configured\n"),
    }
    content
}

#[cfg(test)]
mod service_preview_tests {
    #[test]
    fn preview_sanitizes_directory_text_and_shows_effective_lifecycle_commands() {
        let service = crate::config::ServiceConfig::systemd("api.service");
        let preview = super::service_preview(&service, "/srv/a\u{202e}\u{1b}[2J");
        assert!(!preview.contains('\u{202e}') && !preview.contains('\u{1b}'));
        for stage in [
            "First start",
            "Update",
            "Restore",
            "Stop before publishing / on empty rollback",
        ] {
            assert!(preview.contains(stage));
        }
        assert_eq!(preview.matches("\"restart\"").count(), 3);
    }
}

fn build_preview(build: &crate::config::ComponentConfig) -> String {
    let mut content = crate::tui::i18n::format!(
        "  Working directory: {}\n  Artifact (relative to working directory): {}\n",
        "  构建目录：{}\n  产物（相对构建目录）：{}\n",
        crate::tui::presentation::path_label(&build.working_directory),
        crate::tui::presentation::path_label(&build.artifact.path)
    );
    for command in &build.build {
        if command.shell {
            let _ = crate::tui::i18n::writeln!(
                content,
                "  Build [explicit shell]: {:?}",
                "  构建 [显式 Shell]：{:?}",
                command.program
            );
        } else {
            let _ = crate::tui::i18n::writeln!(
                content,
                "  Build [program + arguments]: {:?} {:?}",
                "  构建 [程序 + 参数]：{:?} {:?}",
                command.program,
                command.args
            );
        }
    }
    content
}

fn render_deployment_running(
    frame: &mut Frame<'_>,
    area: ratatui::layout::Rect,
    logs: &log_view::LogView,
    cancellation_requested: bool,
    progress: Option<&live_progress::LiveProgress>,
) {
    let status = if cancellation_requested {
        "Safe cancellation requested; waiting for recovery. Do not close the terminal."
    } else {
        "Deployment running. Esc requests safe cancellation."
    };
    let progress = progress.map(live_progress::LiveProgress::snapshot);
    if let Some(progress) = progress {
        let areas = Layout::vertical([Constraint::Length(6), Constraint::Min(0)]).split(area);
        let mut text = crate::tui::i18n::format!(
            "{status}\nElapsed: {}.{}s · l opens logs/search/export\n",
            "{status}\n耗时：{}.{} 秒 · l 日志/搜索/导出\n",
            progress.elapsed_ms / 1000,
            (progress.elapsed_ms % 1000) / 100
        );
        let mut steps = progress.steps.iter().collect::<Vec<_>>();
        steps.sort_by_key(|step| std::cmp::Reverse(step.updated_sequence));
        for step in steps.into_iter().take(2) {
            let _ = writeln!(
                text,
                "{} / {}: {:?} · persistence {:?}",
                step.scope.component,
                step_label(&step.scope.step),
                step.state,
                step.persistence
            );
        }
        if progress.dropped_rows > 0
            || progress.dropped_steps > 0
            || progress.rejected_events > 0
            || progress.poisoned
        {
            let _ = writeln!(
                text,
                "Window gaps: {} rows / {} steps / {} rejected; progress unavailable: {}",
                progress.dropped_rows,
                progress.dropped_steps,
                progress.rejected_events,
                progress.poisoned
            );
        }
        frame.render_widget(panel(" Deployment progress ", text), areas[0]);
        render_deployment_log(
            frame,
            areas[1],
            " Recent output ",
            "Live window; retained history is available through l → h",
            logs,
        );
    } else {
        render_deployment_log(frame, area, " Deployment progress ", status, logs);
    }
}

fn render_deployment_finished(
    frame: &mut Frame<'_>,
    area: ratatui::layout::Rect,
    summary: &str,
    logs: &log_view::LogView,
    scroll: u16,
) {
    let mut content =
        crate::tui::i18n::format!("{summary}\nRecent events:\n", "{summary}\n最近事件：\n");
    for row in logs.matching() {
        let _ = writeln!(
            content,
            "[{}] {}",
            step_label(&row.event.namespace),
            row.event.message
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
    logs: &log_view::LogView,
) {
    let areas = Layout::vertical([Constraint::Length(3), Constraint::Min(0)]).split(area);
    frame.render_widget(panel(title, status.to_owned()), areas[0]);
    let visible = usize::from(areas[1].height.saturating_sub(2));
    let matching = logs.matching();
    let lines: Vec<_> = matching
        .iter()
        .skip(matching.len().saturating_sub(visible))
        .map(|row| {
            Line::from(format!(
                "[{}] {}",
                step_label(&row.event.namespace),
                row.event.message
            ))
        })
        .collect();
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .title(i18n::tr(" Recent events "))
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
            crate::tui::i18n::format!(
                "Connecting to {}…\n\nOnly the SSH handshake is performed. No authentication or remote command is attempted.","正在连接 {}…\n\n仅执行 SSH 握手，不进行认证或运行远端命令。",
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
            crate::tui::i18n::format!(
                "Endpoint: {}\n\nHost Key:\n{}\n\nVerify this fingerprint through a trusted channel before confirming. Select Trust this host key with Down, then Enter accepts; y is an optional shortcut.","地址：{}\n\n主机密钥：\n{}\n\n请通过可信渠道核对指纹。向下选中“信任此主机指纹”后按 Enter 确认；也可按 y。",
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
            crate::tui::i18n::format!(
                "Authenticating {}…\n\nAfter authentication, ShipForge runs read-only probes for the default root and systemd service candidates.","正在认证 {}…\n\n认证后仅探测默认目录和 systemd 候选服务。",
                setup_endpoint(draft)
            ),
        ),
        area,
    );
}

fn render_key_browser(
    frame: &mut Frame<'_>,
    area: ratatui::layout::Rect,
    browser: &app::KeyFileBrowser,
) {
    let mut lines = vec![Line::from(crate::tui::i18n::format!(
        "Directory: {}",
        "目录：{}",
        crate::tui::presentation::path_label(&browser.directory)
    ))];
    if browser.entries.is_empty() {
        lines.push(Line::from("  No files or directories"));
    } else {
        lines.extend(browser.entries.iter().enumerate().map(|(index, path)| {
            let suffix = if browser.directories.contains(path) {
                "/"
            } else {
                ""
            };
            let name = path.file_name().map_or_else(
                || crate::tui::presentation::path_label(path),
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
            .scroll((
                choice_scroll(browser.selected.saturating_add(1), area.height),
                0,
            )),
        area,
    );
}

fn render_new_ssh_destination(
    frame: &mut Frame<'_>,
    area: ratatui::layout::Rect,
    draft: &app::NewSshDestinationState,
) {
    if let Some(app::CredentialChoice::Password(input)) =
        draft.credentials.get(draft.credential_cursor)
    {
        render_password_connection(
            frame,
            area,
            &draft.host,
            &draft.user,
            &draft.port,
            draft.field,
            input,
        );
        return;
    }
    let mut lines = vec![
        Line::from("Values are discovered when possible; edit only what is missing."),
        Line::from(""),
        form_line("Host", &draft.host, draft.field == app::SshField::Host),
        form_line("User", &draft.user, draft.field == app::SshField::User),
        form_line("Port", &draft.port, draft.field == app::SshField::Port),
        Line::from(""),
        Line::from(safe_text(&draft.agent_status)),
    ];
    if draft.credentials.is_empty() {
        lines.push(Line::from(Span::styled(
            "No identities are available in this form. Press F3 to choose a key file; F5 for password.",
            Style::default().fg(Color::Yellow),
        )));
    } else {
        lines.push(Line::from(i18n::tr("Identity:")));
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
        lines.push(Line::from(crate::tui::i18n::format!(
            "SSH config candidate {}/{} (press F2 to cycle)",
            "SSH 配置候选 {}/{}（F2 切换）",
            draft.connection_cursor + 1,
            draft.connections.len()
        )));
    }
    lines.push(Line::from(
        "F5: password (hidden); Backspace: erase last; Delete: clear.",
    ));
    lines.push(Line::from(
        "Password is encrypted for this Windows user after authentication.",
    ));
    let focused_row = match draft.field {
        app::SshField::Host => 2,
        app::SshField::User => 3,
        app::SshField::Port => 4,
        app::SshField::Credential if draft.credentials.is_empty() => 7,
        app::SshField::Credential => 8 + draft.credential_cursor.min(draft.credentials.len() - 1),
    };
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .title(i18n::tr(" New SSH Destination "))
                    .borders(Borders::ALL),
            )
            .scroll((choice_scroll(focused_row, area.height), 0)),
        area,
    );
}

fn render_password_connection(
    frame: &mut Frame<'_>,
    area: ratatui::layout::Rect,
    host: &str,
    user: &str,
    port: &str,
    field: app::SshField,
    input: &crate::config::PasswordInput,
) {
    let lines = vec![
        form_line("Host", host, field == app::SshField::Host),
        form_line("User", user, field == app::SshField::User),
        form_line("Port", port, field == app::SshField::Port),
        Line::from(""),
        form_line(
            "Password",
            input.masked_value(),
            field == app::SshField::Credential,
        ),
        Line::from(""),
        Line::from(i18n::tr("Enter: review host key before connecting")),
    ];
    let focus = match field {
        app::SshField::Host => 0,
        app::SshField::User => 1,
        app::SshField::Port => 2,
        app::SshField::Credential => 4,
    };
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .title(i18n::tr(" SSH connection · Password "))
                    .borders(Borders::ALL),
            )
            .scroll((choice_scroll(focus, area.height), 0)),
        area,
    );
}

fn form_line(label: &str, value: &str, selected: bool) -> Line<'static> {
    let label = i18n::tr(label);
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
    let mut lines = vec![Line::from(crate::tui::i18n::format!(
        "Root: {}",
        "目录：{}",
        crate::tui::presentation::path_label(&setup.components.root)
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
        lines.push(Line::from(crate::tui::i18n::format!(
            "Component {}/{}: {}   assigned: {assigned}",
            "组件 {}/{}：{}   已分配：{assigned}",
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
            .scroll((
                choice_scroll(setup.destination_cursor.saturating_add(2), area.height),
                0,
            )),
        area,
    );
}

fn render_component_setup(
    frame: &mut Frame<'_>,
    area: ratatui::layout::Rect,
    setup: &app::ComponentSetupState,
) {
    let mut lines = vec![Line::from(crate::tui::i18n::format!(
        "Root: {}",
        "目录：{}",
        crate::tui::presentation::path_label(&setup.root)
    ))];
    if setup.report.components.is_empty() {
        lines.push(Line::from("No deployable Components were inferred. Press a to add one manually, or Esc to choose another project."));
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
            crate::tui::presentation::path_label(&candidate.setup.artifact.path),
            crate::tui::presentation::path_label(&candidate.source)
        );
        lines.push(selected_line(index == setup.cursor, &text));
    }
    for notice in &setup.report.notices {
        lines.push(Line::from(Span::styled(
            crate::tui::i18n::format!("Note: {notice}", "提示：{notice}"),
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
            .scroll((
                choice_scroll(setup.cursor.saturating_add(1), area.height),
                0,
            )),
        area,
    );
}

fn render_projects(frame: &mut Frame<'_>, area: ratatui::layout::Rect, app: &App) {
    let mut lines = Vec::new();
    let area = if app.recent_unavailable {
        let height = area.height.min(2);
        frame.render_widget(Paragraph::new("Recent projects unavailable; cached entries are disabled. f retries; o browses directories.").wrap(Wrap { trim: false }),
            ratatui::layout::Rect::new(area.x, area.y, area.width, height));
        ratatui::layout::Rect::new(
            area.x,
            area.y.saturating_add(height),
            area.width,
            area.height.saturating_sub(height),
        )
    } else {
        area
    };
    if let Some(notice) = app.attention_notice() {
        lines.push(Line::styled(notice, Style::default().fg(Color::Yellow)));
        lines.push(Line::from(""));
    }
    let selected_row = lines.len().saturating_add(app.selected_recent);
    for (index, status) in app.recent.iter().enumerate() {
        let suffix = if status.available {
            ""
        } else {
            " [unavailable]"
        };
        lines.push(selected_line(
            index == app.selected_recent,
            &format!(
                "{}{}",
                crate::tui::presentation::path_label(&status.project.root),
                suffix
            ),
        ));
    }
    lines.push(selected_line(
        app.selected_recent == app.recent.len(),
        i18n::tr("Browse directories…"),
    ));
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .title(i18n::tr(" Projects "))
                    .borders(Borders::ALL),
            )
            .scroll((choice_scroll(selected_row, area.height), 0)),
        area,
    );
}

fn selected_line(selected: bool, text: &str) -> Line<'static> {
    let text = safe_text(text);
    if selected {
        Line::from(Span::styled(
            format!("> {text}"),
            Style::default().add_modifier(Modifier::BOLD | Modifier::REVERSED),
        ))
    } else {
        Line::from(format!("  {text}"))
    }
}

fn choice_scroll(row: usize, height: u16) -> u16 {
    u16::try_from(row.saturating_sub(usize::from(height.saturating_sub(3)))).unwrap_or(u16::MAX)
}

fn panel(title: &'static str, content: String) -> Paragraph<'static> {
    Paragraph::new(content)
        .block(
            Block::default()
                .title(i18n::tr(title))
                .borders(Borders::ALL),
        )
        .wrap(Wrap { trim: false })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::{Terminal, backend::TestBackend, layout::Rect, widgets::Paragraph};

    use crate::telemetry::log_record::{LogEvent, LogEventKind};

    use super::{ExitState, log_view};

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
        let mut logs = log_view::LogView::default();
        for index in 0..100 {
            assert!(logs.push(log_view::LogRow {
                sequence: index + 1,
                elapsed_ms: None,
                event: Arc::new(LogEvent {
                    namespace: "build".into(),
                    message: format!("event-{index:03}"),
                    scope: None,
                    kind: LogEventKind::Output,
                }),
            }));
        }
        let mut terminal = Terminal::new(TestBackend::new(100, 12)).unwrap();
        terminal
            .draw(|frame| super::render_deployment_running(frame, frame.area(), &logs, true, None))
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
                        &log_view::LogView::default(),
                        0,
                    );
                })
                .unwrap();
        }
    }

    #[test]
    fn exit_overlay_is_topmost_and_distinguishes_confirmation_from_waiting() {
        for (state, expected) in [
            (ExitState::Confirm, "c / Ctrl+C"),
            (ExitState::Waiting, "Waiting for tracked workers"),
        ] {
            let mut terminal = Terminal::new(TestBackend::new(80, 10)).unwrap();
            terminal
                .draw(|frame| {
                    frame.render_widget(Paragraph::new("UNDERLAY-MARKER"), Rect::new(10, 4, 20, 1));
                    super::render_exit_overlay(frame, state);
                })
                .unwrap();
            let content = terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(ratatui::buffer::Cell::symbol)
                .collect::<String>();
            assert!(content.contains(expected));
            assert!(!content.contains("UNDERLAY-MARKER"));
        }
    }

    #[test]
    fn exit_overlay_handles_tiny_and_empty_terminals() {
        for (width, height) in [(0, 0), (1, 1), (8, 2), (20, 3)] {
            for state in [ExitState::Confirm, ExitState::Waiting] {
                let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                terminal
                    .draw(|frame| super::render_exit_overlay(frame, state))
                    .unwrap();
            }
        }
    }

    #[test]
    fn short_exit_overlay_prioritizes_the_available_action() {
        let mut terminal = Terminal::new(TestBackend::new(20, 3)).unwrap();
        terminal
            .draw(|frame| super::render_exit_overlay(frame, ExitState::Confirm))
            .unwrap();
        let content = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        assert!(content.contains("c / Ctrl+C"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn q_then_c_exits_when_app_becomes_ready_without_a_signal_flag() {
        let directory = tempfile::tempdir().unwrap();
        let mut app = super::App::new(
            directory.path().join("projects.yaml"),
            directory.path().join("destinations.yaml"),
            directory.path(),
        )
        .unwrap();

        assert!(!app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE,)));
        assert!(matches!(app.screen, super::Screen::Connections(_)));
        // The Connections worker remains tracked until poll_background joins
        // it, even if its local read finishes before the next key press.
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE,)));
        assert_eq!(app.exit_state(), ExitState::Confirm);
        assert!(!super::event_loop_exit_ready(&app));

        assert!(!app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE,)));
        assert_eq!(app.exit_state(), ExitState::Waiting);
        assert!(!super::event_loop_exit_ready(&app));
        app.shutdown();
        assert!(super::event_loop_exit_ready(&app));
    }
}

#[cfg(test)]
mod project_path_display_tests {
    #[test]
    fn project_list_displays_normalized_path_without_changing_its_selection_target() {
        let directory = tempfile::tempdir().unwrap();
        let mut app = super::App::new(
            directory.path().join("projects.yaml"),
            directory.path().join("destinations.yaml"),
            directory.path(),
        )
        .unwrap();
        let root = std::path::PathBuf::from(r"\\?\D:\project\aiagent");
        app.recent = vec![crate::projects::ProjectStatus {
            project: crate::projects::RegisteredProject {
                root: root.clone(),
                last_opened_unix_ms: 0,
            },
            available: true,
        }];
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 12)).unwrap();
        terminal
            .draw(|frame| super::render_projects(frame, frame.area(), &app))
            .unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        assert!(text.contains("D:/project/aiagent"));
        assert!(!text.contains(r"\\?\"));
        assert_eq!(app.recent[0].project.root, root);
    }
}
