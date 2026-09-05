use std::env;
use std::ffi::OsStr;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use portable_pty::{Child, ChildKiller, CommandBuilder, ExitStatus, PtySize, native_pty_system};

const RELEASE_BINARY_ENV: &str = "SHIPFORGE_RELEASE_SMOKE_BINARY";
const ALT_SCREEN_ENTER: &[u8] = b"\x1b[?1049h";
const ALT_SCREEN_LEAVE: &[u8] = b"\x1b[?1049l";
const CURSOR_HIDE: &[u8] = b"\x1b[?25l";
const CURSOR_SHOW: &[u8] = b"\x1b[?25h";
const CURSOR_POSITION_QUERY: &[u8] = b"\x1b[6n";
const CURSOR_POSITION_RESPONSE: &[u8] = b"\x1b[1;1R";
const FIRST_FRAME_TEXT: &[u8] = b"Projects";
const STARTUP_TIMEOUT: Duration = Duration::from_secs(10);
const EXIT_TIMEOUT: Duration = Duration::from_secs(15);
const READER_TIMEOUT: Duration = Duration::from_secs(5);
const CHILD_REAP_TIMEOUT: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_millis(5);
const CTRL_C_OBSERVATION_DELAY: Duration = Duration::from_millis(250);
#[cfg(windows)]
const CONPTY_CTRL_C_FALLBACK_DELAY: Duration = Duration::from_millis(250);
#[cfg(windows)]
const CONPTY_CTRL_C_KEY_DOWN: &[u8] = b"\x1b[67;46;3;1;8;1_";
#[cfg(windows)]
const CONPTY_CTRL_C_KEY_UP: &[u8] = b"\x1b[67;46;3;0;8;1_";
const PREFIX_LIMIT: usize = 128 * 1024;
const TAIL_LIMIT: usize = 128 * 1024;
const WALK_ENTRY_LIMIT: usize = 1_024;

#[test]
#[ignore = "requires SHIPFORGE_RELEASE_SMOKE_BINARY pointing to a built release binary"]
fn release_binary_q_exits_and_restores_terminal() {
    run_release_smoke(SmokeScenario::Quit)
        .unwrap_or_else(|error| panic!("q PTY smoke failed: {error}"));
}

#[test]
#[ignore = "requires SHIPFORGE_RELEASE_SMOKE_BINARY pointing to a built release binary"]
fn release_binary_recovers_after_idle_ctrl_c_bytes_then_q() {
    // Idle Ctrl+C is cancellation-only. This black-box smoke cannot prove the key was consumed
    // because that action has no visible state; it proves those platform bytes do not prevent a
    // subsequent specified q exit and terminal recovery. App-level tests cover key dispatch.
    run_release_smoke(SmokeScenario::ControlCThenQuit)
        .unwrap_or_else(|error| panic!("Ctrl+C input PTY smoke failed: {error}"));
}

#[derive(Clone, Copy)]
enum SmokeScenario {
    Quit,
    ControlCThenQuit,
}

