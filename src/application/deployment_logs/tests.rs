use super::*;

#[derive(Default)]
struct CollectedEvents(Mutex<Vec<DriverLog>>);

#[derive(Default)]
struct StructuredEvents(Mutex<Vec<LogEvent>>);

impl EventSink for StructuredEvents {
    fn emit(&self, _event: DriverLog) {
        panic!("structured writer must preserve event metadata");
    }

    fn emit_record(&self, event: LogEvent) {
        self.0.lock().unwrap().push(event);
    }
}

impl EventSink for CollectedEvents {
    fn emit(&self, event: DriverLog) {
        self.0.lock().unwrap().push(event);
    }
}

fn emit(logs: &DeploymentLogSink<'_>, message: &str) {
    logs.emit(DriverLog {
        namespace: "test.output".into(),
        message: message.into(),
    });
}

#[tokio::test]
async fn event_writer_preserves_full_utf8_text_and_scope_as_complete_json_lines() {
    use crate::telemetry::log_record::LogScope;
    let directory = tempfile::tempdir().unwrap();
    let id = DeploymentId::new();
    let ui = StructuredEvents::default();
    let cancellation = CancellationToken::new();
    let logs = DeploymentLogSink::open_events(directory.path(), &id, &ui, &cancellation).unwrap();
    let message = format!("FIRST_LINE\n{}\nEND_SENTINEL", "文".repeat(25_000));
    let scope = LogScope {
        component: ComponentName::parse("api").unwrap(),
        step: "build-package".into(),
    };
    logs.emit_record(LogEvent {
        namespace: "build.stdout".into(),
        message: message.clone(),
        scope: Some(scope.clone()),
        kind: LogEventKind::Output,
    });
    assert!(logs.finish().await.is_none());
    let text = std::fs::read_to_string(directory.path().join(format!("{id}.log"))).unwrap();
    let records: Vec<_> = text
        .lines()
        .map(|line| serde_json::from_str::<LogRecord>(line).unwrap())
        .collect();
    assert!(records.len() > 1);
    assert!(
        records
            .iter()
            .all(|record| record.event.scope.as_ref() == Some(&scope))
    );
    assert_eq!(
        records
            .iter()
            .map(|record| record.event.message.as_str())
            .collect::<String>(),
        message
    );
    assert!(!cancellation.is_cancelled());
    let projected = ui.0.lock().unwrap();
    assert_eq!(projected.len(), records.len());
    assert!(
        projected
            .iter()
            .all(|event| event.scope.as_ref() == Some(&scope))
    );
    assert_eq!(
        projected
            .iter()
            .map(|event| event.message.as_str())
            .collect::<String>(),
        message,
        "the production writer must not truncate or flatten the live full-record view"
    );
}

#[tokio::test]
async fn event_writer_redacts_named_command_secrets_and_rejects_invalid_metadata() {
    use crate::telemetry::log_record::{CommandLocation, RecordedCommand, decode_log_record};
    let directory = tempfile::tempdir().unwrap();
    let id = DeploymentId::new();
    let ui = CollectedEvents::default();
    let cancellation = CancellationToken::new();
    let logs = DeploymentLogSink::open_events(directory.path(), &id, &ui, &cancellation).unwrap();
    logs.emit_record(LogEvent {
        namespace: "build.command".into(),
        message: "failed".into(),
        scope: None,
        kind: LogEventKind::FailedCommand {
            command: RecordedCommand {
                working_directory: None,
                location: CommandLocation::Local,
                index: Some(1),
                program: "tool".into(),
                args: vec!["--token".into(), "PRIVATE_SENTINEL".into()],
            },
        },
    });
    assert!(logs.finish().await.is_none());
    let text = std::fs::read_to_string(directory.path().join(format!("{id}.log"))).unwrap();
    assert!(!text.contains("PRIVATE_SENTINEL"));
    assert!(matches!(
        decode_log_record(text.trim().as_bytes(), &Redactor::default())
            .unwrap()
            .event
            .kind,
        LogEventKind::FailedCommand { .. }
    ));
    let id = DeploymentId::new();
    let logs = DeploymentLogSink::open_events(directory.path(), &id, &ui, &cancellation).unwrap();
    logs.emit_record(LogEvent {
        namespace: "\u{1b}unsafe".into(),
        message: "PRIVATE_SENTINEL".into(),
        scope: None,
        kind: LogEventKind::Output,
    });
    assert!(logs.finish().await.is_some());
    assert!(cancellation.is_cancelled());
    assert_eq!(
        std::fs::read(directory.path().join(format!("{id}.log"))).unwrap(),
        b""
    );
}

