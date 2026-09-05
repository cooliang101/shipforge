//! Whole-process TUI stress harness. Both entry points are ignored so the timed
//! parent runs only as an explicit, serial performance gate; it starts the child
//! in a fresh copy of this test binary.

use std::{
    io,
    process::{Child, Command, Output, Stdio},
    sync::{
        Arc, Barrier,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError},
    },
    thread,
    time::{Duration, Instant},
};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{Terminal, backend::TestBackend};

use crate::{
    domain::ComponentName,
    telemetry::log_record::{LogEvent, LogEventKind, LogScope},
    tui::{FrameSchedule, live_progress::LiveProgress},
};

use super::App;

const CHILD_TEST: &str = "tui::app::performance_tests::isolated_tui_stress_child";
const CHILD_PREFIX: &str = "SHIPFORGE_TUI_STRESS_CHILD ";
const CHILD_TOKEN_ENV: &str = "SHIPFORGE_TUI_STRESS_TOKEN";
const PRODUCERS: usize = 4;
const EVENTS_PER_PRODUCER: u64 = 20_000;
const INITIAL_BURST_PER_PRODUCER: u64 = 1_024;
const TOTAL_EVENTS: u64 = 80_000;
const VIEW_PRIMING_EVENTS: u64 = 600;
const VIEW_PRIMING_BATCH: u64 = 100;
const INPUT_EVENTS: u64 = 400;
const INPUT_QUEUE: usize = 4;
const CHILD_MINIMUM_RUNTIME: Duration = Duration::from_secs(2);
const CHILD_TIMEOUT: Duration = Duration::from_secs(20);
const PARENT_TIMEOUT: Duration = Duration::from_secs(45);
const MAX_LATENCY_SAMPLES: usize = 20_000;
const MIN_LATENCY_SAMPLES: u64 = 64;
const MAX_P99_MICROS: u64 = 100_000;
const MAX_INPUT_TO_FRAME_MICROS: u64 = 1_000_000;
const MAX_RSS_BYTES: u64 = 256 * 1024 * 1024;
const RSS_SAMPLE_ATTEMPTS: usize = 3;

#[derive(Debug, serde::Deserialize, serde::Serialize)]
struct StressMetrics {
    token: String,
    events: u64,
    input_events: u64,
    producer_dropped: u64,
    view_omitted: u64,
    frames: u64,
    changed_polls: u64,
    samples: u64,
    p99_micros: u64,
    max_micros: u64,
    process_rss_bytes: u64,
    checksum: u64,
}