fn run_release_smoke(scenario: SmokeScenario) -> Result<(), String> {
    let binary = release_binary_from_environment()?;
    let fixture = IsolatedFixture::new().map_err(display_error)?;
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(display_error)?;
    #[cfg(unix)]
    let initial_terminal_mode = pair
        .master
        .get_termios()
        .ok_or_else(|| "native PTY did not expose its initial termios".to_owned())?;
    let reader = pair.master.try_clone_reader().map_err(display_error)?;
    let mut writer = pair.master.take_writer().map_err(display_error)?;
    let command = isolated_command(&binary, &fixture);
    let child = pair.slave.spawn_command(command).map_err(display_error)?;
    let mut child = ManagedChild::new(child);
    drop(pair.slave);

    let capture = Arc::new(Mutex::new(OutputCapture::default()));
    let reader_task = match spawn_reader(reader, Arc::clone(&capture)) {
        Ok(task) => task,
        Err(error) => {
            let cleanup = child.kill_and_reap();
            return Err(match cleanup {
                Ok(()) => display_error(error),
                Err(cleanup) => format!("{}; {cleanup}", display_error(error)),
            });
        }
    };
    let result = drive_child(&mut child, &capture, &mut writer, scenario);
    let termination_result = if result.is_err() {
        child.kill_and_reap()
    } else {
        Ok(())
    };
    #[cfg(unix)]
    let terminal_mode_result = match pair.master.get_termios() {
        Some(final_mode) if final_mode == initial_terminal_mode => Ok(()),
        Some(_) => Err("release binary did not restore the native PTY termios".to_owned()),
        None => Err("native PTY did not expose its final termios".to_owned()),
    };
    #[cfg(windows)]
    let terminal_mode_result: Result<(), String> = Ok(());
    drop(writer);
    let master_task = thread::spawn(move || drop(pair.master));

    let reader_result = finish_reader(&capture, reader_task);
    let master_result = finish_task(master_task, "PTY master did not close after process exit");
    let snapshot = snapshot(&capture);
    termination_result.map_err(|error| with_output(&error, &snapshot))?;
    let status = result.map_err(|error| with_output(&error, &snapshot))?;
    reader_result.map_err(|error| with_output(&error, &snapshot))?;
    master_result.map_err(|error| with_output(&error, &snapshot))?;
    terminal_mode_result.map_err(|error| with_output(&error, &snapshot))?;
    validate_success(&status)?;
    validate_terminal_recovery(snapshot.positions)
        .map_err(|error| with_output(error, &snapshot))?;
    assert_no_state_files(fixture.root.path()).map_err(display_error)
}

fn drive_child(
    child: &mut ManagedChild,
    capture: &Arc<Mutex<OutputCapture>>,
    writer: &mut Box<dyn Write + Send>,
    scenario: SmokeScenario,
) -> Result<ExitStatus, String> {
    wait_for_initial_frame(child, capture, writer)?;
    let exit_deadline = Instant::now() + EXIT_TIMEOUT;
    match scenario {
        SmokeScenario::Quit => write_input(writer, b"q")?,
        SmokeScenario::ControlCThenQuit => {
            exercise_control_c_without_exit(child, writer, exit_deadline)?;
            write_input(writer, b"q")?;
        }
    }
    wait_for_exit(child, exit_deadline)
}

fn wait_for_initial_frame(
    child: &mut ManagedChild,
    capture: &Arc<Mutex<OutputCapture>>,
    writer: &mut Box<dyn Write + Send>,
) -> Result<(), String> {
    let startup_deadline = Instant::now() + STARTUP_TIMEOUT;
    let mut answered_cursor_query = false;
    loop {
        let positions = snapshot(capture).positions;
        if positions.cursor_query.is_some() && !answered_cursor_query {
            writer
                .write_all(CURSOR_POSITION_RESPONSE)
                .map_err(display_error)?;
            writer.flush().map_err(display_error)?;
            answered_cursor_query = true;
        }
        if let Some(status) = child.try_wait()? {
            return Err(format!(
                "process exited before the first rendered frame (code {})",
                status.exit_code()
            ));
        }
        if positions.initial_frame_ready() {
            return Ok(());
        }
        if Instant::now() >= startup_deadline {
            return Err("timed out waiting for the first rendered frame".to_owned());
        }
        thread::sleep(POLL_INTERVAL);
    }
}

fn wait_for_exit(child: &mut ManagedChild, deadline: Instant) -> Result<ExitStatus, String> {
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            return Err("timed out waiting for the process to exit".to_owned());
        }
        thread::sleep(POLL_INTERVAL);
    }
}

fn exercise_control_c_without_exit(
    child: &mut ManagedChild,
    writer: &mut Box<dyn Write + Send>,
    deadline: Instant,
) -> Result<(), String> {
    write_input(writer, b"\x03")?;
    #[cfg(windows)]
    send_platform_control_c(child, writer, deadline)?;
    confirm_process_stays_alive(child, CTRL_C_OBSERVATION_DELAY, deadline)
}