#[tokio::test]
async fn split_multiline_secrets_are_redacted_before_ui_and_disk() {
    let directory = tempfile::tempdir().unwrap();
    let id = DeploymentId::new();
    let ui = CollectedEvents::default();
    let cancellation = CancellationToken::new();
    let redactor = Redactor::new(["token-value".into(), "part-a\npart-b".into()]);
    let mut writer = RollingLogWriter::open(directory.path(), &id, LOG_MAX_BYTES, 3).unwrap();
    let logs =
        DeploymentLogSink::with_writer(&ui, &cancellation, redactor.clone(), 128, move |text| {
            writer
                .append(text, &Redactor::default())
                .map_err(|error| error.to_string())
        })
        .unwrap();
    let component = ComponentName::parse("api").unwrap();
    let output = BuildOutputProjector::with_redactor(&component, &logs, &redactor);
    for chunk in b"prefix token-value part-a\npart-b suffix\n".chunks(5) {
        output.output(0, OutputStream::Stdout, chunk);
    }
    output.output(0, OutputStream::Stdout, &[]);
    assert!(logs.finish().await.is_none());
    let text = std::fs::read_to_string(directory.path().join(format!("{id}.log"))).unwrap();
    let ui_text = format!("{:?}", ui.0.lock().unwrap());
    for sensitive in ["token-value", "part-a", "part-b"] {
        assert!(!text.contains(sensitive));
        assert!(!ui_text.contains(sensitive));
    }
    assert!(text.contains("[REDACTED]"));
    assert!(!cancellation.is_cancelled());
}

#[test]
fn text_stream_preserves_long_lines_and_detects_private_keys_after_the_ui_limit() {
    let prefix = "x".repeat(MAX_UI_CHARACTERS * 3);
    let input = format!(
        "{prefix}-----BEGIN OPENSSH PRIVATE KEY-----\nsecret-body\n-----END OPENSSH PRIVATE KEY-----\n结束"
    );
    let mut stream = OutputText::new(&Redactor::default());
    let mut output = String::new();
    for chunk in input.as_bytes().chunks(11) {
        output.push_str(&stream.push(chunk, false));
        assert!(stream.pending.len() <= 3);
    }
    output.push_str(&stream.push(&[], true));
    assert_eq!(output, format!("{prefix}[REDACTED PRIVATE KEY]\n结束"));
}

#[tokio::test]
async fn long_log_text_is_complete_on_disk_and_bounded_only_in_the_ui() {
    let directory = tempfile::tempdir().unwrap();
    let id = DeploymentId::new();
    let ui = CollectedEvents::default();
    let cancellation = CancellationToken::new();
    let logs = DeploymentLogSink::open(directory.path(), &id, &ui, &cancellation).unwrap();
    let content = "长".repeat(MAX_UI_CHARACTERS * 3);
    emit(&logs, &content);
    assert!(logs.finish().await.is_none());
    let text = std::fs::read_to_string(directory.path().join(format!("{id}.log"))).unwrap();
    assert!(text.contains(&content));
    assert_eq!(
        ui.0.lock().unwrap()[0].message.chars().count(),
        MAX_UI_CHARACTERS
    );
}

#[tokio::test]
async fn slow_disk_never_blocks_cancellation_and_queue_saturation_keeps_the_first_error() {
    let ui = CollectedEvents::default();
    let cancellation = CancellationToken::new();
    let (started, started_receiver) = tokio::sync::oneshot::channel();
    let mut started = Some(started);
    let (release, release_receiver) = mpsc::channel();
    let logs =
        DeploymentLogSink::with_writer(&ui, &cancellation, Redactor::default(), 1, move |_| {
            if let Some(started) = started.take() {
                started.send(()).unwrap();
                release_receiver
                    .recv_timeout(Duration::from_secs(2))
                    .unwrap();
            }
            Ok(())
        })
        .unwrap();
    emit(&logs, "first");
    tokio::time::timeout(Duration::from_secs(1), started_receiver)
        .await
        .unwrap()
        .unwrap();
    emit(&logs, "queued");
    emit(&logs, "overflow");
    assert!(cancellation.is_cancelled());
    let original = logs.failure().unwrap();
    assert!(original.contains("queue is full"));
    let failure = tokio::time::timeout(
        Duration::from_secs(1),
        logs.finish_with_timeout(Duration::from_millis(10)),
    )
    .await
    .unwrap();
    assert_eq!(failure, Some(original.clone()));
    emit(&logs, "cannot replace the first error");
    assert_eq!(logs.failure(), Some(original));
    release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(1), logs.state.completed())
        .await
        .unwrap();
}