#[test]
#[ignore = "explicit isolated performance gate; run this test alone with --ignored --exact --test-threads=1"]
fn isolated_tui_stress_stays_responsive_and_memory_bounded() {
    let executable = std::env::current_exe().expect("current test executable");
    let token = uuid::Uuid::now_v7().to_string();
    let child = Command::new(executable)
        .arg(CHILD_TEST)
        .args(["--exact", "--ignored", "--nocapture", "--test-threads=1"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env(CHILD_TOKEN_ENV, &token)
        .spawn()
        .expect("spawn isolated TUI stress child");
    let output = wait_for_child(child);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "isolated stress child failed\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    let metrics = parse_metrics(&stdout);
    assert_eq!(
        metrics.token, token,
        "stress child token did not round-trip"
    );
    assert_eq!(metrics.events, TOTAL_EVENTS);
    assert_eq!(metrics.input_events, INPUT_EVENTS);
    assert_eq!(metrics.samples, metrics.input_events);
    assert!(
        metrics.producer_dropped > 0,
        "producer drops were not reported"
    );
    assert!(metrics.view_omitted > 0, "view eviction was not reported");
    assert!(metrics.frames >= MIN_LATENCY_SAMPLES);
    assert!(metrics.changed_polls > 0);
    assert!(metrics.checksum > 0, "the rendered frame was empty");
    assert!(
        metrics.p99_micros <= MAX_P99_MICROS,
        "p99 input-to-frame latency was {} us",
        metrics.p99_micros
    );
    assert!(
        metrics.max_micros <= MAX_INPUT_TO_FRAME_MICROS,
        "maximum input-to-frame latency was {} us",
        metrics.max_micros
    );
    assert!(
        metrics.process_rss_bytes > 0,
        "child-reported whole-process RSS must be nonzero"
    );
    assert!(
        metrics.process_rss_bytes <= MAX_RSS_BYTES,
        "child-reported whole-process RSS was {} bytes",
        metrics.process_rss_bytes
    );
    println!(
        "SHIPFORGE_TUI_STRESS_RESULT events={} input_events={} producer_dropped={} view_omitted={} frames={} p99_micros={} max_micros={} process_rss_bytes={}",
        metrics.events,
        metrics.input_events,
        metrics.producer_dropped,
        metrics.view_omitted,
        metrics.frames,
        metrics.p99_micros,
        metrics.max_micros,
        metrics.process_rss_bytes
    );
}

#[test]
#[ignore = "started by the isolated parent test"]
fn isolated_tui_stress_child() {
    let directory = tempfile::tempdir().expect("temporary stress directory");
    let mut app = App::new(
        directory.path().join("projects.yaml"),
        directory.path().join("destinations.yaml"),
        directory.path(),
    )
    .expect("create stress App");
    settle_initial_attention(&mut app);
    let progress = LiveProgress::default();
    app.live_progress = Some(progress.clone());
    app.open_live_logs();
    prime_live_log_view(&mut app, &progress);
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("test terminal");

    let produced = Arc::new(AtomicU64::new(0));
    let start_gate = Arc::new(Barrier::new(PRODUCERS + 1));
    let producers = spawn_producers(&progress, &produced, &start_gate);
    start_gate.wait();
    let (input_receiver, input_producer) = spawn_input_producer();
    let mut exercise = exercise_app(&mut app, &mut terminal, produced.as_ref(), &input_receiver);

    input_producer.join().expect("synthetic input producer");
    for producer in producers {
        producer.join().expect("synthetic log producer");
    }
    progress.finish();
    app.poll_background();
    terminal
        .draw(|frame| crate::tui::render(frame, &app))
        .expect("render final stress frame");

    let snapshot = progress.snapshot();
    let (p99_micros, max_micros) = latency_summary(&mut exercise.latencies);
    // Sample only after the latency workload so platform-specific helper
    // processes cannot contend with the timed input-to-draw path.
    let process_rss_bytes = measure_process_rss().expect("measure stress child RSS");
    let metrics = StressMetrics {
        token: std::env::var(CHILD_TOKEN_ENV).unwrap_or_else(|_| "direct".into()),
        events: produced.load(Ordering::Relaxed),
        input_events: exercise.input_events,
        producer_dropped: snapshot.dropped_rows,
        view_omitted: app.live_logs.omitted(),
        frames: exercise.frames,
        changed_polls: exercise.changed_polls,
        samples: u64::try_from(exercise.latencies.len()).unwrap_or(u64::MAX),
        p99_micros,
        max_micros,
        process_rss_bytes,
        checksum: frame_checksum(terminal.backend()),
    };
    assert_eq!(metrics.events, TOTAL_EVENTS);
    assert_eq!(metrics.input_events, INPUT_EVENTS);
    assert_eq!(metrics.samples, metrics.input_events);
    assert!(metrics.producer_dropped > 0);
    assert!(metrics.view_omitted > 0);
    assert!(metrics.samples >= MIN_LATENCY_SAMPLES);
    app.shutdown();
    println!(
        "{CHILD_PREFIX}{}",
        serde_json::to_string(&metrics).expect("encode fixed stress metrics")
    );
}

fn prime_live_log_view(app: &mut App, progress: &LiveProgress) {
    let scope = LogScope {
        component: ComponentName::parse("api").expect("component"),
        step: "view-priming".into(),
    };
    let payload = "p".repeat(192);
    for sequence in 0..VIEW_PRIMING_EVENTS {
        progress.record(LogEvent {
            namespace: "stress.prime".into(),
            message: format!("view priming event {sequence} {payload}"),
            scope: Some(scope.clone()),
            kind: LogEventKind::Output,
        });
        if (sequence + 1).is_multiple_of(VIEW_PRIMING_BATCH) {
            assert!(
                app.poll_background().changed(),
                "each priming batch must reach the live LogView"
            );
        }
    }
    assert!(
        app.live_logs.omitted() > 0,
        "priming must deterministically exercise LogView eviction"
    );
}

#[derive(Debug)]
struct StressExercise {
    latencies: Vec<u64>,
    frames: u64,
    changed_polls: u64,
    input_events: u64,
}

fn exercise_app(
    app: &mut App,
    terminal: &mut Terminal<TestBackend>,
    produced: &AtomicU64,
    input: &Receiver<(Instant, KeyEvent)>,
) -> StressExercise {
    let started = Instant::now();
    let mut schedule = FrameSchedule::default();
    let mut latencies = Vec::with_capacity(MAX_LATENCY_SAMPLES);
    let mut frames = 0_u64;
    let mut changed_polls = 0_u64;
    let mut input_events = 0_u64;
    let mut pending_input = None;
    let mut input_closed = false;
    loop {
        assert!(started.elapsed() < CHILD_TIMEOUT, "stress child timed out");
        let background = app.poll_background();
        changed_polls += u64::from(background.changed());
        if background.requires_frame_boundary() {
            schedule.mark_interaction();
        } else if background.changed() {
            schedule.mark_dirty();
        }
        let now = Instant::now();
        if schedule.draw_due(now, app.needs_periodic_redraw()) {
            terminal
                .draw(|frame| crate::tui::render(frame, app))
                .expect("render stress frame");
            let completed = Instant::now();
            schedule.record_draw(completed);
            frames = frames.saturating_add(1);
            if let Some(enqueued) = pending_input.take()
                && latencies.len() < MAX_LATENCY_SAMPLES
            {
                latencies.push(micros(completed.saturating_duration_since(enqueued)));
            }
        }
        if produced.load(Ordering::Relaxed) == TOTAL_EVENTS
            && input_closed
            && pending_input.is_none()
            && started.elapsed() >= CHILD_MINIMUM_RUNTIME
        {
            break;
        }
        let timeout = schedule.wait_timeout(Instant::now(), app.needs_periodic_redraw());
        if input_closed {
            if timeout.is_zero() {
                thread::yield_now();
            } else {
                thread::sleep(timeout);
            }
            continue;
        }
        match input.recv_timeout(timeout) {
            Ok((enqueued, key)) => {
                assert!(!app.handle_key(key), "stress navigation requested exit");
                pending_input = Some(enqueued);
                input_events = input_events.saturating_add(1);
                schedule.mark_interaction();
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => input_closed = true,
        }
    }
    StressExercise {
        latencies,
        frames,
        changed_polls,
        input_events,
    }
}

fn settle_initial_attention(app: &mut App) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while app.attention_busy() && Instant::now() < deadline {
        app.poll_background();
        thread::sleep(Duration::from_millis(1));
    }
    assert!(
        !app.attention_busy(),
        "initial local-only attention read did not settle"
    );
}

fn spawn_input_producer() -> (Receiver<(Instant, KeyEvent)>, thread::JoinHandle<()>) {
    let (sender, receiver) = mpsc::sync_channel(INPUT_QUEUE);
    let worker = thread::Builder::new()
        .name("shipforge-stress-input".into())
        .spawn(move || {
            for index in 0..INPUT_EVENTS {
                let enqueued = Instant::now();
                sender
                    .send((enqueued, stress_key(index)))
                    .expect("stress input receiver");
                thread::yield_now();
            }
        })
        .expect("spawn synthetic input producer");
    (receiver, worker)
}

fn spawn_producers(
    progress: &LiveProgress,
    produced: &Arc<AtomicU64>,
    start_gate: &Arc<Barrier>,
) -> Vec<thread::JoinHandle<()>> {
    (0..PRODUCERS)
        .map(|producer| {
            let progress = progress.clone();
            let produced = Arc::clone(produced);
            let start_gate = Arc::clone(start_gate);
            thread::Builder::new()
                .name(format!("shipforge-stress-log-{producer}"))
                .spawn(move || {
                    let scope = LogScope {
                        component: ComponentName::parse("api").expect("component"),
                        step: format!("synthetic-{producer}"),
                    };
                    let payload = "x".repeat(192);
                    for sequence in 0..EVENTS_PER_PRODUCER {
                        progress.record(LogEvent {
                            namespace: "stress.stdout".into(),
                            message: format!(
                                "synthetic producer {producer} event {sequence} {payload}"
                            ),
                            scope: Some(scope.clone()),
                            kind: LogEventKind::Output,
                        });
                        produced.fetch_add(1, Ordering::Relaxed);
                        if sequence + 1 == INITIAL_BURST_PER_PRODUCER {
                            start_gate.wait();
                        }
                    }
                })
                .expect("spawn synthetic log producer")
        })
        .collect()
}

fn stress_key(frame: u64) -> KeyEvent {
    let code = match frame % 7 {
        0 => KeyCode::Down,
        1 => KeyCode::Up,
        2 => KeyCode::PageDown,
        3 => KeyCode::PageUp,
        4 => KeyCode::Home,
        5 => KeyCode::End,
        _ => KeyCode::Char(' '),
    };
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn latency_summary(samples: &mut [u64]) -> (u64, u64) {
    assert!(!samples.is_empty(), "no input-to-frame samples");
    samples.sort_unstable();
    let rank = samples
        .len()
        .saturating_mul(99)
        .div_ceil(100)
        .saturating_sub(1);
    (samples[rank], samples.last().copied().unwrap_or(u64::MAX))
}

fn micros(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

fn frame_checksum(backend: &TestBackend) -> u64 {
    backend
        .buffer()
        .content()
        .iter()
        .flat_map(|cell| cell.symbol().bytes())
        .fold(0_u64, |sum, byte| sum.wrapping_add(u64::from(byte)))
}

fn parse_metrics(stdout: &str) -> StressMetrics {
    let payload = stdout
        .lines()
        .find_map(|line| {
            line.find(CHILD_PREFIX)
                .map(|offset| &line[offset + CHILD_PREFIX.len()..])
        })
        .expect("child did not print fixed stress metrics");
    serde_json::from_str(payload).expect("child stress metrics were malformed")
}

fn wait_for_child(mut child: Child) -> Output {
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {}
            Err(error) => stop_child(child, &format!("could not observe stress child: {error}")),
        }
        if started.elapsed() >= PARENT_TIMEOUT {
            stop_child(
                child,
                &format!("isolated stress child exceeded {PARENT_TIMEOUT:?}"),
            );
        }
        thread::sleep(Duration::from_millis(20));
    }
    child
        .wait_with_output()
        .expect("collect stress child output")
}

fn stop_child(mut child: Child, reason: &str) -> ! {
    let kill = child.kill().err();
    match child.wait_with_output() {
        Ok(output) => panic!(
            "{reason}; kill error: {kill:?}\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ),
        Err(error) => panic!("{reason}; kill error: {kill:?}; child reap failed: {error}"),
    }
}

#[cfg(any(target_os = "macos", windows))]
fn bounded_command_output(command: &mut Command) -> io::Result<Output> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return child.wait_with_output(),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(5)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "RSS sampling command timed out",
                ));
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        }
    }
}