#[cfg(windows)]
fn send_platform_control_c(
    child: &mut ManagedChild,
    writer: &mut Box<dyn Write + Send>,
    deadline: Instant,
) -> Result<(), String> {
    confirm_process_stays_alive(child, CONPTY_CTRL_C_FALLBACK_DELAY, deadline)?;
    // portable-pty requests ConPTY's Win32 input mode. A literal ETX is tried first, but this
    // mode represents Ctrl+C as lossless KEY_EVENT_RECORD bytes when ETX alone is not accepted.
    writer
        .write_all(CONPTY_CTRL_C_KEY_DOWN)
        .map_err(display_error)?;
    writer
        .write_all(CONPTY_CTRL_C_KEY_UP)
        .map_err(display_error)?;
    writer.flush().map_err(display_error)
}

fn confirm_process_stays_alive(
    child: &mut ManagedChild,
    duration: Duration,
    deadline: Instant,
) -> Result<(), String> {
    let observation_deadline = (Instant::now() + duration).min(deadline);
    loop {
        if let Some(status) = child.try_wait()? {
            return Err(format!(
                "idle Ctrl+C input unexpectedly exited the process (code {})",
                status.exit_code()
            ));
        }
        let now = Instant::now();
        if now >= deadline {
            return Err("timed out while observing Ctrl+C input handling".to_owned());
        }
        if now >= observation_deadline {
            return Ok(());
        }
        thread::sleep(POLL_INTERVAL);
    }
}

fn write_input(writer: &mut Box<dyn Write + Send>, input: &[u8]) -> Result<(), String> {
    writer.write_all(input).map_err(display_error)?;
    writer.flush().map_err(display_error)
}

fn isolated_command(binary: &Path, fixture: &IsolatedFixture) -> CommandBuilder {
    let mut command = CommandBuilder::new(binary);
    command.env_clear();
    command.cwd(&fixture.workspace);
    command.env("TERM", "xterm-256color");
    command.env("HOME", &fixture.home);
    command.env("USERPROFILE", &fixture.home);
    command.env("APPDATA", &fixture.config);
    command.env("LOCALAPPDATA", &fixture.local_config);
    command.env("XDG_CONFIG_HOME", &fixture.config);
    command.env("XDG_CACHE_HOME", &fixture.cache);
    command.env("XDG_DATA_HOME", &fixture.data);
    command.env("TMPDIR", &fixture.temporary);
    command.env("TMP", &fixture.temporary);
    command.env("TEMP", &fixture.temporary);
    command.env("RUST_BACKTRACE", "0");
    preserve_windows_runtime_environment(&mut command);
    command
}

#[cfg(windows)]
fn preserve_windows_runtime_environment(command: &mut CommandBuilder) {
    for name in ["SystemRoot", "WINDIR"] {
        if let Some(value) = env::var_os(name) {
            command.env(name, value);
        }
    }
}

#[cfg(not(windows))]
fn preserve_windows_runtime_environment(_command: &mut CommandBuilder) {}

fn release_binary_from_environment() -> Result<PathBuf, String> {
    let value = env::var_os(RELEASE_BINARY_ENV)
        .ok_or_else(|| format!("{RELEASE_BINARY_ENV} is not set"))?;
    validate_release_binary(Path::new(&value)).map_err(str::to_owned)
}

fn validate_release_binary(path: &Path) -> Result<PathBuf, &'static str> {
    if !path.is_absolute() {
        return Err("release smoke binary path must be absolute");
    }
    if path.parent().and_then(Path::file_name) != Some(OsStr::new("release")) {
        return Err("release smoke binary must be directly inside a release directory");
    }
    let metadata =
        fs::symlink_metadata(path).map_err(|_| "release smoke binary metadata is unavailable")?;
    if !metadata.file_type().is_file() {
        return Err("release smoke binary must be a regular, non-symlink file");
    }
    let canonical =
        fs::canonicalize(path).map_err(|_| "release smoke binary could not be resolved")?;
    if canonical.parent().and_then(Path::file_name) != Some(OsStr::new("release")) {
        return Err("resolved release smoke binary is outside the release directory");
    }
    Ok(canonical)
}