#[tokio::test]
async fn write_error_cancels_and_is_not_replaced_by_later_output() {
    let ui = CollectedEvents::default();
    let cancellation = CancellationToken::new();
    let logs = DeploymentLogSink::with_writer(&ui, &cancellation, Redactor::default(), 4, |_| {
        Err("injected disk failure".into())
    })
    .unwrap();
    emit(&logs, "first");
    tokio::time::timeout(Duration::from_secs(1), logs.state.completed())
        .await
        .unwrap();
    assert!(cancellation.is_cancelled());
    emit(&logs, "later");
    assert_eq!(
        logs.finish().await.as_deref(),
        Some("injected disk failure")
    );
    assert_eq!(ui.0.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn finish_reports_stalled_writer_without_blocking_the_runtime() {
    let ui = CollectedEvents::default();
    let cancellation = CancellationToken::new();
    let (started, started_receiver) = tokio::sync::oneshot::channel();
    let mut started = Some(started);
    let (release, release_receiver) = mpsc::channel();
    let logs =
        DeploymentLogSink::with_writer(&ui, &cancellation, Redactor::default(), 4, move |_| {
            if let Some(started) = started.take() {
                started.send(()).unwrap();
            }
            release_receiver
                .recv_timeout(Duration::from_secs(2))
                .unwrap();
            Ok(())
        })
        .unwrap();
    emit(&logs, "stalled");
    started_receiver.await.unwrap();
    let error = logs
        .finish_with_timeout(Duration::from_millis(10))
        .await
        .unwrap();
    assert!(error.contains("shutdown deadline"));
    assert!(cancellation.is_cancelled());
    release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(1), logs.state.completed())
        .await
        .unwrap();
}

#[tokio::test]
async fn sanitized_disk_output_rotates_within_file_and_count_limits() {
    let directory = tempfile::tempdir().unwrap();
    let id = DeploymentId::new();
    let ui = CollectedEvents::default();
    let cancellation = CancellationToken::new();
    let mut writer = RollingLogWriter::open(directory.path(), &id, 128, 3).unwrap();
    let logs = DeploymentLogSink::with_writer(
        &ui,
        &cancellation,
        Redactor::new(["secret-token".into()]),
        32,
        move |text| {
            writer
                .append(text, &Redactor::default())
                .map_err(|error| error.to_string())
        },
    )
    .unwrap();
    for index in 0..20 {
        emit(
            &logs,
            &format!("entry-{index} secret-token {}", "x".repeat(30)),
        );
    }
    assert!(logs.finish().await.is_none());
    let paths = std::fs::read_dir(directory.path())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect::<Vec<_>>();
    assert_eq!(paths.len(), 4);
    for path in paths {
        let bytes = std::fs::read(path).unwrap();
        assert!(bytes.len() <= 128);
        assert!(!String::from_utf8_lossy(&bytes).contains("secret-token"));
    }
}

#[test]
fn utf8_chunk_boundaries_and_invalid_bytes_are_handled_before_redaction() {
    let mut stream = OutputText::new(&Redactor::new(["key-�-secret".into()]));
    let mut result = String::new();
    for byte in "开始 ".as_bytes().iter().chain(b"key-\xff-secret") {
        result.push_str(&stream.push(&[*byte], false));
    }
    result.push_str(&stream.push(&[], true));
    assert_eq!(result, "开始 [REDACTED]");
}

#[test]
fn sensitive_environment_matching_accepts_os_strings_and_multiline_values() {
    let redactor = redactor_for_environment([
        (OsString::from("APP_TOKEN"), OsString::from("one\ntwo")),
        (
            OsString::from("not_secret_path"),
            OsString::from("location"),
        ),
    ]);
    let mut stream = OutputText::new(&redactor);
    assert_eq!(stream.push(b"one\ntwo", true), "[REDACTED]");
}

#[cfg(unix)]
#[test]
fn non_unicode_environment_entries_do_not_panic() {
    use std::os::unix::ffi::OsStringExt;
    let redactor = redactor_for_environment([
        (
            OsString::from_vec(b"INVALID_\xff".to_vec()),
            OsString::from_vec(b"value\xff".to_vec()),
        ),
        (
            OsString::from("APP_TOKEN"),
            OsString::from_vec(b"key\xffsecret".to_vec()),
        ),
    ]);
    let mut stream = OutputText::new(&redactor);
    assert_eq!(stream.push(b"key\xffsecret", true), "[REDACTED]");
}