fn measure_process_rss() -> io::Result<u64> {
    let pid = std::process::id();
    let mut last_error = None;
    for attempt in 0..RSS_SAMPLE_ATTEMPTS {
        match sample_process_rss(pid) {
            Ok(0) => {
                last_error = Some(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "RSS sampling returned zero bytes",
                ));
            }
            Ok(bytes) => return Ok(bytes),
            Err(error) => last_error = Some(error),
        }
        if attempt + 1 < RSS_SAMPLE_ATTEMPTS {
            thread::sleep(Duration::from_millis(20));
        }
    }
    Err(last_error.unwrap_or_else(|| io::Error::other("RSS sampling did not run")))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn rss_bytes(kibibytes: u64) -> io::Result<u64> {
    kibibytes
        .checked_mul(1024)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "RSS value overflowed"))
}

#[cfg(target_os = "linux")]
fn sample_process_rss(pid: u32) -> io::Result<u64> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status"))?;
    let line = status
        .lines()
        .find(|line| line.starts_with("VmHWM:"))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "VmHWM was unavailable"))?;
    let mut fields = line.split_ascii_whitespace();
    if fields.next() != Some("VmHWM:") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "VmHWM label was invalid",
        ));
    }
    let kibibytes = fields
        .next()
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "VmHWM was invalid"))?;
    if fields.next() != Some("kB") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "VmHWM unit was invalid",
        ));
    }
    rss_bytes(kibibytes)
}