struct IsolatedFixture {
    root: tempfile::TempDir,
    workspace: PathBuf,
    home: PathBuf,
    config: PathBuf,
    local_config: PathBuf,
    cache: PathBuf,
    data: PathBuf,
    temporary: PathBuf,
}

impl IsolatedFixture {
    fn new() -> io::Result<Self> {
        let root = tempfile::Builder::new()
            .prefix("shipforge-release-smoke-")
            .tempdir()?;
        let workspace = root.path().join("workspace");
        let home = root.path().join("home");
        let config = root.path().join("config");
        let local_config = root.path().join("local-config");
        let cache = root.path().join("cache");
        let data = root.path().join("data");
        let temporary = root.path().join("tmp");
        for directory in [
            &workspace,
            &home,
            &config,
            &local_config,
            &cache,
            &data,
            &temporary,
        ] {
            fs::create_dir(directory)?;
        }
        Ok(Self {
            root,
            workspace,
            home,
            config,
            local_config,
            cache,
            data,
            temporary,
        })
    }
}

struct ManagedChild {
    child: Option<Box<dyn Child + Send + Sync>>,
}

impl ManagedChild {
    fn new(child: Box<dyn Child + Send + Sync>) -> Self {
        Self { child: Some(child) }
    }

    fn try_wait(&mut self) -> Result<Option<ExitStatus>, String> {
        let child = self
            .child
            .as_mut()
            .ok_or_else(|| "process was already reaped".to_owned())?;
        let status = child.try_wait().map_err(display_error)?;
        if status.is_some() {
            self.child.take();
        }
        Ok(status)
    }

    fn kill_and_reap(&mut self) -> Result<(), String> {
        self.kill_and_reap_with_timeout(CHILD_REAP_TIMEOUT)
    }

    fn kill_and_reap_with_timeout(&mut self, timeout: Duration) -> Result<(), String> {
        let Some(mut child) = self.child.take() else {
            return Ok(());
        };
        let started = Instant::now();
        let deadline = started + timeout;
        let retry_at = started + timeout / 2;
        let mut kill_failures = usize::from(child.kill().is_err());
        let mut retried = false;
        loop {
            match child.try_wait() {
                Ok(Some(_)) => return Ok(()),
                Ok(None) => {}
                Err(_) => return Err("timed-out PTY child status could not be read".to_owned()),
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(if kill_failures == 0 {
                    "timed-out PTY child did not exit after two bounded kill requests".to_owned()
                } else {
                    "timed-out PTY child did not exit and a bounded kill request failed".to_owned()
                });
            }
            if !retried && now >= retry_at {
                retried = true;
                kill_failures += usize::from(child.kill().is_err());
            }
            thread::sleep(POLL_INTERVAL.min(deadline.saturating_duration_since(now)));
        }
    }
}

impl Drop for ManagedChild {
    fn drop(&mut self) {
        drop(self.kill_and_reap());
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct TerminalPositions {
    cursor_query: Option<u64>,
    alternate_enter: Option<u64>,
    cursor_hide: Option<u64>,
    first_frame: Option<u64>,
    alternate_leave: Option<u64>,
    cursor_show: Option<u64>,
}

impl TerminalPositions {
    fn initial_frame_ready(self) -> bool {
        let (Some(hide), Some(frame)) = (self.cursor_hide, self.first_frame) else {
            return false;
        };
        platform_initial_frame_ready(self, hide, frame)
    }
}

#[cfg(unix)]
fn platform_initial_frame_ready(positions: TerminalPositions, hide: u64, frame: u64) -> bool {
    positions.alternate_enter.is_some_and(|enter| enter < hide)
        && hide < frame
        && positions.alternate_leave.is_none_or(|leave| frame < leave)
        && positions.cursor_show.is_none_or(|show| frame < show)
}

#[cfg(windows)]
fn platform_initial_frame_ready(positions: TerminalPositions, hide: u64, frame: u64) -> bool {
    match positions.alternate_enter {
        Some(enter) => {
            enter < hide
                && hide < frame
                && positions.alternate_leave.is_none_or(|leave| frame < leave)
                && positions.cursor_show.is_none_or(|show| frame < show)
        }
        None => {
            // ConPTY consumes the 1049 transition and can expose the resulting
            // screen before forwarding the cursor-hide bytes. Child liveness plus
            // both setup facts and the absence of recovery facts is the strongest
            // observable initial-frame boundary in this mode.
            positions.alternate_leave.is_none() && positions.cursor_show.is_none() && hide != frame
        }
    }
}

#[derive(Default)]
struct OutputCapture {
    total_bytes: u64,
    prefix: Vec<u8>,
    tail: Vec<u8>,
    scan_tail: Vec<u8>,
    positions: TerminalPositions,
    finished: bool,
}

impl OutputCapture {
    fn push(&mut self, bytes: &[u8]) {
        let old_total = self.total_bytes;
        self.total_bytes = self
            .total_bytes
            .saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
        let prefix_room = PREFIX_LIMIT.saturating_sub(self.prefix.len());
        self.prefix
            .extend_from_slice(&bytes[..bytes.len().min(prefix_room)]);
        self.tail.extend_from_slice(bytes);
        if self.tail.len() > TAIL_LIMIT {
            self.tail.drain(..self.tail.len() - TAIL_LIMIT);
        }

        let mut searchable = Vec::with_capacity(self.scan_tail.len() + bytes.len());
        searchable.extend_from_slice(&self.scan_tail);
        searchable.extend_from_slice(bytes);
        let base = old_total.saturating_sub(u64::try_from(self.scan_tail.len()).unwrap_or(0));
        self.positions.observe(&searchable, base);
        let keep = longest_marker_length().saturating_sub(1);
        self.scan_tail = searchable[searchable.len().saturating_sub(keep)..].to_vec();
    }
}

impl TerminalPositions {
    fn observe(&mut self, bytes: &[u8], base: u64) {
        observe_first(&mut self.cursor_query, bytes, base, CURSOR_POSITION_QUERY);
        observe_first(&mut self.alternate_enter, bytes, base, ALT_SCREEN_ENTER);
        observe_first(&mut self.cursor_hide, bytes, base, CURSOR_HIDE);
        observe_first(&mut self.first_frame, bytes, base, FIRST_FRAME_TEXT);
        observe_first(&mut self.alternate_leave, bytes, base, ALT_SCREEN_LEAVE);
        observe_first(&mut self.cursor_show, bytes, base, CURSOR_SHOW);
    }
}

#[derive(Clone)]
struct CaptureSnapshot {
    total_bytes: u64,
    prefix: Vec<u8>,
    tail: Vec<u8>,
    positions: TerminalPositions,
    finished: bool,
}

fn spawn_reader(
    mut reader: Box<dyn Read + Send>,
    capture: Arc<Mutex<OutputCapture>>,
) -> io::Result<JoinHandle<()>> {
    thread::Builder::new()
        .name("shipforge-release-smoke-reader".to_owned())
        .spawn(move || {
            let mut buffer = [0_u8; 8 * 1024];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(length) => lock_capture(&capture).push(&buffer[..length]),
                }
            }
            lock_capture(&capture).finished = true;
        })
}

fn finish_reader(capture: &Arc<Mutex<OutputCapture>>, task: JoinHandle<()>) -> Result<(), String> {
    let deadline = Instant::now() + READER_TIMEOUT;
    while !snapshot(capture).finished {
        if Instant::now() >= deadline {
            return Err("PTY output reader did not finish after process exit".to_owned());
        }
        thread::sleep(POLL_INTERVAL);
    }
    task.join()
        .map_err(|_| "PTY output reader panicked".to_owned())
}