#[cfg(target_os = "macos")]
fn sample_process_rss(pid: u32) -> io::Result<u64> {
    let mut command = Command::new("/bin/ps");
    command
        .env("LC_ALL", "C")
        .args(["-o", "rss=", "-p", &pid.to_string()]);
    let output = bounded_command_output(&mut command)?;
    if !output.status.success() {
        return Err(io::Error::other("RSS sampling command failed"));
    }
    let kibibytes = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<u64>()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "RSS output was invalid"))?;
    rss_bytes(kibibytes)
}

#[cfg(windows)]
fn sample_process_rss(pid: u32) -> io::Result<u64> {
    // `tasklist` can require process-enumeration rights that a restricted test
    // runner does not have. Query only the known child PID and use its peak
    // working set because process startup makes sampling deliberately sparse.
    let script = format!(
        "$p=Get-Process -Id {pid} -ErrorAction Stop; [Console]::Out.Write($p.PeakWorkingSet64.ToString([Globalization.CultureInfo]::InvariantCulture))"
    );
    let mut command = Command::new("powershell.exe");
    command.args([
        "-NoLogo",
        "-NoProfile",
        "-NonInteractive",
        "-Command",
        &script,
    ]);
    let output = bounded_command_output(&mut command)?;
    if !output.status.success() {
        return Err(io::Error::other("RSS sampling command failed"));
    }
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<u64>()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "RSS output was invalid"))
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn sample_process_rss(_pid: u32) -> io::Result<u64> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "RSS sampling is implemented for Windows, Linux, and macOS",
    ))
}