fn finish_task(task: JoinHandle<()>, timeout_error: &str) -> Result<(), String> {
    let deadline = Instant::now() + READER_TIMEOUT;
    while !task.is_finished() {
        if Instant::now() >= deadline {
            return Err(timeout_error.to_owned());
        }
        thread::sleep(POLL_INTERVAL);
    }
    task.join()
        .map_err(|_| "PTY cleanup worker panicked".to_owned())
}

fn snapshot(capture: &Arc<Mutex<OutputCapture>>) -> CaptureSnapshot {
    let capture = lock_capture(capture);
    CaptureSnapshot {
        total_bytes: capture.total_bytes,
        prefix: capture.prefix.clone(),
        tail: capture.tail.clone(),
        positions: capture.positions,
        finished: capture.finished,
    }
}

fn lock_capture(capture: &Arc<Mutex<OutputCapture>>) -> std::sync::MutexGuard<'_, OutputCapture> {
    capture
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn observe_first(slot: &mut Option<u64>, bytes: &[u8], base: u64, marker: &[u8]) {
    if slot.is_some() {
        return;
    }
    if let Some(index) = find_bytes(bytes, marker) {
        *slot = Some(base.saturating_add(u64::try_from(index).unwrap_or(u64::MAX)));
    }
}

fn find_bytes(bytes: &[u8], needle: &[u8]) -> Option<usize> {
    bytes
        .windows(needle.len())
        .position(|window| window == needle)
}

fn longest_marker_length() -> usize {
    [
        ALT_SCREEN_ENTER.len(),
        ALT_SCREEN_LEAVE.len(),
        CURSOR_HIDE.len(),
        CURSOR_SHOW.len(),
        CURSOR_POSITION_QUERY.len(),
        FIRST_FRAME_TEXT.len(),
    ]
    .into_iter()
    .max()
    .unwrap_or(1)
}

fn validate_success(status: &ExitStatus) -> Result<(), String> {
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "release binary exited unsuccessfully (code {})",
            status.exit_code()
        ))
    }
}

fn validate_ansi_terminal_recovery(positions: TerminalPositions) -> Result<(), &'static str> {
    let (Some(enter), Some(hide), Some(frame), Some(leave), Some(show)) = (
        positions.alternate_enter,
        positions.cursor_hide,
        positions.first_frame,
        positions.alternate_leave,
        positions.cursor_show,
    ) else {
        return Err("terminal setup or recovery sequence was incomplete");
    };
    if enter < hide && hide < frame && frame < leave && leave < show {
        Ok(())
    } else {
        Err("terminal setup and recovery sequences were out of order")
    }
}

#[cfg(unix)]
fn validate_terminal_recovery(positions: TerminalPositions) -> Result<(), &'static str> {
    validate_ansi_terminal_recovery(positions)
}

#[cfg(windows)]
fn validate_terminal_recovery(positions: TerminalPositions) -> Result<(), &'static str> {
    match (positions.alternate_enter, positions.alternate_leave) {
        (Some(_), Some(_)) => validate_ansi_terminal_recovery(positions),
        (None, None) => {
            let (Some(hide), Some(frame), Some(show)) = (
                positions.cursor_hide,
                positions.first_frame,
                positions.cursor_show,
            ) else {
                return Err(
                    "Windows ConPTY cursor setup or recovery sequence was incomplete; ConPTY does not expose 1049 bytes",
                );
            };
            // ConPTY may serialize the newly exposed screen before cursor-hide,
            // so only recovery ordering is stable: show must follow both facts.
            if frame < show && hide < show {
                Ok(())
            } else {
                Err(
                    "Windows ConPTY cursor setup and recovery sequences were out of order; ConPTY does not expose 1049 bytes",
                )
            }
        }
        _ => Err(
            "Windows ConPTY exposed only one alternate-screen transition; expected both 1049 bytes or neither",
        ),
    }
}

fn with_output(error: &str, capture: &CaptureSnapshot) -> String {
    format!("{error}; PTY output: {}", capture.diagnostic())
}

impl CaptureSnapshot {
    fn diagnostic(&self) -> String {
        const DIAGNOSTIC_EDGE: usize = 2 * 1024;
        let prefix = &self.prefix[..self.prefix.len().min(DIAGNOSTIC_EDGE)];
        let tail_start = self.tail.len().saturating_sub(DIAGNOSTIC_EDGE);
        let tail = &self.tail[tail_start..];
        let mut text = String::new();
        for byte in prefix {
            text.extend(std::ascii::escape_default(*byte).map(char::from));
        }
        if self.total_bytes > u64::try_from(prefix.len() + tail.len()).unwrap_or(u64::MAX) {
            text.push_str("<bounded-output-omitted>");
        }
        if self.total_bytes > u64::try_from(prefix.len()).unwrap_or(u64::MAX) {
            for byte in tail {
                text.extend(std::ascii::escape_default(*byte).map(char::from));
            }
        }
        text
    }
}

fn assert_no_state_files(root: &Path) -> io::Result<()> {
    let files = non_directory_entries(root)?;
    if files.is_empty() {
        return Ok(());
    }
    Err(io::Error::other(format!(
        "release smoke created local state: {}",
        files
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    )))
}

fn non_directory_entries(root: &Path) -> io::Result<Vec<PathBuf>> {
    let mut directories = vec![root.to_path_buf()];
    let mut entries_seen = 0_usize;
    let mut files = Vec::new();
    while let Some(directory) = directories.pop() {
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            entries_seen += 1;
            if entries_seen > WALK_ENTRY_LIMIT {
                return Err(io::Error::other("isolated state tree exceeded entry limit"));
            }
            let path = entry.path();
            if entry.file_type()?.is_dir() {
                directories.push(path);
            } else {
                files.push(path);
            }
        }
    }
    files.sort();
    Ok(files)
}

fn display_error(error: impl std::fmt::Display) -> String {
    error.to_string()
}

#[test]
fn release_binary_validation_rejects_relative_and_non_release_paths() {
    assert_eq!(
        validate_release_binary(Path::new("target/release/shipforge")),
        Err("release smoke binary path must be absolute")
    );

    let fixture = tempfile::tempdir().unwrap();
    let binary = fixture.path().join("shipforge");
    fs::write(&binary, b"not executed").unwrap();
    assert_eq!(
        validate_release_binary(&binary),
        Err("release smoke binary must be directly inside a release directory")
    );
}

#[test]
fn release_binary_validation_accepts_only_regular_file_in_release_directory() {
    let fixture = tempfile::tempdir().unwrap();
    let release = fixture.path().join("release");
    fs::create_dir(&release).unwrap();
    let binary = release.join("shipforge-test-binary");
    fs::write(&binary, b"not executed").unwrap();
    let directory = release.join("not-a-binary");
    fs::create_dir(&directory).unwrap();

    assert_eq!(
        validate_release_binary(&binary),
        Ok(fs::canonicalize(binary).unwrap())
    );
    assert_eq!(
        validate_release_binary(&directory),
        Err("release smoke binary must be a regular, non-symlink file")
    );
}

#[cfg(unix)]
#[test]
fn release_binary_validation_rejects_symlinks() {
    use std::os::unix::fs::symlink;

    let fixture = tempfile::tempdir().unwrap();
    let release = fixture.path().join("release");
    fs::create_dir(&release).unwrap();
    let binary = release.join("binary");
    let link = release.join("shipforge");
    fs::write(&binary, b"not executed").unwrap();
    symlink(binary, &link).unwrap();

    assert_eq!(
        validate_release_binary(&link),
        Err("release smoke binary must be a regular, non-symlink file")
    );
}

#[test]
fn output_capture_finds_split_markers_without_growing_retention() {
    let mut capture = OutputCapture::default();
    capture.push(b"\x1b[?10");
    capture.push(b"49h\x1b[?25lPro");
    capture.push(b"jects");
    capture.push(&vec![b'x'; PREFIX_LIMIT + TAIL_LIMIT + 1]);
    capture.push(b"\x1b[?1049l\x1b[?25h");

    assert!(capture.positions.initial_frame_ready());
    assert!(validate_ansi_terminal_recovery(capture.positions).is_ok());
    assert_eq!(capture.prefix.len(), PREFIX_LIMIT);
    assert_eq!(capture.tail.len(), TAIL_LIMIT);
    assert!(capture.total_bytes > u64::try_from(PREFIX_LIMIT + TAIL_LIMIT).unwrap());
}

#[test]
fn terminal_recovery_rejects_missing_or_out_of_order_sequences() {
    assert_eq!(
        validate_ansi_terminal_recovery(TerminalPositions::default()),
        Err("terminal setup or recovery sequence was incomplete")
    );
    assert_eq!(
        validate_ansi_terminal_recovery(TerminalPositions {
            cursor_query: None,
            alternate_enter: Some(20),
            cursor_hide: Some(10),
            alternate_leave: Some(30),
            cursor_show: Some(40),
            first_frame: Some(25),
        }),
        Err("terminal setup and recovery sequences were out of order")
    );
}

#[cfg(windows)]
#[test]
fn conpty_accepts_screen_before_cursor_hide_but_requires_later_recovery() {
    let positions = TerminalPositions {
        cursor_query: Some(1),
        alternate_enter: None,
        cursor_hide: Some(20),
        first_frame: Some(10),
        alternate_leave: None,
        cursor_show: Some(30),
    };
    let mut initial = positions;
    initial.cursor_show = None;
    assert!(initial.initial_frame_ready());
    assert!(validate_terminal_recovery(positions).is_ok());

    let premature_show = TerminalPositions {
        cursor_show: Some(15),
        ..positions
    };
    assert!(!premature_show.initial_frame_ready());
    assert!(validate_terminal_recovery(premature_show).is_err());
}

#[test]
fn state_scan_reports_nested_files_but_not_empty_directories() {
    let fixture = tempfile::tempdir().unwrap();
    let nested = fixture.path().join("one").join("two");
    fs::create_dir_all(&nested).unwrap();
    assert!(non_directory_entries(fixture.path()).unwrap().is_empty());

    let state = nested.join("state.json");
    fs::write(&state, b"{}").unwrap();
    assert_eq!(non_directory_entries(fixture.path()).unwrap(), vec![state]);
}

#[derive(Debug)]
struct StalledChild {
    kill_count: Arc<AtomicUsize>,
}

#[derive(Debug)]
struct StalledChildKiller {
    kill_count: Arc<AtomicUsize>,
}

impl ChildKiller for StalledChild {
    fn kill(&mut self) -> io::Result<()> {
        self.kill_count.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }

    fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
        Box::new(StalledChildKiller {
            kill_count: Arc::clone(&self.kill_count),
        })
    }
}

impl ChildKiller for StalledChildKiller {
    fn kill(&mut self) -> io::Result<()> {
        self.kill_count.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }

    fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
        Box::new(Self {
            kill_count: Arc::clone(&self.kill_count),
        })
    }
}

impl Child for StalledChild {
    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        Ok(None)
    }

    fn wait(&mut self) -> io::Result<ExitStatus> {
        panic!("bounded cleanup must poll try_wait instead of blocking in wait")
    }

    fn process_id(&self) -> Option<u32> {
        None
    }

    #[cfg(windows)]
    fn as_raw_handle(&self) -> Option<std::os::windows::io::RawHandle> {
        None
    }
}

#[test]
fn failed_child_cleanup_is_bounded_when_child_resists_kill() {
    let kill_count = Arc::new(AtomicUsize::new(0));
    let mut child = ManagedChild::new(Box::new(StalledChild {
        kill_count: Arc::clone(&kill_count),
    }));
    let started = Instant::now();

    assert_eq!(
        child.kill_and_reap_with_timeout(Duration::from_millis(25)),
        Err("timed-out PTY child did not exit after two bounded kill requests".to_owned())
    );
    assert!(started.elapsed() < Duration::from_secs(1));
    assert_eq!(kill_count.load(Ordering::Acquire), 2);
}
